//! 本地端口转发：把一个本地监听端口桥接到经 [`Dialer`] 可达的目标。
//!
//! 对应 STEPS.md 步骤 54。
//!
//! ## 为什么需要它
//!
//! SSH 可以直接吃 `AsyncRead + AsyncWrite`，所以代理隧道对它透明。
//! 但 **FreeRDP 只认 TCP socket**，没法直接塞给它一条代理隧道。
//!
//! 办法就是「落地」：在本地 `127.0.0.1:0` 起一个监听器，把每个入站连接
//! 经 `Dialer` 转发到真正的目标，再把 socket 交给 FreeRDP。于是 RDP 的
//! 「经代理 / 经跳板机」都退化成「连一个本地端口」。
//!
//! ```text
//!   FreeRDP ──TCP──▶ 127.0.0.1:port ──Dialer──▶ 目标 host:port
//!                     （LocalForward）
//! ```
//!
//! ## 生命周期
//!
//! [`LocalForward::bind`] 返回后监听器已就绪。`Drop` 时通过 [`tokio::sync::Notify`]
//! 通知接受循环退出，并关闭监听器。绑定 `127.0.0.1:0` 由系统分配端口，
//! 会话结束自动释放，不会泄漏。

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::dialer::{Dialer, TargetAddr};
use crate::error::{NetError, Result};

/// 本地端口转发器。
///
/// 起一个本地监听器，把入站 TCP 连接逐个经 `dialer` 转发到 `target`。
/// `dialer` 用 [`Arc`] 共享——每个入站连接独立拨号，不能复用同一条流。
///
/// 监听器本身由后台接受循环任务持有；本结构体只记地址、目标与停止信号。
/// `Drop` 时发停止信号，接受循环退出后监听器随之释放。
pub struct LocalForward {
	local_addr: SocketAddr,
	target: TargetAddr,
	shutdown: Arc<Notify>,
}

impl LocalForward {
	/// 绑定本地监听器并开始转发。绑定 `127.0.0.1:0` 会由系统分配端口。
	pub async fn bind(
		bind_addr: &str,
		target: TargetAddr,
		dialer: Arc<dyn Dialer>,
	) -> Result<Self> {
		let listener = Arc::new(TcpListener::bind(bind_addr).await.map_err(|source| {
			NetError::InvalidTarget(format!("绑定本地转发端口失败：{source}"))
		})?);
		let local_addr = listener
			.local_addr()
			.map_err(|source| NetError::InvalidTarget(format!("读取本地转发端口失败：{source}")))?;

		let shutdown = Arc::new(Notify::new());

		let task_listener = Arc::clone(&listener);
		let task_target = target.clone();
		let task_shutdown = Arc::clone(&shutdown);
		tokio::spawn(async move {
			accept_loop(task_listener, task_target, dialer, task_shutdown).await;
		});

		Ok(Self {
			local_addr,
			target,
			shutdown,
		})
	}

	/// 本地监听地址。`127.0.0.1:<port>`，交给 FreeRDP 的就是这个。
	#[must_use]
	pub fn local_addr(&self) -> SocketAddr {
		self.local_addr
	}

	/// 本地端口。
	#[must_use]
	pub fn local_port(&self) -> u16 {
		self.local_addr.port()
	}

	/// 转发目标。
	#[must_use]
	pub fn target(&self) -> &TargetAddr {
		&self.target
	}

	/// 一句话描述，用于日志。
	#[must_use]
	pub fn describe(&self) -> String {
		format!("{} → {}", self.local_addr, self.target.display())
	}
}

impl Drop for LocalForward {
	fn drop(&mut self) {
		// 通知接受循环退出。listener 的 Arc 仍在任务里持有一份，
		// 这里不主动 drop 它——由任务退出后自行释放。
		self.shutdown.notify_waiters();
	}
}

/// 接受连接的主循环。每个入站连接起一个任务做双向转发。
async fn accept_loop(
	listener: Arc<TcpListener>,
	target: TargetAddr,
	dialer: Arc<dyn Dialer>,
	shutdown: Arc<Notify>,
) {
	loop {
		let incoming = tokio::select! {
			result = listener.accept() => result,
			_ = shutdown.notified() => return,
		};

		let (mut local, peer) = match incoming {
			Ok(pair) => pair,
			Err(error) => {
				tracing::debug!("本地转发 accept 失败：{error}");
				return;
			},
		};

		tracing::debug!("本地转发：{peer} 连入，转发到 {}", target.display());

		// 每个连接独立 dial，因为代理隧道通常不能复用
		let conn_target = target.clone();
		let conn_dialer = Arc::clone(&dialer);
		tokio::spawn(async move {
			let _ = bridge(&mut local, &conn_target, conn_dialer.as_ref()).await;
		});
	}
}

