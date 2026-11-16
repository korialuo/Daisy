//! 界面层错误类型。

use thiserror::Error;

/// 界面层错误。
#[derive(Debug, Error)]
pub enum AppError {
	/// 终端层错误。
	#[error("{0}")]
	Term(#[from] daisy_term::TermError),

	/// 存储层错误。
	#[error("{0}")]
	Store(#[from] daisy_store::StoreError),

	/// 核心层错误。
	#[error("{0}")]
	Core(#[from] daisy_core::CoreError),
}

/// 界面层结果别名。
pub type Result<T, E = AppError> = std::result::Result<T, E>;
