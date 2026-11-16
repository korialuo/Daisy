//! 灌一套「分组 + 主机」数据，供分组树的可视化验证使用。
//!
//! ```bash
//! DAISY_DATA_DIR=/tmp/daisy-groups cargo run -p daisy-app --example seed_group_demo
//! ```
//!
//! 造出的结构覆盖树形展示的各种情形：
//!
//! ```text
//! 生产
//! ├── 华东
//! └── 华南
//! 测试
//! 归档（空分组——验证空分组也显示）
//! 未分组
//! ├── alpha
//! └── zoo
//! ```
//!
//! 主机名故意用大小写混合与非字母开头，顺带验证「按名称排序不区分大小写」。
//! 「测试」组里放一台 RDP 主机：既覆盖协议标签的展示，也让
//! `DAISY_SMOKE_AUTOCONNECT` 能演示「RDP 界面未接入」的错误提示。

use daisy_core::config::AppPaths;
use daisy_core::key::EmbeddedKey;
use daisy_core::model::{Host, Protocol, ScaleMode, ScaleSettings};
use daisy_store::{Database, HostSecrets};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	// 与主程序同源：`DAISY_DATA_DIR` 优先，否则走标准目录
	let paths = match std::env::var_os("DAISY_DATA_DIR") {
		Some(base) if !base.is_empty() => AppPaths::from_base(base),
		_ => AppPaths::discover()?,
	};

	let db = Database::open(&paths, &EmbeddedKey)?;

	let production = db.create_group("生产", None)?;
	let east = db.create_group("华东", Some(production))?;
	let south = db.create_group("华南", Some(production))?;
	let testing = db.create_group("测试", None)?;
	db.create_group("归档", None)?;

	let seed =
		|name: &str, addr: &str, group: Option<i64>| -> Result<(), Box<dyn std::error::Error>> {
			let mut host = Host::draft(Protocol::Ssh);
			host.name = name.to_string();
			host.host = addr.to_string();
			host.username = Some("root".to_string());
			host.group_id = group;
			db.save_host(&host, &HostSecrets::with_password("demo-password"))?;
			Ok(())
		};

	seed("数据库主库", "10.20.1.10", Some(east))?;
	seed("应用服务器", "10.20.1.11", Some(east))?;
	seed("备份节点", "10.20.2.10", Some(south))?;
	seed("alpha", "10.30.0.1", None)?;
	seed("zoo", "10.30.0.2", None)?;

	// 一台 RDP 主机。密码可用 `DAISY_SEED_RDP_PASSWORD` 覆盖——
	// 真机验证时要连得上，而默认的占位串只够演示界面
	let rdp_password =
		std::env::var("DAISY_SEED_RDP_PASSWORD").unwrap_or_else(|_| "demo-password".to_string());
	let mut rdp = Host::draft(Protocol::Rdp);
	rdp.name = "Windows 测试机".to_string();
	rdp.host = "192.168.10.20".to_string();
	rdp.port = 3389;
	rdp.username = Some("Administrator".to_string());
	rdp.group_id = Some(testing);
	// 缩放档可用 `DAISY_SEED_RDP_SCALE` 指定。缩放是**每台主机各自的偏好**，
	// 存在库里；写成一个非默认值才能验证「读出来确实生效」
	if let Ok(raw) = std::env::var("DAISY_SEED_RDP_SCALE") {
		let mode = match raw.trim() {
			"fit" => ScaleMode::Fit,
			"fill" => ScaleMode::Fill,
			"actual" => ScaleMode::Actual,
			"custom" => ScaleMode::Custom,
			other => {
				eprintln!("DAISY_SEED_RDP_SCALE 的取值「{other}」不认识，已忽略");
				ScaleMode::Fit
			},
		};
		rdp.display.scale = ScaleSettings { mode, factor: 1.5 };
	}
	db.save_host(&rdp, &HostSecrets::with_password(rdp_password))?;

	println!(
		"已灌入：{} 个分组（生产/华东/华南/测试/归档）、6 台主机",
		db.groups()?.len()
	);
	Ok(())
}
