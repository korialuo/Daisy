//! SFTP 后台会话：把同步的界面接到异步的 SSH 上。
//!
//! ## 为什么单独一条连接
//!
//! 终端的 SSH 连接被 `RemoteShell` 独占（它自己 spawn 了收发任务），
//! 再在上面开一个 sftp 通道要和那条任务的窗口管理纠缠。SFTP 只是偶尔用一次，
//! 单独连一条最干净：用完就关，不影响终端。
//!
//! ## 线程模型
//!
//! ```text
//!   界面线程                    后台线程（current_thread runtime）
//!   ────────                    ──────────────────────────────
//!   list/download ─▶ 命令队列 ─▶ SftpClient ──▶ 远端
//!   try_recv     ◀─ 事件队列 ◀─ 结果 / 错误 / 进度
//! ```
//!
//! 请求队列用 tokio 的（后台要 `.await` 收），事件队列用标准库的
//! （界面要 `try_recv` 不阻塞）——与 `RemoteShell` 同一套取舍。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use daisy_ssh::sftp::{DirEntry, SftpClient};
use daisy_ssh::{SshParams, connect};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// 界面发给后台的操作。
#[derive(Debug)]
pub enum SftpRequest {
	/// 列目录。
	List(String),
	/// 下载远端文件到本地路径。
	Download {
		/// 远端文件。
		remote: String,
		/// 本地落盘位置。
		local: PathBuf,
	},
	/// 上传本地文件到远端路径。
	Upload {
		/// 本地文件。
		local: PathBuf,
		/// 远端完整路径（含文件名）。
		remote: String,
	},
	/// 删除文件或空目录。
	Delete {
		/// 目标路径。
		path: String,
		/// 是不是目录（决定用 rmdir 还是 remove）。
		is_dir: bool,
	},
	/// 远端内部复制。
	Copy {
		/// 源。
		from: String,
		/// 目标。
		to: String,
	},
	/// 结束会话。
	Close,
}

/// 后台报给界面的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SftpEvent {
	/// 连上了，`home` 是登录用户的家目录。
	Connected {
		/// 家目录绝对路径。
		home: String,
	},
	/// 目录内容。
	Listed {
		/// 目录路径。
		path: String,
		/// 目录项（已排序）。
		entries: Vec<DirEntry>,
	},
	/// 忙碌状态：`Some` 表示开始（带一句正在做什么），`None` 表示结束。
	Busy(Option<String>),
	/// 一句成功提示。
	Info(String),
	/// 一句错误提示。
	Error(String),
	/// 连接已结束，附带原因。
	Closed(String),
}

/// 一条 SFTP 会话。
pub struct SftpSession {
	/// 界面 → 后台。
	requests: UnboundedSender<SftpRequest>,
	/// 后台 → 界面。
	events: Receiver<SftpEvent>,
	/// 会话是否已经结束（避免重复关闭）。
	closed: bool,
}