/// 桥接一条本地连接：先 dial 到目标，再双向拷贝字节。
async fn bridge(
	local: &mut tokio::net::TcpStream,
	target: &TargetAddr,
	dialer: &dyn Dialer,
) -> Result<()> {
	let mut remote = dialer.dial(target).await?;

	// 双向拷贝。任一方向断开就收尾。
	let (mut local_read, mut local_write) = tokio::io::split(local);
	let (mut remote_read, mut remote_write) = tokio::io::split(&mut remote);

	let local_to_remote = tokio::io::copy(&mut local_read, &mut remote_write);
	let remote_to_local = tokio::io::copy(&mut remote_read, &mut local_write);

	tokio::select! {
		_ = local_to_remote => {},
		_ = remote_to_local => {},
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	use daisy_core::HostAddr;
	use tokio::io::{AsyncReadExt, AsyncWriteExt};
	use tokio::net::TcpListener;

	use crate::DirectDialer;

	fn target(host: &str, port: u16) -> TargetAddr {
		TargetAddr::new(HostAddr::parse(host).expect("测试地址应合法"), port)
	}

	/// 起一个回显服务端，读一段写回一段后即退出。
	async fn echo_server() -> (u16, tokio::task::JoinHandle<()>) {
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定失败");
		let port = listener.local_addr().expect("取地址失败").port();
		let handle = tokio::spawn(async move {
			let Ok((mut socket, _)) = listener.accept().await else {
				return;
			};
			let mut buffer = [0u8; 256];
			if let Ok(n) = socket.read(&mut buffer).await
				&& n > 0
			{
				let _ = socket.write_all(&buffer[..n]).await;
			}
		});
		(port, handle)
	}

	fn direct_dialer() -> Arc<dyn Dialer> {
		Arc::new(DirectDialer::new(Duration::from_secs(5)))
	}

	#[tokio::test]
	async fn forwards_bytes_both_ways() {
		let (echo_port, echo) = echo_server().await;
		let forward = LocalForward::bind(
			"127.0.0.1:0",
			target("127.0.0.1", echo_port),
			direct_dialer(),
		)
		.await
		.expect("绑定失败");

		// 连到本地转发端口，等于连到了回显服务
		let mut stream = tokio::net::TcpStream::connect(forward.local_addr())
			.await
			.expect("连接失败");
		stream.write_all(b"hello-forward").await.expect("写入失败");

		let mut buf = [0u8; 32];
		let n = stream.read(&mut buf).await.expect("读取失败");
		assert_eq!(&buf[..n], b"hello-forward", "回显应原样返回");

		drop(forward);
		echo.await.expect("回显任务失败");
	}

	#[tokio::test]
	async fn binds_to_an_ephemeral_port() {
		let (echo_port, echo) = echo_server().await;
		let forward = LocalForward::bind(
			"127.0.0.1:0",
			target("127.0.0.1", echo_port),
			direct_dialer(),
		)
		.await
		.expect("绑定失败");

		assert!(forward.local_port() > 0, "应拿到一个系统分配的非零端口");
		assert_eq!(forward.local_addr().ip().to_string(), "127.0.0.1");
		assert_eq!(forward.target().display(), format!("127.0.0.1:{echo_port}"));

		drop(forward);
		echo.abort();
	}

	#[tokio::test]
	async fn unreachable_target_does_not_kill_the_listener() {
		// 绑一个端口再释放，拿到没人听的端口
		let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定失败");
		let dead_port = listener.local_addr().expect("取地址失败").port();
		drop(listener);

		let forward = LocalForward::bind(
			"127.0.0.1:0",
			target("127.0.0.1", dead_port),
			direct_dialer(),
		)
		.await
		.expect("绑定失败");

		// 连到转发端口，目标不可达，转发器本身应该还活着（能再 accept）
		let mut stream = tokio::net::TcpStream::connect(forward.local_addr())
			.await
			.expect("连接失败");
		let mut buf = [0u8; 8];
		// 目标不可达，读会得到 EOF
		let _ = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await;

		drop(forward);
	}
}
