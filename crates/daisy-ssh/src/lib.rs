//! Daisy SSH 会话层。
//!
//! 两种认证方式（见 PLAN.md §5.2）：
//!
//! - **密码认证** —— 密码从加密库取出后交给 `russh`，明文只在内存短暂存在，
//!   用完即清零（[`std::zeroize::Zeroizing`]）
//! - **RSA 私钥认证** —— 支持 OpenSSH / PKCS#1 / PKCS#8 / PuTTY PPK，
//!   支持带口令私钥
//!
//! 签名算法优先级：`rsa-sha2-512` → `rsa-sha2-256` → `ssh-rsa`（仅在显式开启
//! 「兼容旧服务器」时启用，见 [`key::RsaSigningPolicy`]）。
//!
//! | 模块 | 职责 | 步骤 |
//! | --- | --- | --- |
//! | [`key`] | 私钥加载、格式判定、RSA 签名策略 | 42–44 |
//! | [`session`] | 建连、主机密钥校验、认证顺序与失败回退 | 40–41、45–46、49 |
//! | [`channel`] | 远端 PTY、字节流桥接到 `daisy-term` | 47–48 |
//!
//! ## 异步边界
//!
//! SSH 是异步的（tokio），界面是同步的（egui 每帧拉一次）。
//! [`channel::RemoteShell`] 用一个后台任务把两边接起来，
//! 并实现 `daisy-term` 的 `SessionLink`——因此远端会话与本地终端
//! 共用同一套网格与渲染代码。

pub mod channel;
pub mod error;
pub mod jump;
pub mod key;
pub mod session;
pub mod sftp;

pub use channel::{RemoteShell, ShellEvent, ShellRequest};
pub use error::{Result, SshError};
pub use jump::{JumpDialer, connect_via_jump};
pub use key::{KeyFormat, LoadedKey, RsaSigningPolicy};
pub use session::{
	AuthAttempt, AuthMethod, AuthOutcome, AuthSpec, HostKeyInfo, HostKeyPolicy, HostKeyVerdict,
	SshParams, SshSession, SshTarget, TrustedHostKey, connect,
};
pub use sftp::{DirEntry, SftpClient, join_path, parent_of};

#[cfg(test)]
mod e2e;
#[cfg(test)]
mod test_server;
