//! 远端 shell 通道，并桥接到 `daisy-term`。
//!
//! 对应 STEPS.md 步骤 47–48。
//!
//! ## 异步 / 同步接缝
//!
//! SSH 是异步的，界面每帧同步拉一次数据。中间靠两条件量接起来：
//!
//! ```text
//!   界面线程                    后台任务                   远端
//!   ────────                    ────────                   ────
//!   write ─▶ 请求队列 ─────────▶ channel.data ────────────▶ stdin
//!   resize ─▶ 请求队列 ─────────▶ channel.window_change ──▶ SIGWINCH
//!   try_recv ◀─ 事件队列 ◀─────── channel.wait ◀─────────── stdout/stderr
//! ```
//!
//! 请求队列用 tokio 的（异步侧要 `.await` 发送），事件队列用标准库的
//! （同步侧要 `try_recv`）。方向不同，选型也不同。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver as StdReceiver, TryRecvError};
use std::time::Duration;

use daisy_term::{LinkEvent, SessionLink, TERM_VALUE, TermError, Waker, WakerSlot};
use russh::ChannelMsg;
use tokio::sync::mpsc::{self as tokio_mpsc, UnboundedSender};

use crate::error::{Result, SshError};
use crate::session::SshSession;

/// 异步边界上的错误翻译。
///
/// `SessionLink` 属于终端层，只能返回 [`TermError`]。这里做一次有损但有用的压缩：
///
/// - 链路已关闭 → [`TermError::SessionClosed`]，终端层据此停止重试
/// - 其余 → [`TermError::Terminal`]，**原文保留在消息里**，不丢诊断信息
impl From<SshError> for TermError {
	fn from(error: SshError) -> Self {
		match error {
			SshError::Disconnected { .. } => Self::SessionClosed,
			other => Self::Terminal(other.to_string()),
		}
	}
}

/// 界面线程发给后台任务的请求。
#[derive(Debug)]
pub enum ShellRequest {
	/// 把这段字节写进远端 stdin。
	Data(Vec<u8>),
	/// 通知远端窗口尺寸变化。
	Resize {
		/// 列数。
		cols: u16,
		/// 行数。
		rows: u16,
	},
	/// 结束通道。
	Close,
}

/// 后台任务上报给界面的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
	/// 远端输出（stdout 与 stderr 已合并）。
	Data(Vec<u8>),
	/// 远端进程退出，附带退出码。
	Exit(Option<i32>),
	/// 通道或连接已结束。
	Closed,
}

/// 一条远端 shell 通道。
pub struct RemoteShell {
	/// 界面 → 后台。
	requests: UnboundedSender<ShellRequest>,
	/// 后台 → 界面。
	events: StdReceiver<ShellEvent>,
	/// 「有新事件」的唤醒回调槽，与后台任务共享。
	waker: WakerSlot,
	/// 对端是否已关闭。
	closed: Arc<AtomicBool>,
	/// 是否已经把「结束」上报给调用方。
	///
	/// **只由 [`SessionLink::try_recv`] 读写。** 后台任务不得触碰它——
	/// 否则任务里的 `store` 会让这里的 `swap` 看到「已经上报过」而把事件丢掉。
	/// （这个 bug 真发生过，见 `remote_exit_code_is_reported_once`。）
	exit_reported: bool,
	cols: u16,
	rows: u16,
}

