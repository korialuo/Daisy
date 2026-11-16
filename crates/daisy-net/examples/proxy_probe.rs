//! 经真实代理拨号并读对端 banner 的探测工具。
//!
//! 用于 S7 的验收：验证 Daisy 自己的 SOCKS5 / HTTP CONNECT 客户端
//! 能连上真实第三方代理并建立隧道。用法：
//!
//! ```text
//! DAISY_PROXY=socks5://127.0.0.1:10800 \
//! DAISY_TARGET=192.168.10.21:22 \
//! cargo run -p daisy-net --example proxy_probe
//! ```
//!
//! 可选 `DAISY_PROXY_USER` / `DAISY_PROXY_PASSWORD` 提供代理凭据。
//! 连上后读一段对端数据（SSH banner），退出码 0 表示隧道建立成功。

use std::env;
use std::time::Duration;

use daisy_core::model::ProxyKind;
use daisy_net::{Dialer, ProxyAuth, ProxyDialer, ProxyEndpoint, TargetAddr};
use tokio::io::AsyncReadExt;

fn env_required(key: &str) -> String {
	env::var(key).unwrap_or_else(|_| panic!("缺少环境变量 {key}"))
}

#[tokio::main]
async fn main() {
	let proxy_raw = env_required("DAISY_PROXY");
	let target_raw = env_required("DAISY_TARGET");

	// 解析代理地址 socks5://host:port 或 http://host:port
	let (kind, rest) = if let Some(rest) = proxy_raw.strip_prefix("socks5://") {
		(ProxyKind::Socks5, rest)
	} else if let Some(rest) = proxy_raw.strip_prefix("http://") {
		(ProxyKind::Http, rest)
	} else {
		panic!("DAISY_PROXY 需以 socks5:// 或 http:// 开头，收到：{proxy_raw}");
	};

	let (host, port) = if let Some(rest) = rest.strip_prefix('[') {
		let (h, tail) = rest.split_once(']').expect("IPv6 代理缺少右括号");
		(
			h,
			tail.trim_start_matches(':')
				.parse::<u16>()
				.expect("代理端口非法"),
		)
	} else {
		let (h, p) = rest.rsplit_once(':').expect("代理地址需含端口 host:port");
		(h, p.parse::<u16>().expect("代理端口非法"))
	};

	let mut endpoint = ProxyEndpoint::new(kind, host, port).expect("代理地址非法");
	if let (Ok(user), Ok(pass)) = (
		env::var("DAISY_PROXY_USER"),
		env::var("DAISY_PROXY_PASSWORD"),
	) {
		endpoint = endpoint.with_auth(ProxyAuth::new(user, pass));
	}

	let dialer = ProxyDialer::new(
		endpoint,
		Box::new(daisy_net::DirectDialer::new(Duration::from_secs(8))),
	);

	let target = TargetAddr::parse(&target_raw).expect("目标地址非法");
	println!("{}，拨号到 {} ...", dialer.describe(), target.display());

	let mut stream = dialer.dial(&target).await.expect("拨号失败");

	// 读一段对端数据，证明隧道真的通到目标了
	let mut banner = [0u8; 64];
	let n = tokio::time::timeout(Duration::from_secs(8), stream.read(&mut banner))
		.await
		.expect("读 banner 超时")
		.expect("读 banner 失败");

	let preview = String::from_utf8_lossy(&banner[..n]).into_owned();
	println!(
		"隧道已建立，对端 banner（{} 字节）：{}",
		n,
		preview.trim_end()
	);

	if n == 0 {
		eprintln!("对端无数据返回，隧道可能未真正建立");
		std::process::exit(1);
	}
}
