//! 生成一个加密数据库，供人工验收与排查使用。
//!
//! 用法：
//!
//! ```text
//! cargo run -p daisy-store --example make_db -- /tmp/daisy-accept/daisy.db
//! ```
//!
//! 生成后可以用系统 sqlite3 试着打开——**应该打不开**，这正说明全库加密生效了：
//!
//! ```text
//! sqlite3 /tmp/daisy-accept/daisy.db ".tables"
//! # → Error: file is not a database
//! ```

use std::path::PathBuf;

use daisy_core::key::{EmbeddedKey, KeyProvider};
use daisy_core::model::{Host, Protocol};
use daisy_store::{Database, HostSecrets};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let path: PathBuf = std::env::args()
		.nth(1)
		.unwrap_or_else(|| "/tmp/daisy-accept/daisy.db".to_string())
		.into();

	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent)?;
	}

	let provider = EmbeddedKey;
	println!("密钥来源：{}", provider.describe());
	println!("库文件：{}", path.display());

	let db = Database::open_file(&path, &provider)?;

	let mut ssh = Host::draft(Protocol::Ssh);
	ssh.name = "验收 · SSH".to_string();
	ssh.host = "10.0.0.10".to_string();
	ssh.username = Some("root".to_string());
	let ssh_id = db.save_host(&ssh, &HostSecrets::with_password("ssh-password-4444"))?;

	let mut rdp = Host::draft(Protocol::Rdp);
	rdp.name = "验收 · RDP".to_string();
	rdp.host = "192.168.10.20".to_string();
	rdp.username = Some("Administrator".to_string());
	let rdp_id = db.save_host(&rdp, &HostSecrets::with_password("rdp-password-5555"))?;

	println!("\n已写入 {} 台主机：", db.hosts()?.len());
	for host in db.hosts()? {
		println!(
			"  [{}] {} {}:{}",
			host.id.unwrap_or_default(),
			host.name,
			host.host,
			host.port
		);
	}

	// 读回验证解密链路
	let ssh_password = db.host_password(ssh_id)?.unwrap_or_default();
	let rdp_password = db.host_password(rdp_id)?.unwrap_or_default();
	println!("\n解密校验：");
	println!("  SSH 密码长度 {} 字节", ssh_password.as_str().len());
	println!("  RDP 密码长度 {} 字节", rdp_password.as_str().len());

	// 落盘内容检查：明文不应出现在文件里
	let bytes = std::fs::read(&path)?;
	let mut leaked = Vec::new();
	for marker in [
		"ssh-password-4444",
		"rdp-password-5555",
		"验收",
		"SQLite format 3",
	] {
		if bytes
			.windows(marker.len())
			.any(|window| window == marker.as_bytes())
		{
			leaked.push(marker);
		}
	}

	if leaked.is_empty() {
		println!("\n✓ 文件内未出现任何明文标记，全库加密生效");
	} else {
		eprintln!("\n✗ 文件内出现明文：{leaked:?}");
		std::process::exit(1);
	}

	Ok(())
}