impl RemoteShell {
	/// 在既有会话上开一条交互式 shell 通道。
	///
	/// 依次做三件事：开会话通道 → 申请 PTY → 启动 shell。
	/// 三步都带 `want_reply`，服务端不支持时会在读取侧表现为通道异常结束。
	pub async fn open(session: &SshSession, cols: u16, rows: u16) -> Result<Self> {
		let channel = session
			.handle()
			.channel_open_session()
			.await
			.map_err(|error| SshError::Channel(format!("打开会话通道失败：{error}")))?;

		let (read_half, write_half) = channel.split();

		// 申请 PTY。pix_width/pix_height 填 0：远端绝大多数程序只看字符尺寸，
		// 而像素尺寸涉及两端 DPI，属于 S9 的显示层要解决的问题。
		write_half
			.request_pty(true, TERM_VALUE, cols as u32, rows as u32, 0, 0, &[])
			.await
			.map_err(|error| SshError::Channel(format!("申请远端 PTY 失败：{error}")))?;

		write_half
			.request_shell(true)
			.await
			.map_err(|error| SshError::Channel(format!("请求远端 shell 失败：{error}")))?;

		Self::spawn_pump(read_half, write_half, cols, rows)
	}

	/// 在既有会话上执行一条命令并取回输出（不申请 PTY，不走登录 shell）。
	///
	/// 与 [`Self::open`] 的区别：
	///
	/// | | `open` | `exec` |
	/// | --- | --- | --- |
	/// | 通道类型 | `shell` + PTY | `exec` |
	/// | 输出 | 带 ANSI 转义、需过终端网格 | 原始字节 |
	/// | 用途 | 交互式会话 | 一次性命令、脚本化运维 |
	///
	/// 因为不走登录 shell，`PATH` / profile 不会加载——这是 SSH 的既有语义，
	/// 不是 Daisy 的选择。需要登录环境时用 [`Self::open`]。
	pub async fn exec(session: &SshSession, command: &str) -> Result<Self> {
		let channel = session
			.handle()
			.channel_open_session()
			.await
			.map_err(|error| SshError::Channel(format!("打开会话通道失败：{error}")))?;

		let (read_half, write_half) = channel.split();

		write_half
			.exec(true, command.as_bytes())
			.await
			.map_err(|error| SshError::Channel(format!("请求执行远端命令失败：{error}")))?;

		// exec 没有窗口尺寸可言，填 0 表示「不适用」
		Self::spawn_pump(read_half, write_half, 0, 0)
	}

	/// 起后台任务接管通道的双向搬运。
	fn spawn_pump(
		read_half: russh::ChannelReadHalf,
		write_half: russh::ChannelWriteHalf<russh::client::Msg>,
		cols: u16,
		rows: u16,
	) -> Result<Self> {
		let (request_tx, request_rx) = tokio_mpsc::unbounded_channel::<ShellRequest>();
		let (event_tx, event_rx) = mpsc::channel::<ShellEvent>();
		let closed = Arc::new(AtomicBool::new(false));
		let waker = WakerSlot::default();

		let task_closed = Arc::clone(&closed);
		let task_waker = waker.clone();

		tokio::spawn(async move {
			pump(
				read_half,
				write_half,
				request_rx,
				event_tx,
				task_closed,
				task_waker,
			)
			.await;
		});

		Ok(Self {
			requests: request_tx,
			events: event_rx,
			waker,
			closed,
			exit_reported: false,
			cols,
			rows,
		})
	}

	/// 通道是否已结束。
	#[must_use]
	pub fn is_closed(&self) -> bool {
		self.closed.load(Ordering::Acquire)
	}

