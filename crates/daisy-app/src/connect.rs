//! SSH 连接的异步桥。
//!
//! 界面是同步的（egui 每帧拉一次），SSH 是异步的（tokio）。这里用一个
//! 长生命周期的 runtime 把「建连 + 开远端 shell」跑在后台，结果经
//! `std::sync::mpsc` 回传——与 `daisy-term` 的 `SessionLink` 同一思路：
//! **异步性由实现方消化，界面侧只做非阻塞轮询**。
//!
//! ```text
//!   界面：open_ssh() ──▶ Connector::request() ──▶ tokio 任务
//!                                                    │ connect()
//!                                                    │ RemoteShell::open()
//!   界面：poll() ◀── mpsc ◀── ConnectOutcome ◀───────┘
//! ```

use std::sync::mpsc::{Receiver, Sender, TryIter};
use std::time::Duration;

use daisy_ssh::{AuthSpec, HostKeyPolicy, RemoteShell, SshParams, SshSession, SshTarget};
use tokio::runtime::Runtime;

/// 一条已建立的 SSH 连接。
///
/// `_session` **必须留着**：它持有 russh 的 `Handle`，drop 掉会直接断连。
/// 下划线前缀只为消除「从未读取」警告，不代表可以删。
pub struct SshConnection {
	_session: SshSession,
	/// 远端 shell 通道，实现 `daisy_term::SessionLink`。
	pub shell: RemoteShell,
}

impl SshConnection {
	/// 主机密钥核对结论。首次连上的主机是 [`HostKeyVerdict::Unknown`]。
	#[must_use]
	pub fn host_key_verdict(&self) -> daisy_ssh::HostKeyVerdict {
		self._session.host_key_verdict()
	}

	/// 观测到的主机密钥（算法名 + 指纹 + base64）。
	#[must_use]
	pub fn host_key(&self) -> &daisy_ssh::HostKeyInfo {
		self._session.host_key()
	}
}

/// 一次连接请求的结局。
///
/// `Connected` 装箱：会话本体不小，不装箱会让整个枚举变大，
/// 而失败分支才是高频路径（每次只搬一个小 `String`）。
pub enum ConnectOutcome {
	/// 连上了，附会话本体。
	Connected {
		/// 发起时填的请求号，用于把结果对回标签页。
		request_id: u64,
		/// 会话。
		conn: Box<SshConnection>,
	},
	/// 没连上。文案已按 `SshError` 分类整理成可操作形式。
	Failed {
		/// 发起时填的请求号。
		request_id: u64,
		/// 给人看的错误说明。
		error: String,
	},
}

/// 经跳板机连接的完整描述。
///
/// 收拢成一个结构体——四个参数各自属于不同层（跳板机 / 目标 / 认证 / 策略），
/// 平铺成函数参数既难读又容易搞乱顺序。
pub struct JumpSpec {
	/// 跳板机自己的连接参数。
	pub jump: SshParams,
	/// 真正要连的目标。
	pub target: SshTarget,
	/// 目标的认证方式。
	pub auth: AuthSpec,
	/// 目标的主机密钥策略。必须显式携带，否则跳板机路径的
	/// known_hosts 核对会静默失效。
	pub host_key: HostKeyPolicy,
}

/// 连接器。持有 runtime 与回传通道，生命周期与整个应用一致。
pub struct Connector {
	runtime: Runtime,
	inbox: Receiver<ConnectOutcome>,
	outbox: Sender<ConnectOutcome>,
}

/// 连通性探测的超时时长。
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 一次 TCP 探测的结论。
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
	/// 端口是否可达。**成败要单独给一个布尔**：靠文案里有没有「失败」两个字
	/// 去猜，改一次措辞界面上的颜色就错了。
	pub reachable: bool,
	/// 失败原因（成功时为空）。已经翻成中文、并且**不带地址前缀**——
	/// 地址就写在表单上一行，重复一遍只会把真正的原因挤到看不见。
	pub detail: String,
}

