//! 基于 `PRAGMA user_version` 的 schema 迁移框架。
//!
//! 规则：
//!
//! - 每个迁移脚本对应一个从 1 开始**连续递增**的版本号
//! - 迁移在一个事务里完成，SQL 与版本号一起提交，失败则整体回滚
//! - 重复执行幂等：已经到位的版本会被跳过
//! - 库的版本高于本程序认识的版本时直接报错，**不尝试降级**

use rusqlite::Connection;

use crate::error::{Result, StoreError};

/// 迁移脚本表。`(版本号, SQL 内容)`，按版本号升序。
///
/// 新增迁移时在末尾追加，不要改动已有条目——已经发布出去的历史不能重写。
const MIGRATIONS: &[(i64, &str)] = &[
	(1, include_str!("../migrations/001_init.sql")),
	// S8-63：剪贴板映射开关（对应 mstsc 连接前的「剪贴板」勾选项）
	(2, include_str!("../migrations/002_clipboard.sql")),
	// S8-65：RDP 服务端证书指纹（PLAN R17）
	(3, include_str!("../migrations/003_rdp_certs.sql")),
	// S8-64：远端 DPI 缩放（Retina 上「跟随窗口」必需，否则远端字太小）
	(4, include_str!("../migrations/004_dpi.sql")),
];

/// 本程序认识的最高 schema 版本。
pub const SCHEMA_VERSION: i64 = 4;

/// 读取库当前的 `user_version`。
pub fn current_version(conn: &Connection) -> Result<i64> {
	let version = conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
	Ok(version)
}

/// 把库升到 [`SCHEMA_VERSION`]。已是最新时不做任何事。
pub fn migrate(conn: &mut Connection) -> Result<()> {
	let current = current_version(conn)?;

	if current > SCHEMA_VERSION {
		return Err(StoreError::SchemaTooNew {
			found: current,
			supported: SCHEMA_VERSION,
		});
	}

	for (version, sql) in MIGRATIONS {
		if *version <= current {
			continue;
		}

		tracing::info!("应用 schema 迁移 v{version}");

		let tx = conn.transaction()?;
		tx.execute_batch(sql)?;
		// PRAGMA 不支持参数绑定。版本号来自编译期常量，不涉及外部输入。
		tx.execute_batch(&format!("PRAGMA user_version = {version};"))?;
		tx.commit()?;
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use rusqlite::Connection;

	/// 建一个带密钥的内存库。
	fn memory_db() -> Connection {
		let conn = Connection::open_in_memory().expect("打开内存库失败");
		conn.pragma_update(None, "key", "test-key")
			.expect("设置密钥失败");
		conn
	}

	fn table_names(conn: &Connection) -> Vec<String> {
		let mut stmt = conn
			.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
			.expect("准备查询失败");
		let rows = stmt
			.query_map([], |row| row.get::<_, String>(0))
			.expect("遍历失败");
		rows.filter_map(std::result::Result::ok)
			.filter(|name| !name.starts_with("sqlite_"))
			.collect()
	}

	#[test]
	fn schema_version_matches_last_migration() {
		let last = MIGRATIONS.last().map(|(v, _)| *v).unwrap_or(0);
		assert_eq!(
			SCHEMA_VERSION, last,
			"SCHEMA_VERSION 与迁移表最后一个版本号不一致"
		);
	}

	#[test]
	fn migration_versions_are_contiguous_from_one() {
		for (index, (version, _)) in MIGRATIONS.iter().enumerate() {
			let expected = index as i64 + 1;
			assert_eq!(*version, expected, "迁移版本号必须从 1 开始连续递增");
		}
	}

	#[test]
	fn fresh_database_starts_at_zero() {
		let conn = memory_db();
		assert_eq!(current_version(&conn).expect("读取版本失败"), 0);
	}

	#[test]
	fn migrate_creates_all_tables() {
		let mut conn = memory_db();
		migrate(&mut conn).expect("迁移失败");

		assert_eq!(
			current_version(&conn).expect("读取版本失败"),
			SCHEMA_VERSION
		);

		let names = table_names(&conn);
		for expected in ["groups", "hosts", "known_hosts", "proxies", "settings"] {
			assert!(
				names.contains(&expected.to_string()),
				"缺少表 {expected}，实际有 {names:?}"
			);
		}
	}

	#[test]
	fn migrate_is_idempotent() {
		let mut conn = memory_db();

		migrate(&mut conn).expect("第一次迁移失败");
		let after_first = table_names(&conn);

		// 重复执行不应报错，也不应改变结构
		migrate(&mut conn).expect("第二次迁移失败");
		migrate(&mut conn).expect("第三次迁移失败");

		assert_eq!(
			current_version(&conn).expect("读取版本失败"),
			SCHEMA_VERSION
		);
		assert_eq!(table_names(&conn), after_first);
	}

	#[test]
	fn schema_too_new_is_rejected() {
		let mut conn = memory_db();
		migrate(&mut conn).expect("迁移失败");

		conn.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 5))
			.expect("写入版本失败");

		let err = migrate(&mut conn).expect_err("未来版本应被拒绝");
		assert!(
			matches!(err, StoreError::SchemaTooNew { .. }),
			"错误类型不符：{err:?}"
		);
	}

	#[test]
	fn check_constraints_reject_bad_protocol() {
		let mut conn = memory_db();
		migrate(&mut conn).expect("迁移失败");

		let outcome = conn.execute(
			"INSERT INTO hosts (name, protocol, host, port, created_at, updated_at)
			 VALUES ('x', 'telnet', 'h', 22, 0, 0)",
			[],
		);
		assert!(outcome.is_err(), "CHECK 约束未生效，非法协议被写入了");
	}
}
