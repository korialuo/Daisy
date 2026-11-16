//! 诊断用：在真实主机上跑一遍 RDP 证书指纹的三种判定（S8-65）。
//!
//! ```sh
//! cargo run -p daisy-app --example rdp_cert_probe            # 库里第一台 RDP 主机
//! cargo run -p daisy-app --example rdp_cert_probe -- 主机名
//! ```
//!
//! 依次验证：
//!
//! 1. **首连**（库里没有记录）→ 接受，回报 `FirstSight` + 指纹
//! 2. **指纹一致** → 接受，`Match`
//! 3. **指纹不符**（人为给一个假值）→ 拒绝，错误是 `Certificate`
//!
//! 每一步都会真的连上 Windows（占一个桌面会话），结束后立即断开。

use daisy_core::config::AppPaths;
use daisy_core::key::EmbeddedKey;
use daisy_rdp::{ConnectionParams, RdpClient, RdpError};
use daisy_store::Database;

/// 与正式连接保持一致的最小设置。
fn params_of(host: &daisy_core::model::Host, password: String) -> ConnectionParams {
	ConnectionParams::new(
		host.host.clone(),
		host.username.clone().unwrap_or_default(),
		password,
	)
	.with_port(host.port)
	.with_resolution(1024, 768)
}

/// 连一次，返回观察结果或错误。
fn try_connect(
	params: &ConnectionParams,
	expected: Option<&str>,
) -> Result<Option<daisy_rdp::Observation>, RdpError> {
	let mut client = RdpClient::new()?;
	client.trust_certificate(expected);
	client.enable_clipboard();
	let outcome = client.connect(params);
	let observation = client.certificate_observation();
	match outcome {
		Ok(()) => Ok(observation),
		Err(error) => {
			// 失败时证书观察仍然是诊断信息，一并打印
			if let Some(seen) = &observation {
				println!("      （失败时的证书观察：{:?}）", seen.verdict);
			}
			Err(error)
		},
	}
}

fn main() {
	let paths = AppPaths::discover().expect("定位应用目录失败");
	let db = Database::open(&paths, &EmbeddedKey).expect("打开加密库失败");

	let wanted = std::env::args().nth(1);
	let host = db
		.hosts()
		.expect("读取主机失败")
		.into_iter()
		.filter(|host| host.protocol == daisy_core::model::Protocol::Rdp)
		.find(|host| wanted.as_deref().is_none_or(|name| host.name == name))
		.expect("库里没有匹配的 RDP 主机");

	let password = db
		.host_password(host.id.expect("主机缺少主键"))
		.expect("读取密码失败")
		.expect("这台主机没有存密码")
		.to_string();

	println!("目标：{} → {}:{}", host.name, host.host, host.port);
	let params = params_of(&host, password);

	println!("\n[1/3] 首连（库里没有记录）——应接受并回报 FirstSight");
	match try_connect(&params, None) {
		Ok(Some(seen)) => {
			let shown = daisy_rdp::format_fingerprint(&seen.fingerprint);
			println!("      判定：{:?}", seen.verdict);
			println!("      指纹：{shown}");
			println!("      主体：{}", seen.subject.as_deref().unwrap_or("-"));

			println!("\n[2/3] 指纹一致——应接受并判为 Match");
			match try_connect(&params, Some(&seen.fingerprint)) {
				Ok(Some(again)) => println!("      判定：{:?}", again.verdict),
				Ok(None) => {
					println!("      ⚠️ 没有证书回调（FreeRDP 自己的 known_hosts 里有记录？）")
				},
				Err(error) => println!("      ❌ 意外失败：{error}"),
			}
		},
		Ok(None) => {
			println!("      ⚠️ 没走到证书回调——检查 FreeRDP 自己的 known_hosts 是否已有这台机器");
		},
		Err(error) => println!("      ❌ 首连失败：{error}"),
	}

	println!("\n[3/3] 指纹不符（人为给假值）——必须拒绝");
	match try_connect(&params, Some("deadbeefdeadbeef")) {
		Ok(_) => println!("      ❌ 被放行了！指纹校验没有生效"),
		Err(RdpError::Certificate { reason }) => println!("      ✅ 已拒绝：{reason}"),
		Err(other) => println!("      ❌ 拒绝了，但错误类型不对：{other}"),
	}
}
