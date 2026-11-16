//! 测试辅助：构造一个已加密、已迁移的内存库。

use rusqlite::Connection;

use crate::crypto::FieldCipher;
use crate::migrate;

/// 固定的测试字段密钥。测试里不需要真实随机性，固定值便于复现。
pub(crate) const TEST_FIELD_KEY: [u8; 32] = [7u8; 32];

/// 建一个已设密钥、已开外键、已迁移的内存库。
pub(crate) fn memory_db() -> (Connection, FieldCipher) {
	let mut conn = Connection::open_in_memory().expect("打开内存库失败");
	conn.pragma_update(None, "key", "test-key")
		.expect("设置密钥失败");
	conn.execute_batch("PRAGMA foreign_keys = ON;")
		.expect("开启外键失败");
	migrate::migrate(&mut conn).expect("迁移失败");

	(conn, FieldCipher::new(TEST_FIELD_KEY))
}
