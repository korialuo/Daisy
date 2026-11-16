//! SFTP 链路探针：对**真实**主机跑一遍列表 / 下载 / 上传 / 复制 / 删除。
//!
//! ```sh
//! cargo run -p daisy-app --example sftp_probe -- LinuxServer1
//! ```
//!
//! 传主机名（库里的名字）或 `地址:端口`。凭据从加密库读——命令行里不放密码。
//!
//! 验证的是 `daisy-ssh::SftpClient` 这条真实链路（协议编解码 + 真服务端行为），
//! 不是某个内部函数。UI 只是它的调用方。

use std::path::PathBuf;

use daisy_core::config::AppPaths;
use daisy_core::key::EmbeddedKey;
use daisy_core::model::Protocol;
use daisy_ssh::sftp::{SftpClient, join_path, parent_of};
use daisy_ssh::{AuthSpec, HostKeyPolicy, SshParams, SshTarget, TrustedHostKey, connect};
use daisy_store::Database;

#[tokio::main]
async fn main() {
	// 两种来源：
	// ① 直接给 `user:pass@host:port`（容器 / 临时环境，不进库）
	// ② 给库里存的主机名，凭据从加密库读（真机验收用这个）
	let (host, port, user, password, policy) = match std::env::var("DAISY_SFTP_URL") {
		Ok(url) => parse_url(&url),
		Err(_) => from_store(),
	};

	println!("连接 {user}@{host}:{port} …");

	let params = SshParams::new(
		SshTarget::new(&host, port, &user),
		AuthSpec::Password { password },
	)
	.with_host_key_policy(policy);

	let session = connect(params).await.expect("SSH 连接失败");
	println!("SSH 已连接，主机密钥 {:?}", session.host_key_verdict());

	let mut client = SftpClient::open(&session).await.expect("打开 SFTP 失败");

	// ---- 1. 家目录 ----
	let home = client.realpath("").await.expect("realpath 失败");
	println!("\n[1] 家目录：{home}");

	// ---- 2. 列目录 ----
	let entries = client.list_dir(&home).await.expect("列目录失败");
	println!("[2] 共 {} 项：", entries.len());
	for entry in entries.iter().take(12) {
		println!(
			"    {} {:>10} {}",
			entry.mode_text(),
			if entry.is_dir {
				"-".to_string()
			} else {
				entry.size.to_string()
			},
			entry.name
		);
	}
	if entries.len() > 12 {
		println!("    …（其余 {} 项省略）", entries.len() - 12);
	}

	// ---- 3. 上传 ----
	// 容器里的用户常被 chroot 到家目录、只有某个子目录可写：
	// 优先在自己的目录里建测试文件夹
	let base = if entries.iter().any(|e| e.name == "upload" && e.is_dir) {
		join_path(&home, "upload")
	} else {
		home.clone()
	};
	let remote_dir = join_path(&base, ".daisy-sftp-probe");
	let _ = client.remove_dir(&remote_dir).await;
	client.mkdir(&remote_dir).await.expect("mkdir 失败");
	println!("\n[3] 已建测试目录 {remote_dir}");

	let local = std::env::temp_dir().join("daisy-sftp-probe.txt");
	std::fs::write(&local, "daisy sftp probe\n第二行：中文与符号 <>[]{}\n")
		.expect("写本地文件失败");
	let remote_file = format!("{remote_dir}/probe.txt");
	let uploaded = client.upload(&local, &remote_file).await.expect("上传失败");
	println!("    上传 {uploaded} 字节 → {remote_file}");

	// ---- 4. 下载 ----
	let back: PathBuf = std::env::temp_dir().join("daisy-sftp-probe-back.txt");
	let downloaded = client
		.download(&remote_file, &back)
		.await
		.expect("下载失败");
	let content = std::fs::read_to_string(&back).expect("读回下载文件失败");
	println!(
		"[4] 下载 {downloaded} 字节，内容一致：{}",
		content == std::fs::read_to_string(&local).expect("读原文件失败")
	);

	// ---- 5. 远端复制 ----
	let copy = format!("{remote_dir}/copy.txt");
	let copied = client
		.copy_file(&remote_file, &copy)
		.await
		.expect("复制失败");
	println!("[5] 远端复制 {copied} 字节 → {copy}");

	// ---- 6. 重命名 ----
	let renamed = format!("{remote_dir}/renamed.txt");
	client.rename(&copy, &renamed).await.expect("重命名失败");
	println!("[6] 重命名 {copy} → {renamed}");

	// ---- 7. 清理 ----
	client.remove_file(&renamed).await.expect("删除文件失败");
	client
		.remove_file(&remote_file)
		.await
		.expect("删除文件失败");
	client.remove_dir(&remote_dir).await.expect("删除目录失败");
	println!("[7] 已清理 {remote_dir}");

	// ---- 8. 路径工具 ----
	println!(
		"\n[8] parent_of(\"{remote_dir}\") = {:?}",
		parent_of(&remote_dir)
	);

	println!("\n全部通过");
}

/// 解析 `user:pass@host:port`。
fn parse_url(
	url: &str,
) -> (
	String,
	u16,
	String,
	zeroize::Zeroizing<String>,
	HostKeyPolicy,
) {
	let (credentials, address) = url.rsplit_once('@').expect("格式应为 user:pass@host:port");
	let (user, password) = credentials.split_once(':').expect("缺少密码");
	let (host, port) = match address.rsplit_once(':') {
		Some((host, port)) => (host, port.parse().unwrap_or(22)),
		None => (address, 22),
	};
	(
		host.to_string(),
		port,
		user.to_string(),
		zeroize::Zeroizing::new(password.to_string()),
		// 临时环境：没有记录可比，接受并继续
		HostKeyPolicy::first_connection(),
	)
}

/// 从加密库读一台主机（名字或 `地址:端口` 都能定位）。
fn from_store() -> (
	String,
	u16,
	String,
	zeroize::Zeroizing<String>,
	HostKeyPolicy,
) {
	let target = std::env::args().nth(1).unwrap_or_default();
	if target.is_empty() {
		eprintln!("用法：sftp_probe <主机名 | host:port>  或  DAISY_SFTP_URL=user:pass@host:port");
		std::process::exit(2);
	}

	let paths = AppPaths::discover().expect("定位应用目录失败");
	let db = Database::open(&paths, &EmbeddedKey).expect("打开加密库失败");
	let hosts = db.hosts().expect("读取主机失败");

	let host = hosts
		.iter()
		.find(|h| h.name == target)
		.or_else(|| {
			let (addr, port) = match target.rsplit_once(':') {
				Some((addr, port)) => (addr, port.parse().unwrap_or(22)),
				None => (target.as_str(), 22),
			};
			hosts.iter().find(|h| h.host == addr && h.port == port)
		})
		.expect("库里没有这台主机");

	assert_eq!(host.protocol, Protocol::Ssh, "SFTP 只适用于 SSH 主机");

	let id = host.id.expect("主机应有主键");
	let password = db
		.host_password(id)
		.expect("读密码失败")
		.expect("这台主机没有保存密码，探针需要密码认证");
	let user = host.username.clone().expect("主机没有用户名");

	let known = daisy_store::known_hosts::find(
		db.connection(),
		&daisy_store::known_hosts::host_key(&host.host, host.port),
	)
	.expect("读 known_hosts 失败");
	let policy = match known {
		Some(known) => HostKeyPolicy::require_match(TrustedHostKey {
			key_type: String::new(),
			fingerprint: known.fingerprint,
		}),
		None => HostKeyPolicy::first_connection(),
	};

	(host.host.clone(), host.port, user, password, policy)
}
