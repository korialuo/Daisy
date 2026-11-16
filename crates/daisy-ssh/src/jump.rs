//! SSH 跳板机（ProxyJump）：经一条已建立的 SSH 会话到达内网主机。
//!
//! 对应 STEPS.md 步骤 55。
//!
//! 把「在跳板机上开 direct-tcpip 通道」实现成 [`Dialer`]，于是它与
//! 直连、SOCKS5、HTTP CONNECT 完全并列——上层（SSH / RDP）无需感知
//! 自己走的是哪种路径。
//!
//! ```text
//!   目标 SSH/RDP ──▶ JumpDialer ──▶ 跳板机 SSH 会话
//!                                    └── direct-tcpip ──▶ 内网主机
//! ```
//!
//! 每次 `dial` 都开一条**新的** direct-tcpip 通道（通道不能复用）。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use daisy_net::{BoxStream, Dialer, NetError, TargetAddr};

use crate::error::{Result, SshError};
use crate::session::SshSession;

/// 把 russh 的 `ChannelStream` 包一层。
///
/// 不能直接 `impl AsyncStream for russh::ChannelStream`——孤儿规则禁止在
/// 本 crate 为外部类型实现外部 trait。newtype 是标准解法。
///
/// [`AsyncStream`] 有空白实现（`T: AsyncRead + AsyncWrite + Unpin + Send`），
/// 因此只要下面转发好 `AsyncRead` / `AsyncWrite`，`JumpStream` 就自动满足。
struct JumpStream(russh::ChannelStream<russh::client::Msg>);

impl tokio::io::AsyncRead for JumpStream {
	fn poll_read(
		mut self: Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &mut tokio::io::ReadBuf<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		Pin::new(&mut self.0).poll_read(cx, buf)
	}
}

impl tokio::io::AsyncWrite for JumpStream {
	fn poll_write(
		mut self: Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
		buf: &[u8],
	) -> std::task::Poll<std::io::Result<usize>> {
		Pin::new(&mut self.0).poll_write(cx, buf)
	}

	fn poll_flush(
		mut self: Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		Pin::new(&mut self.0).poll_flush(cx)
	}

	fn poll_shutdown(
		mut self: Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<std::io::Result<()>> {
		Pin::new(&mut self.0).poll_shutdown(cx)
	}
}

/// 经跳板机 SSH 会话到达目标的拨号器。
pub struct JumpDialer {
	session: Arc<SshSession>,
}

impl JumpDialer {
	/// 用一条已建立的 SSH 会话构造。
	#[must_use]
	pub fn new(session: Arc<SshSession>) -> Self {
		Self { session }
	}

	/// 跳板机会话。
	#[must_use]
	pub fn session(&self) -> &SshSession {
		&self.session
	}
}

impl Dialer for JumpDialer {
	fn dial<'a>(
		&'a self,
		target: &'a TargetAddr,
	) -> Pin<Box<dyn Future<Output = Result<BoxStream, NetError>> + Send + 'a>> {
		Box::pin(async move {
			// 在跳板机上开一条到目标的 direct-tcpip 通道
			let channel = self
				.session
				.handle()
				.channel_open_direct_tcpip(
					target.host().to_string(),
					u32::from(target.port()),
					"127.0.0.1",
					0,
				)
				.await
				.map_err(|error| {
					NetError::Tunnel(format!(
						"跳板机 {} 未能打开到 {} 的通道：{error}",
						self.session.target().host,
						target.display()
					))
				})?;

			let stream = JumpStream(channel.into_stream());
			Ok(Box::new(stream) as BoxStream)
		})
	}

	fn describe(&self) -> String {
		format!("经跳板机 {}", self.session.target().display())
	}
}

impl std::fmt::Debug for JumpDialer {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("JumpDialer")
			.field("session", &self.session)
			.finish()
	}
}

/// 建立一条经跳板机的 SSH 会话。
///
/// 先连跳板机，再拿 [`JumpDialer`] 作为内层拨号器去连真正目标。
/// 这是 [`session::connect`] 之上的组合，不重复造连接逻辑。
///
/// `inner_host_key` 是**目标**的主机密钥策略——必须由调用方给出。
/// 早先这里用 `SshParams::new` 的默认策略（`known: None`），
/// 等于跳板机路径的 known_hosts 指纹核对整体失效：Mismatch 永不触发，
/// 每次连接都被当成「首次见到」。指纹锁不能留这种暗门。
pub async fn connect_via_jump(
	jump_params: crate::SshParams,
	inner_target: crate::SshTarget,
	inner_auth: crate::AuthSpec,
	inner_host_key: crate::HostKeyPolicy,
) -> Result<crate::SshSession> {
	let jump = Arc::new(crate::connect(jump_params).await?);

	let params = crate::SshParams::new(inner_target, inner_auth)
		.with_dialer(Box::new(JumpDialer::new(Arc::clone(&jump))))
		.with_host_key_policy(inner_host_key);

	crate::connect(params).await.map_err(|error| {
		// 跳板机本身还活着、但目标连不上时，错误信息要能区分这两层
		SshError::JumpConnect {
			jump: jump.target().display(),
			reason: error.to_string(),
		}
	})
}
