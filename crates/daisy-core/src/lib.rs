//! Daisy 核心 crate。
//!
//! 承载与具体协议无关的部分：
//!
//! - `addr` —— 主机地址（域名 / IPv4 / IPv6）的统一解析与规范化
//! - `model` —— 领域模型（主机、代理、分组、认证方式、显示设置）
//! - `config` —— 应用配置与应用数据目录定位
//! - `logging` —— 日志初始化（标准输出 + 按日滚动落盘）
//! - `event` —— 跨模块事件总线
//! - `key` —— 主密钥提供者抽象
//!
//! 本 crate **不得**依赖任何 GUI 或协议实现（SSH / RDP），保持纯净。

pub mod addr;
pub mod config;
pub mod error;
pub mod event;
pub mod key;
pub mod logging;
pub mod model;

pub use addr::{HostAddr, HostKind, endpoint};
pub use error::{CoreError, Result};

/// 应用名，用于窗口标题、日志前缀与数据目录名。
pub const APP_NAME: &str = "Daisy";

/// 应用版本，取自 workspace 包版本。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn app_identity_is_stable() {
		assert_eq!(APP_NAME, "Daisy");
		assert!(!APP_VERSION.is_empty());
	}
}