impl SftpSession {
	/// 起一条会话。连接在后台进行，界面先显示「连接中」。
	///
	/// `params` 与终端会话用的是同一份（含代理 dialer 与主机密钥策略）——
	/// SFTP 不该因为「换了个功能」而绕开主机密钥校验。
	pub fn spawn(params: SshParams) -> Self {
		let (request_tx, mut request_rx) = unbounded_channel::<SftpRequest>();
		let (event_tx, event_rx) = mpsc::channel::<SftpEvent>();

		thread::Builder::new()
			.name("daisy-sftp".to_string())
			.spawn(move || {
				let runtime = match tokio::runtime::Builder::new_current_thread()
					.enable_all()
					.build()
				{
					Ok(runtime) => runtime,
					Err(error) => {
						let _ = event_tx
							.send(SftpEvent::Closed(format!("无法启动 SFTP 运行时：{error}")));
						return;
					},
				};

				runtime.block_on(async move {
					let session = match connect(params).await {
						Ok(session) => session,
						Err(error) => {
							tracing::warn!("[SFTP] 连接失败：{error}");
							let _ = event_tx.send(SftpEvent::Error(error.to_string()));
							let _ = event_tx.send(SftpEvent::Closed("连接失败".to_string()));
							return;
						},
					};
					tracing::info!("[SFTP] SSH 已连接");

					let mut client = match SftpClient::open(&session).await {
						Ok(client) => client,
						Err(error) => {
							tracing::warn!("[SFTP] 打开子系统失败：{error}");
							let _ = event_tx.send(SftpEvent::Error(error.to_string()));
							let _ = event_tx.send(SftpEvent::Closed("无法打开 SFTP".to_string()));
							return;
						},
					};

					// 家目录：登录后落在哪里是用户的预期起点
					let home = match client.realpath("").await {
						Ok(home) => home,
						Err(error) => {
							tracing::warn!("[SFTP] realpath 失败：{error}");
							"/".to_string()
						},
					};
					let _ = event_tx.send(SftpEvent::Connected { home: home.clone() });
					let _ = event_tx.send(SftpEvent::Busy(None));
					if let Ok(entries) = client.list_dir(&home).await {
						let _ = event_tx.send(SftpEvent::Listed {
							path: home,
							entries,
						});
					}

					while let Some(request) = request_rx.recv().await {
						if matches!(request, SftpRequest::Close) {
							break;
						}
						handle(&mut client, request, &event_tx).await;
					}

					let _ = session.disconnect().await;
					let _ = event_tx.send(SftpEvent::Closed("会话已关闭".to_string()));
					tracing::info!("[SFTP] 会话结束");
				});
			})
			.ok();

		Self {
			requests: request_tx,
			events: event_rx,
			closed: false,
		}
	}

	/// 列一个目录。返回 `false` 表示会话已经不在了。
	pub fn list(&self, path: impl Into<String>) -> bool {
		self.send(SftpRequest::List(path.into()))
	}

	/// 下载。
	pub fn download(&self, remote: impl Into<String>, local: impl Into<PathBuf>) -> bool {
		self.send(SftpRequest::Download {
			remote: remote.into(),
			local: local.into(),
		})
	}

	/// 上传。
	pub fn upload(&self, local: impl Into<PathBuf>, remote: impl Into<String>) -> bool {
		self.send(SftpRequest::Upload {
			local: local.into(),
			remote: remote.into(),
		})
	}

	/// 删除。
	pub fn delete(&self, path: impl Into<String>, is_dir: bool) -> bool {
		self.send(SftpRequest::Delete {
			path: path.into(),
			is_dir,
		})
	}

	/// 远端复制。
	pub fn copy(&self, from: impl Into<String>, to: impl Into<String>) -> bool {
		self.send(SftpRequest::Copy {
			from: from.into(),
			to: to.into(),
		})
	}

	fn send(&self, request: SftpRequest) -> bool {
		if self.closed {
			return false;
		}
		self.requests.send(request).is_ok()
	}

	/// 取走本帧的全部事件。会话已关闭时返回的迭代器里带一条 `Closed`。
	pub fn drain(&mut self) -> Vec<SftpEvent> {
		let mut out = Vec::new();
		loop {
			match self.events.try_recv() {
				Ok(event) => {
					if matches!(event, SftpEvent::Closed(_)) {
						self.closed = true;
					}
					out.push(event);
				},
				Err(TryRecvError::Empty) => break,
				Err(TryRecvError::Disconnected) => {
					if !self.closed {
						self.closed = true;
						out.push(SftpEvent::Closed("后台线程已退出".to_string()));
					}
					break;
				},
			}
		}
		out
	}

	/// 主动关闭。
	pub fn close(&mut self) {
		if self.closed {
			return;
		}
		self.closed = true;
		let _ = self.requests.send(SftpRequest::Close);
	}
}