	/// 收齐全部输出与退出码后返回。
	///
	/// 用于 [`Self::exec`] 这类一次性命令。注意它**不会**做 ANSI 解析——
	/// 拿到的是远端进程的原始字节。
	///
	/// 超时是必要的：远端脚本卡住时调用方不能无限等下去。
	pub async fn collect(&mut self, timeout: Duration) -> Result<(Vec<u8>, Option<i32>)> {
		let deadline = tokio::time::Instant::now() + timeout;
		let mut output = Vec::new();
		let mut code = None;

		loop {
			while let Some(event) = self.try_recv() {
				match event {
					LinkEvent::Output(bytes) => output.extend_from_slice(&bytes),
					LinkEvent::Exited(status) => code = Some(status),
				}
			}

			// 拿到退出码，或通道已经结束，都可以收工
			if code.is_some() || self.is_closed() {
				return Ok((output, code));
			}

			if tokio::time::Instant::now() >= deadline {
				return Err(SshError::Channel(format!(
					"等待远端命令结束超时（{} 秒）",
					timeout.as_secs()
				)));
			}

			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	}

	/// 当前尺寸。
	#[must_use]
	pub fn size(&self) -> (u16, u16) {
		(self.cols, self.rows)
	}

	fn send(&self, request: ShellRequest) -> Result<()> {
		self.requests
			.send(request)
			.map_err(|_| SshError::Channel("远端通道的后台任务已结束".to_string()))
	}
}

impl SessionLink for RemoteShell {
	fn try_recv(&mut self) -> Option<LinkEvent> {
		loop {
			match self.events.try_recv() {
				Ok(ShellEvent::Data(bytes)) => return Some(LinkEvent::Output(bytes)),

				Ok(ShellEvent::Exit(code)) => {
					if self.exit_reported {
						continue;
					}
					self.exit_reported = true;
					return Some(LinkEvent::Exited(code.unwrap_or(-1)));
				},

				Ok(ShellEvent::Closed) => {
					self.closed.store(true, Ordering::Release);
					if self.exit_reported {
						// 退出码已经报过了，这里不再重复
						return None;
					}
					self.exit_reported = true;
					// 通道断了但没收到退出状态，用 -1 表示「非正常结束」
					return Some(LinkEvent::Exited(-1));
				},

				Err(TryRecvError::Empty) => return None,
				Err(TryRecvError::Disconnected) => {
					self.closed.store(true, Ordering::Release);
					return None;
				},
			}
		}
	}

	fn write(&mut self, data: &[u8]) -> daisy_term::Result<()> {
		if self.is_closed() {
			return Err(SshError::Disconnected {
				reason: "通道已关闭".to_string(),
			}
			.into());
		}
		self.send(ShellRequest::Data(data.to_vec()))?;
		Ok(())
	}

	fn resize(&mut self, cols: u16, rows: u16) -> daisy_term::Result<()> {
		if (cols, rows) == (self.cols, self.rows) {
			return Ok(());
		}
		self.send(ShellRequest::Resize { cols, rows })?;
		self.cols = cols;
		self.rows = rows;
		Ok(())
	}

	fn shutdown(&mut self) -> daisy_term::Result<bool> {
		if self.is_closed() {
			return Ok(false);
		}
		self.closed.store(true, Ordering::Release);
		// 后台任务已退出时不算失败——要关的东西本来就没了
		let _ = self.requests.send(ShellRequest::Close);
		Ok(true)
	}

	fn set_waker(&mut self, waker: Waker) {
		self.waker.set(waker);
	}
}

impl std::fmt::Debug for RemoteShell {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("RemoteShell")
			.field("cols", &self.cols)
			.field("rows", &self.rows)
			.field("closed", &self.is_closed())
			.finish_non_exhaustive()
	}
}

impl Drop for RemoteShell {
	fn drop(&mut self) {
		// 上层丢弃 shell 时，让后台任务把通道收干净，不留半开的会话
		let _ = self.requests.send(ShellRequest::Close);
	}
}

