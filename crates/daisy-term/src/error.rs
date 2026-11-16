//! 终端层错误类型。

use thiserror::Error;

/// 终端层错误。
#[derive(Debug, Error)]
pub enum TermError {
	/// 上游核心层错误。
	#[error("{0}")]
	Core(#[from] daisy_core::CoreError),

	/// 无法拉起终端进程（可执行文件不存在、权限不足等）。
	#[error("无法启动终端进程：{0}")]
	Spawn(String),

	/// 与 PTY 的读写失败。
	#[error("终端读写失败：{0}")]
	Io(#[from] std::io::Error),

	/// 尺寸非法。
	#[error("终端尺寸非法：{0}")]
	InvalidSize(String),

	/// 会话已经结束，不能再写入。
	#[error("终端会话已结束")]
	SessionClosed,

	/// ANSI 解析或终端状态异常。
	#[error("终端状态异常：{0}")]
	Terminal(String),
}

/// 终端层结果别名。
pub type Result<T, E = TermError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn messages_are_user_facing() {
		assert_eq!(TermError::SessionClosed.to_string(), "终端会话已结束");
		assert!(
			TermError::Spawn("no such file".to_string())
				.to_string()
				.contains("no such file")
		);
	}
}