/// 执行一个请求，把结果翻译成事件。
async fn handle(client: &mut SftpClient, request: SftpRequest, events: &mpsc::Sender<SftpEvent>) {
	let send = |event| {
		let _ = events.send(event);
	};

	match request {
		SftpRequest::List(path) => {
			send(SftpEvent::Busy(Some("读取目录…".to_string())));
			match client.list_dir(&path).await {
				Ok(entries) => send(SftpEvent::Listed { path, entries }),
				Err(error) => send(SftpEvent::Error(format!("读取 {path} 失败：{error}"))),
			}
			send(SftpEvent::Busy(None));
		},

		SftpRequest::Download { remote, local } => {
			let name = base_name(&remote).to_string();
			send(SftpEvent::Busy(Some(format!("下载 {name}…"))));
			match client.download(&remote, &local).await {
				Ok(bytes) => send(SftpEvent::Info(format!(
					"已下载 {name}（{}）到 {}",
					human_size(bytes),
					local.display()
				))),
				Err(error) => send(SftpEvent::Error(format!("下载 {name} 失败：{error}"))),
			}
			send(SftpEvent::Busy(None));
		},

		SftpRequest::Upload { local, remote } => {
			let name = local
				.file_name()
				.map(|n| n.to_string_lossy().into_owned())
				.unwrap_or_else(|| remote.clone());
			send(SftpEvent::Busy(Some(format!("上传 {name}…"))));
			match client.upload(&local, &remote).await {
				Ok(bytes) => send(SftpEvent::Info(format!(
					"已上传 {name}（{}）",
					human_size(bytes)
				))),
				Err(error) => send(SftpEvent::Error(format!("上传 {name} 失败：{error}"))),
			}
			send(SftpEvent::Busy(None));
		},

		SftpRequest::Delete { path, is_dir } => {
			let name = base_name(&path).to_string();
			let result = if is_dir {
				client.remove_dir(&path).await
			} else {
				client.remove_file(&path).await
			};
			match result {
				Ok(()) => send(SftpEvent::Info(format!("已删除 {name}"))),
				Err(error) => {
					// 「目录非空」是最常见的一种，单独说清楚
					let text = error.to_string();
					if is_dir && text.contains("Failure") {
						send(SftpEvent::Error(format!("{name} 不是空目录，无法删除")));
					} else {
						send(SftpEvent::Error(format!("删除 {name} 失败：{text}")));
					}
				},
			}
		},

		SftpRequest::Copy { from, to } => {
			let name = base_name(&to).to_string();
			send(SftpEvent::Busy(Some(format!("复制到 {name}…"))));
			match client.copy_file(&from, &to).await {
				Ok(bytes) => send(SftpEvent::Info(format!(
					"已复制到 {name}（{}）",
					human_size(bytes)
				))),
				Err(error) => send(SftpEvent::Error(format!("复制失败：{error}"))),
			}
			send(SftpEvent::Busy(None));
		},

		SftpRequest::Close => {},
	}
}

/// 路径最后一段。
///
/// 根路径没有最后一段，返回它自己（`"/"`）——否则界面上会出现一行
/// 「有箭头但没名字」的树节点。
#[must_use]
pub fn base_name(path: &str) -> &str {
	let trimmed = path.trim_end_matches('/');
	if trimmed.is_empty() {
		return "/";
	}
	trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// 人类可读的大小。
#[must_use]
pub fn human_size(bytes: u64) -> String {
	const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
	let mut value = bytes as f64;
	let mut unit = 0;
	while value >= 1024.0 && unit + 1 < UNITS.len() {
		value /= 1024.0;
		unit += 1;
	}
	if unit == 0 {
		format!("{bytes} B")
	} else {
		format!("{value:.1} {}", UNITS[unit])
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn base_name_handles_trailing_slash_and_root() {
		assert_eq!(base_name("/var/log/syslog"), "syslog");
		assert_eq!(base_name("/var/log/"), "log");
		assert_eq!(base_name("/"), "/");
	}

	#[test]
	fn human_size_switches_units() {
		assert_eq!(human_size(512), "512 B");
		assert_eq!(human_size(1024), "1.0 KB");
		assert_eq!(human_size(1536), "1.5 KB");
		assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
	}
}
