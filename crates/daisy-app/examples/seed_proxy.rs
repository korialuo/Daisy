//! 向加密库灌一个代理，供无人值守验证使用。
//!
//! 与 `seed_host` 同思路：界面没法脚本点击，就把「新建代理」的前半段做实，
//! 连接侧由应用的 `DAISY_SMOKE_AUTOCONNECT` 钩子完成。
//!
//! ```bash
//! DAISY_DATA_DIR=/tmp/daisy-demo \
//!   cargo run -p daisy-app --example seed_proxy -- "本地SOCKS5" socks5 127.0.0.1 10800
//! ```

use daisy_core::config::AppPaths;
use daisy_core::key::EmbeddedKey;
use daisy_core::model::{Proxy, ProxyKind};
use daisy_store::Database;

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut args = std::env::args().skip(1);
	let name = args
		.next()
		.ok_or("用法：seed_proxy <名称> <socks5|http> <地址> <端口>")?;
	let kind_raw = args.next().ok_or("缺少参数：类型")?;
	let host = args.next().ok_or("缺少参数：地址")?;
	let port: u16 = args
		.next()
		.ok_or("缺少参数：端口")?
		.parse()
		.map_err(|error| format!("端口不是数字：{error}"))?;

	let kind = match kind_raw.as_str() {
		"socks5" => ProxyKind::Socks5,
		"http" => ProxyKind::Http,
		other => return Err(format!("未知代理类型 {other}（应为 socks5 或 http）").into()),
	};

	let paths = match std::env::var_os("DAISY_DATA_DIR") {
		Some(base) if !base.is_empty() => AppPaths::from_base(base),
		_ => AppPaths::discover()?,
	};

	let db = Database::open(&paths, &EmbeddedKey)?;

	let proxy = Proxy {
		id: None,
		name: name.clone(),
		kind,
		host,
		port,
		username: None,
	};

	let id = db.save_proxy(&proxy, None)?;
	println!(
		"已保存代理 id={id}：{name}（{kind} {}:{}）",
		proxy.host, proxy.port
	);
	Ok(())
}