impl Connector {
	/// 创建连接器。runtime 只用 1 个工作线程——连接是偶发操作，
	/// 会话建立后的字节搬运由 `RemoteShell` 自己的任务负责，不占这里的线程。
	pub fn new() -> Self {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(1)
			.thread_name("daisy-connect")
			.enable_all()
			.build()
			.expect("创建 tokio runtime 失败");
		let (outbox, inbox) = std::sync::mpsc::channel();
		Self {
			runtime,
			inbox,
			outbox,
		}
	}

	/// 发起一次连接。立即返回，结果经 [`Connector::poll`] 取。
	///
	/// `cols` / `rows` 只是首帧网格尺寸——界面量出真实区域后会 resize 校正。
	pub fn request(&self, request_id: u64, params: SshParams, cols: u16, rows: u16) {
		let tx = self.outbox.clone();
		self.runtime.spawn(async move {
			tracing::debug!("连接请求 {request_id}：开始建立");
			match establish(params, cols, rows).await {
				Ok(conn) => {
					tracing::debug!("连接请求 {request_id}：成功");
					let _ = tx.send(ConnectOutcome::Connected {
						request_id,
						conn: Box::new(conn),
					});
				},
				Err(error) => {
					tracing::debug!("连接请求 {request_id}：失败（{error}）");
					let _ = tx.send(ConnectOutcome::Failed {
						request_id,
						error: error.to_string(),
					});
				},
			}
		});
	}

	/// 经跳板机发起连接（步骤 55 的 UI 接线）。
	///
	/// 与 [`Connector::request`] 的差别只在建立方式：先连跳板机，
	/// 再在其上开 `direct-tcpip` 通道到真正目标。
	pub fn request_jump(&self, request_id: u64, spec: JumpSpec, cols: u16, rows: u16) {
		let tx = self.outbox.clone();
		self.runtime.spawn(async move {
			tracing::debug!("跳板连接请求 {request_id}：开始建立");
			match establish_via_jump(spec, cols, rows).await {
				Ok(conn) => {
					tracing::debug!("跳板连接请求 {request_id}：成功");
					let _ = tx.send(ConnectOutcome::Connected {
						request_id,
						conn: Box::new(conn),
					});
				},
				Err(error) => {
					tracing::debug!("跳板连接请求 {request_id}：失败（{error}）");
					let _ = tx.send(ConnectOutcome::Failed {
						request_id,
						error: error.to_string(),
					});
				},
			}
		});
	}

	/// 取走所有已到达的结果。非阻塞。
	pub fn poll(&mut self) -> TryIter<'_, ConnectOutcome> {
		self.inbox.try_iter()
	}

	/// 探测一个 TCP 端点是否可达（连通性测试按钮用）。
	///
	/// 返回一条新接收端——每次探测独立通道，UI 轮询它即可。
	/// 跑在 runtime 里，不阻塞界面；解析失败与被拒分别报告，
	/// 因为这三件事的排查方向完全不同。
	pub fn probe_tcp(&self, host: String, port: u16) -> Receiver<ProbeOutcome> {
		let (tx, rx) = std::sync::mpsc::channel();
		self.runtime.spawn(async move {
			let outcome = match tokio::time::timeout(
				PROBE_TIMEOUT,
				tokio::net::TcpStream::connect((host.as_str(), port)),
			)
			.await
			{
				Err(_) => ProbeOutcome {
					reachable: false,
					detail: format!("连接超时（{} 秒内没有响应）", PROBE_TIMEOUT.as_secs()),
				},
				Ok(Err(error)) => ProbeOutcome {
					reachable: false,
					detail: probe_reason(&error),
				},
				Ok(Ok(_)) => ProbeOutcome {
					reachable: true,
					detail: String::new(),
				},
			};
			let _ = tx.send(outcome);
		});
		rx
	}
}

