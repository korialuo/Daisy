//! 诊断用：列出库里记录过的主机（**不打印密码**）。
//!
//! 用法：
//!
//! ```sh
//! cargo run -p daisy-app --example list_hosts
//! ```
//!
//! 用来确认真机验证该连哪台机器，以及 `rdp_certs` 里是否已有记录。

use daisy_core::config::AppPaths;
use daisy_core::key::EmbeddedKey;
use daisy_store::Database;

fn main() {
	let paths = AppPaths::discover().expect("定位应用目录失败");
	println!("数据目录：{}", paths.base().display());

	let db = Database::open(&paths, &EmbeddedKey).expect("打开加密库失败");
	let hosts = db.hosts().expect("读取主机失败");
	println!("主机 {} 台：", hosts.len());
	for host in &hosts {
		let has_password = db
			.host_password(host.id.unwrap_or_default())
			.ok()
			.flatten()
			.is_some();
		println!(
			"  [{:?}] {} → {}:{} 用户 {} 密码{}",
			host.protocol,
			host.name,
			host.host,
			host.port,
			host.username.as_deref().unwrap_or("-"),
			if has_password { "已存" } else { "无" }
		);
	}

	for host in hosts
		.iter()
		.filter(|h| h.protocol == daisy_core::model::Protocol::Rdp)
	{
		let stored = db
			.rdp_cert_fingerprint(&host.host, host.port)
			.unwrap_or(None);
		println!(
			"RDP 证书 {}:{} → {}",
			host.host,
			host.port,
			stored
				.map(|fp| daisy_rdp::format_fingerprint(&fp))
				.unwrap_or_else(|| "（未记录）".to_string())
		);
	}
}
