//! 核心错误类型。
//!
//! 约定：每个 crate 用 [`thiserror`] 定义结构化错误枚举，界面层按需分类展示；
//! 不引入额外的错误聚合库（没必要，且会丢掉类型信息）。
//! 错误文案面向最终用户，应可直接展示在界面上。

use thiserror::Error;

/// Daisy 核心错误。
#[derive(Debug, Error)]
pub enum CoreError {
	/// 配置内容非法（值缺失、超出范围、格式不符）。
	#[error("配置无效：{0}")]
	InvalidConfig(String),

	/// 当前平台或当前版本不支持该能力。
	#[error("不支持的操作：{0}")]
	Unsupported(String),

	/// 内部不变量被破坏，属于缺陷而非用户输入问题。
	#[error("内部错误：{0}")]
	Internal(String),
}

/// 核心 crate 的便捷结果别名。
pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn error_messages_are_user_facing() {
		let err = CoreError::InvalidConfig("端口必须是 1-65535".to_string());
		assert_eq!(err.to_string(), "配置无效：端口必须是 1-65535");
	}

	#[test]
	fn result_alias_defaults_to_core_error() {
		fn parse_port(raw: &str) -> Result<u16> {
			raw.parse::<u16>()
				.map_err(|e| CoreError::InvalidConfig(e.to_string()))
		}

		assert_eq!(parse_port("3389").unwrap(), 3389);
		assert!(parse_port("not-a-port").is_err());
	}
}
