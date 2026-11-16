//! 存储层错误类型。
//!
//! 与 `daisy-core` 同样的约定：库 crate 用结构化错误枚举，文案面向最终用户。

use thiserror::Error;

/// 存储层错误。
#[derive(Debug, Error)]
pub enum StoreError {
	/// 上游核心层错误（配置、模型校验、密钥提供者）。
	#[error("{0}")]
	Core(#[from] daisy_core::CoreError),

	/// SQLite / SQLCipher 层面的错误。
	#[error("数据库错误：{0}")]
	Database(#[from] rusqlite::Error),

	/// 文件系统错误。
	#[error("文件操作失败：{0}")]
	Io(#[from] std::io::Error),

	/// 库中数据不符合预期（字段缺失、格式非法、约束被破坏）。
	#[error("数据格式异常：{0}")]
	Format(String),

	/// 按主键查找未命中。
	#[error("找不到{entity}（id={id}）")]
	NotFound {
		/// 实体名，用于拼错误文案。
		entity: &'static str,
		/// 主键。
		id: i64,
	},

	/// 密钥派生或加解密失败。
	#[error("加解密失败：{0}")]
	Crypto(String),

	/// 数据库 schema 版本比当前程序认识的更新。
	#[error("数据库版本 {found} 高于本程序支持的 {supported}，请升级 Daisy")]
	SchemaTooNew {
		/// 库中的 `user_version`。
		found: i64,
		/// 本程序支持的最高版本。
		supported: i64,
	},
}

/// 存储层结果别名。
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn not_found_message_is_readable() {
		let err = StoreError::NotFound {
			entity: "主机",
			id: 42,
		};
		assert_eq!(err.to_string(), "找不到主机（id=42）");
	}

	#[test]
	fn schema_too_new_message_is_actionable() {
		let err = StoreError::SchemaTooNew {
			found: 9,
			supported: 3,
		};
		let text = err.to_string();
		assert!(text.contains('9') && text.contains('3'));
		assert!(text.contains("升级"));
	}
}