/// 把 `io::Error` 翻成一句人能直接照着排查的原因。
///
/// 直接显示 `error` 会把 `Connection refused (os error 61)` 这类原文抖到界面上：
/// 英文、带平台错误码、对不熟悉的人等于没说。「域名解析失败」这种情况系统给的
/// 是 `Other` + 英文长句，只能按关键字认——认不出来就退回原文，至少有点信息量。
fn probe_reason(error: &std::io::Error) -> String {
	use std::io::ErrorKind;
	match error.kind() {
		ErrorKind::ConnectionRefused => "端口未开放（连接被拒绝）".to_string(),
		ErrorKind::TimedOut => "连接超时".to_string(),
		ErrorKind::NetworkUnreachable => "网络不可达".to_string(),
		ErrorKind::HostUnreachable => "主机不可达".to_string(),
		ErrorKind::AddrNotAvailable => "地址不可用（本机没有这个地址）".to_string(),
		_ => {
			let text = error.to_string();
			if text.contains("lookup address") || text.contains("Name or service not known") {
				"域名解析失败".to_string()
			} else {
				shorten(&text)
			}
		},
	}
}

/// 截断过长的错误原文。
///
/// 认不出来的系统错误有时是一整句英文解释（"failed to lookup address
/// information: nodename nor servname provided, or not known"），
/// 原样摆进表单会把对话框顶宽。留前 48 个字，够认出是什么毛病。
fn shorten(text: &str) -> String {
	/// 最多保留的字数。
	const MAX_CHARS: usize = 48;
	if text.chars().count() <= MAX_CHARS {
		return text.to_owned();
	}
	let head: String = text.chars().take(MAX_CHARS).collect();
	format!("{head}…")
}

impl std::fmt::Debug for Connector {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Connector").finish_non_exhaustive()
	}
}

/// 建连并开好远端 shell。任何一步失败都整体算失败。
async fn establish(params: SshParams, cols: u16, rows: u16) -> daisy_ssh::Result<SshConnection> {
	let session = daisy_ssh::connect(params).await?;
	let shell = RemoteShell::open(&session, cols, rows).await?;
	Ok(SshConnection {
		_session: session,
		shell,
	})
}

/// 经跳板机建连并开好远端 shell。
///
/// `connect_via_jump` 内部先连跳板机、再在其上开 `direct-tcpip` 通道，
/// 返回的会话已经是「目标」的会话——后面的 `RemoteShell::open` 与直连无异。
async fn establish_via_jump(
	spec: JumpSpec,
	cols: u16,
	rows: u16,
) -> daisy_ssh::Result<SshConnection> {
	let session =
		daisy_ssh::connect_via_jump(spec.jump, spec.target, spec.auth, spec.host_key).await?;
	let shell = RemoteShell::open(&session, cols, rows).await?;
	Ok(SshConnection {
		_session: session,
		shell,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 常见失败要翻成人话，别把 `os error 61` 抖到界面上。
	#[test]
	fn probe_reasons_are_readable() {
		use std::io::{Error, ErrorKind};
		assert_eq!(
			probe_reason(&Error::from(ErrorKind::ConnectionRefused)),
			"端口未开放（连接被拒绝）"
		);
		assert_eq!(
			probe_reason(&Error::from(ErrorKind::NetworkUnreachable)),
			"网络不可达"
		);
	}

	/// 认不出来的错误照原文给，但必须截断——否则一行英文解释会把对话框顶宽。
	#[test]
	fn unknown_errors_are_shortened() {
		let long =
			"failed to lookup address information: nodename nor servname provided, or not known";
		let reason = probe_reason(&std::io::Error::other(long));
		assert_eq!(reason, "域名解析失败");

		let odd = "some very unusual transport failure that goes on and on and on and on forever";
		let reason = probe_reason(&std::io::Error::other(odd));
		assert!(
			reason.chars().count() <= 49,
			"没截断：{} 字",
			reason.chars().count()
		);
		assert!(reason.ends_with('…'), "截断后要有省略号：{reason}");

		// 短的原样返回，不加省略号
		assert_eq!(shorten("一点小事"), "一点小事");
	}
}
