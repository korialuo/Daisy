//! 向加密库灌一台主机，供无人值守验证使用。
//!
//! 界面还没法用脚本点击（AppleScript 被权限拦），这个示例把「新建主机 →
//! 保存 → 连接」的前半段做实，后半段由应用的 `DAISY_SMOKE_AUTOCONNECT`
//! 钩子完成——两者走的是同一条 store 路径。
//!
//! ```bash
//! DAISY_DATA_DIR=/tmp/daisy-demo \
//! DAISY_SEED_PASSWORD='xxx' \
//!   cargo run -p daisy-app --example seed_host -- "验收机" 192.168.10.21 22 root [代理id]
//! ```

use daisy_core::config::AppPaths;
use daisy_core::key::EmbeddedKey;
use daisy_core::model::{Host, Protocol};
use daisy_store::{Database, HostSecrets};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut args = std::env::args().skip(1);
	let name = args
		.next()
		.ok_or("用法：seed_host <名称> <地址> <端口> <用户名>")?;
	let host = args.next().ok_or("缺少参数：地址")?;
	let port: u16 = args
		.next()
		.ok_or("缺少参数：端口")?
		.parse()
		.map_err(|error| format!("端口不是数字：{error}"))?;
	let user = args.next().ok_or("缺少参数：用户名")?;
	// 第五个参数可选：关联的代理主键（验证「经代理连接」用）
	let proxy_id = args
		.next()
		.filter(|raw| !raw.trim().is_empty())
		.map(|raw| {
			raw.parse::<i64>()
				.map_err(|error| format!("代理 id 不是数字：{error}"))
		})
		.transpose()?;

	// 口令只从环境变量读，不进 argv——进程列表对其它用户可见
	let password = std::env::var("DAISY_SEED_PASSWORD")
		.map_err(|_| "缺少环境变量 DAISY_SEED_PASSWORD（口令不走命令行参数）")?;

	let paths = match std::env::var_os("DAISY_DATA_DIR") {
		Some(base) if !base.is_empty() => AppPaths::from_base(base),
		_ => AppPaths::discover()?,
	};

	let db = Database::open(&paths, &EmbeddedKey)?;

	// 协议走环境变量而不是位置参数：位置参数已经排到第 5 个（代理 id），
	// 再插一个会打乱现有脚本。默认 SSH，`DAISY_SEED_PROTOCOL=rdp` 灌远程桌面主机。
	let protocol = match std::env::var("DAISY_SEED_PROTOCOL")
		.unwrap_or_else(|_| "ssh".to_string())
		.to_ascii_lowercase()
		.as_str()
	{
		"rdp" => Protocol::Rdp,
		"ssh" => Protocol::Ssh,
		other => return Err(format!("未知协议 {other}（可选 ssh / rdp）").into()),
	};

	let mut entry = Host::draft(protocol);
	entry.name = name.clone();
	entry.host = host.clone();
	entry.port = port;
	entry.username = Some(user.clone());
	entry.proxy_id = proxy_id;

	let id = db.save_host(&entry, &HostSecrets::with_password(password))?;
	println!(
		"已保存主机 id={id}：{name}（{user}@{host}:{port}{}）",
		proxy_id.map_or_else(String::new, |proxy| format!("，经代理 #{proxy}"))
	);
	Ok(())
}