/// 后台任务：双向搬运，直到任一侧结束。
///
/// **不参与「是否已上报退出」的判断**——那是 [`RemoteShell::try_recv`] 的事。
/// 这里只负责把远端消息如实变成事件。
async fn pump(
	mut read_half: russh::ChannelReadHalf,
	write_half: russh::ChannelWriteHalf<russh::client::Msg>,
	mut requests: tokio_mpsc::UnboundedReceiver<ShellRequest>,
	events: mpsc::Sender<ShellEvent>,
	closed: Arc<AtomicBool>,
	waker: WakerSlot,
) {
	loop {
		tokio::select! {
			// 远端 → 界面
			message = read_half.wait() => {
				match message {
					None => {
						tracing::debug!("通道消息流结束");
						break;
					},
					Some(ChannelMsg::Data { data }) => {
						if events.send(ShellEvent::Data(data.to_vec())).is_err() {
							break;
						}
						// 送完就叫醒界面。少了这一步，输出只能等下一次周期性重绘
						// （光标闪烁 500ms 一次）才上屏，远端刷屏看起来像卡住。
						waker.wake();
					},
					// ext == 1 是 stderr。终端要的是合并流，直接拼在一起。
					Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
						if events.send(ShellEvent::Data(data.to_vec())).is_err() {
							break;
						}
						waker.wake();
					},
					Some(ChannelMsg::ExitStatus { exit_status }) => {
						tracing::debug!("远端退出状态：{exit_status}");
						if events
							.send(ShellEvent::Exit(Some(exit_status as i32)))
							.is_err()
						{
							break;
						}
					},
					Some(ChannelMsg::ExitSignal { .. }) => {
						tracing::debug!("远端以信号结束，无退出状态");
						if events.send(ShellEvent::Exit(None)).is_err() {
							break;
						}
					},
					// ⚠️ **EOF 不能当作结束**。
					//
					// 实测真实 sshd（OpenSSH 9.9）在交互式 session 上的发送顺序是：
					//
					//     EOF → exit-status → CLOSE
					//
					// 也就是 **exit-status 在 EOF 之后**。EOF 的语义只是
					// 「对端不会再发数据」，通道本身仍然开着——
					// 若在这里 break，退出码会被整个丢掉（实测拿到 -1）。
					//
					// 真正的结束信号是 CLOSE，或消息流返回 None。
					Some(ChannelMsg::Eof) => {
						tracing::debug!("远端发送 EOF（通道仍开着，继续等退出状态）");
					},
					Some(ChannelMsg::Close) => {
						tracing::debug!("远端关闭通道");
						break;
					},
					// 其它消息（窗口调整、成功/失败应答等）不需要转给界面
					Some(other) => {
						tracing::trace!("忽略通道消息：{other:?}");
					},
				}
			}

			// 界面 → 远端
			request = requests.recv() => {
				match request {
					None => {
						// 上层已经丢弃 shell，把远端也收掉
						let _ = write_half.eof().await;
						break;
					},
					Some(ShellRequest::Data(bytes)) => {
						if let Err(error) = write_half.data_bytes(bytes).await {
							// 不 break：对端可能已经 EOF，但我们仍要留在原地
							// 把紧随其后的 exit-status 收下来
							tracing::debug!("向远端写入失败：{error}");
						}
					},
					Some(ShellRequest::Resize { cols, rows }) => {
						if let Err(error) =
							write_half.window_change(cols as u32, rows as u32, 0, 0).await
						{
							tracing::debug!("上报远端窗口尺寸失败：{error}");
						}
					},
					Some(ShellRequest::Close) => {
						let _ = write_half.eof().await;
						let _ = write_half.close().await;
						break;
					},
				}
			}
		}
	}

	closed.store(true, Ordering::Release);
	tracing::debug!("通道后台任务结束");
	// 界面已经丢弃时这里会失败，属正常收尾
	let _ = events.send(ShellEvent::Closed);
	// 收尾也要叫一次：退出码/断连提示都靠这一下才上屏
	waker.wake();
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn shell_request_is_debug_printable() {
		let request = ShellRequest::Resize { cols: 80, rows: 24 };
		assert!(format!("{request:?}").contains("80"));
	}

	#[test]
	fn shell_events_compare_by_value() {
		assert_eq!(ShellEvent::Exit(Some(0)), ShellEvent::Exit(Some(0)));
		assert_ne!(ShellEvent::Exit(Some(0)), ShellEvent::Closed);
		assert_eq!(ShellEvent::Data(vec![1, 2]), ShellEvent::Data(vec![1, 2]));
	}
}
