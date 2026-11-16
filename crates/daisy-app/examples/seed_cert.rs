//! 往库里写一条假的 RDP 证书指纹（夹具，不连任何主机）。
//!
//! 两个用途：
//!
//! 1. **看界面**：主机编辑表单里的「证书指纹」区块只有存在记录时才显示，
//!    而真机可达之前没法自然产生一条。
//! 2. **模拟「指纹变了」**：给一台连得上的主机塞一个假指纹，再去连——
//!    应当被拒，并说明「记录为 X、本次为 Y」。这正是 S8-65 要验的第三条判定，
//!    不必等真机重装系统。
//!
//! ```sh
//! # 写一个固定假指纹（默认值，方便记）
//! cargo run -p daisy-app --example seed_cert -- "验收机"
//! # 写指定指纹
//! cargo run -p daisy-app --example seed_cert -- "验收机" aabbccdd
//! # 清掉记录（等价于界面上的「重新信任」）
//! cargo run -p daisy-app --example seed_cert -- "验收机" clear
//! ```

use daisy_core::config::AppPaths;
use daisy_core::key::EmbeddedKey;
use daisy_store::Database;

/// 默认假指纹：64 个十六进制字符（sha256），与真实指纹长度一致。
const DEFAULT_FAKE: &str = "00112233445566778899aabbccddeeff\
                           00112233445566778899aabbccddeeff";

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut args = std::env::args().skip(1);
	let name = args.next().ok_or("用法：seed_cert <主机名> [指纹|clear]")?;
	let value = args.next().unwrap_or_else(|| DEFAULT_FAKE.to_string());

	let paths = match std::env::var_os("DAISY_DATA_DIR") {
		Some(base) if !base.is_empty() => AppPaths::from_base(base),
		_ => AppPaths::discover()?,
	};
	let db = Database::open(&paths, &EmbeddedKey)?;

	let host = db
		.hosts()?
		.into_iter()
		.find(|host| host.name == name)
		.ok_or_else(|| format!("找不到名为「{name}」的主机"))?;

	if value == "clear" {
		let removed = db.forget_rdp_cert(&host.host, host.port)?;
		println!(
			"已清除 {}:{} 的证书记录（{}）",
			host.host,
			host.port,
			if removed {
				"确实有一条"
			} else {
				"本来就没有"
			}
		);
		return Ok(());
	}

	db.remember_rdp_cert(
		&host.host,
		host.port,
		&value,
		Some("CN=Daisy 测试夹具"),
		Some("CN=Daisy 测试夹具"),
	)?;
	println!(
		"已写入 {}:{} 的假指纹：{}",
		host.host,
		host.port,
		daisy_rdp::format_fingerprint(&value)
	);
	Ok(())
}
