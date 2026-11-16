//! Daisy 网络层。
//!
//! 两类消费方式共用同一套代理握手实现（见 PLAN.md §5.1）：
//!
//! - **SSH 路径**：`russh` 直接接管 `AsyncRead + AsyncWrite`，无需额外转发
//! - **RDP 路径**：FreeRDP 只认 TCP，因此在本机 `127.0.0.1:0` 起监听器做桥接
//!
//! | 模块 | 职责 | 阶段 |
//! | --- | --- | --- |
//! | [`dialer`] | 连接抽象与直连拨号 | S6 步骤 39 |
//! | `socks5` / `http_proxy` / `tunnel` | 代理隧道（第二个 [`dialer::Dialer`] 实现） | S7 |
//! | `forward` | 本地端口桥接，供 FreeRDP 使用 | S7 |
//!
//! **本 crate 不含任何平台判断**，也不依赖 `daisy-term` / `daisy-ssh`。

pub mod dialer;
pub mod error;
pub mod forward;
pub mod http_proxy;
pub mod socks5;
pub mod tunnel;

pub use dialer::{
	AsyncStream, BoxStream, DEFAULT_CONNECT_TIMEOUT, Dialer, DirectDialer, TargetAddr,
};
pub use error::{NetError, Result};
pub use forward::LocalForward;
pub use tunnel::{ProxyAuth, ProxyDialer, ProxyEndpoint, build_dialer};

#[cfg(test)]
pub(crate) mod test_stream;
