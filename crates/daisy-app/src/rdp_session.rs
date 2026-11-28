//! RDP 会话：专用线程持有 `RdpClient`，UI 线程通过通道收发。
//!
//! ## 为什么必须是专用线程
//!
//! `RdpClient` 持有 FreeRDP 上下文（裸指针，**不是 `Send`**），而 FreeRDP 也
//! 要求连接与事件泵都在同一个线程里调用。所以线程由本模块创建、`RdpClient`
//! 一生不离开它；跨线程只传两样东西——**像素数据**与**状态文案**。
//!
//! ## 为什么拆两个通道
//!
//! 帧、光标、状态的生命周期完全不同：
//!
//! - **状态**（已连接 / 失败 / 断开）一条都不能丢，用无界通道；
//! - **帧**是"最新覆盖"语义，积压只会让画面越拖越旧，用容量 1 的通道，
//!   满了直接丢——下一帧马上就到。1080p 一帧 8 MB，无界通道能在一秒内吃掉几百 MB。
//! - **光标形状**也是"最新覆盖"，但**不能丢**：它是状态而不是流。
//!   丢掉一次「缩放 → 箭头」的切换，界面上就会永远停在缩放光标上，
//!   所以走 [`CursorSlot`] 这个 `Mutex` 槽而不是通道。
//!
//! 反方向的指令（断开、输入事件）走第三条通道。输入与断开共用一条队列是
//! 有意的：这样断开一定排在它之前的输入之后被处理，不会出现「已经断开了
//! 还在追发按键」。

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::JoinHandle;

use daisy_rdp::{ConnectionParams, CursorSprite, InputEvent, RdpClient, RgbaFrame, to_rgba};

/// 事件泵单次等待时长（毫秒）。
///
/// 小一点让断开指令响应更及时，代价是空转时多几次系统调用。
const PUMP_WAIT_MS: u32 = 30;

/// 两次推帧之间的最小间隔（约 60fps）。
///
/// 拖动窗口时服务端会连发多个 UPDATE PDU，`EndPaint` 每个 PDU 结束都触发
/// 一次——立刻推就等于把「这一批里的中间状态」画给用户（窗口已到新位置、
/// 旧位置还留着残影）。合并到一拍之后再推，中间态就没人看见。
const FRAME_MIN_INTERVAL_MS: u32 = 16;
/// [`FRAME_MIN_INTERVAL_MS`] 的 `Duration` 形式。
const FRAME_MIN_INTERVAL: std::time::Duration =
	std::time::Duration::from_millis(FRAME_MIN_INTERVAL_MS as u64);

/// 一次粘贴要送进远端剪贴板的内容。
enum PasteContent {
	/// 纯文本。
	Text(String),
	/// 一组本地文件（走 FileGroupDescriptorW / FileContents）。
	Files(Vec<daisy_rdp::LocalFile>),
}

/// 发往 RDP 线程的指令。
enum Command {
	/// 主动断开（界面关标签页或退出）。
	Disconnect,
	/// 注入一次键鼠输入。
	Input(InputEvent),
	/// 把剪贴板内容送进远端剪贴板，并补发 Ctrl+V（S8-63 / S10）。
	///
	/// **必须是同一条命令**：内容走 `pending_out`（要等下一次 pump 才发），
	/// 而按键走当前循环。若分成两条命令，命令队列在 pump 之前消费，
	/// Ctrl+V 会**先于**格式列表到达远端——那时远端剪贴板还是空的，
	/// 粘贴出来是空白。
	Paste {
		/// 要送过去的内容：文本，或一组文件。
		///
		/// **文件不能退化成文本**：Finder 复制文件时剪贴板上同时有纯文本
		/// （文件名），只报文本会把远端的文件格式列表覆盖掉——用户按下
		/// Cmd+V 时远端只剩一段文本，桌面粘贴毫无反应。
		content: PasteContent,
		/// 要不要先上报格式列表。
		///
		/// `false` 用在「这批内容刚刚已经报过」的场景（轮询上报后用户
		/// 才按的 Cmd+V）：**重复上报同一批文件会把 rdpclip 的粘贴流程
		/// 搞坏**——真机对照实测，重报后它变成反复索要描述表、一次不问
		/// 大小，文件永远不落盘；只注入 Ctrl+V 则正常。
		announce: bool,
	},
	/// 请求远端切桌面分辨率（S8-64，经 disp 通道）。
	///
	/// 走命令队列而不是共享变量：`DispClientContext` 不是 `Send`，
	/// 发送必须发生在会话线程里，和输入注入同理。
	Resize {
		/// 目标宽度（像素）。
		width: u32,
		/// 目标高度（像素）。
		height: u32,
	},
}

/// 注入 Ctrl+V 之后，等服务端来取数——**客观判定正向是否落地**。
///
/// 服务端**只在真的粘贴、需要渲染延迟数据时**才发 `CB_FORMAT_DATA_REQUEST`。
/// 收到它就说明两件事：
/// 1. Windows 剪贴板里确实有我们上报的格式（否则没东西可渲染）；
/// 2. 那次 Ctrl+V 落到了能接收文本的窗口上（否则没人去要数据）。
///
/// 反之，超时没收到 = 粘贴没落地。这时**不能再等广播**——
/// 服务端把格式装进自己的剪贴板后不会回播给发起方，等广播永远是空等
/// （第二十五轮的验证逻辑就错在这里，每次都报"没广播"）。
fn verify_forward_paste(client: &RdpClient, pushed: &str) {
	let mut waited = 0u32;
	// 等取数请求。2 秒足够：实测请求在按键后 20ms 内就到。
	while !client.clipboard_remote_took_data() && waited < VERIFY_TIMEOUT_MS {
		if client.pump(20).is_err() {
			return;
		}
		waited += 20;
	}
	if client.clipboard_remote_took_data() {
		tracing::info!(
			"[剪贴板] ✅ 验证通过：服务端在 {waited}ms 内索要了数据（{pushed}）——Windows 剪贴板已拿到内容、粘贴动作已落地"
		);
	} else {
		tracing::warn!(
			"[剪贴板] ❌ 验证超时：{VERIFY_TIMEOUT_MS}ms 内服务端没来取数——粘贴没落到可接收文本的窗口上，或 Windows 剪贴板没装上我们的格式"
		);
	}
}

/// 注入 Ctrl+V 之前，最多等格式列表回执多久。
///
/// 这是**上限**不是固定等待：实测回执只要 5ms，
/// 到达上限还没确认就只能照常注入（否则用户会觉得卡）。
const ACK_WAIT_MAX_MS: u32 = 2000;
/// 轮询间隔。`pump` 是阻塞的，得分段才好中途退出。
const PASTE_POLL_MS: u32 = 20;
/// 组合键每步之间的间隔（毫秒）。
///
/// 组合键对时序敏感：Ctrl 按下与 V 按下挨得太近，RDP 输入通道
/// 可能当成同一批处理，远端就只看到 V（用户实测：Ctrl 被丢掉、粘出字母 v）。
/// 30ms 与真实键盘的间隔同量级。
const KEY_GAP_MS: u32 = 30;

/// 自我验证：等服务端广播回来的上限。实测往返只要几毫秒。
const VERIFY_TIMEOUT_MS: u32 = 2000;

/// Ctrl+V 的按键序列（会话线程内直接注入）。
fn paste_keys() -> [InputEvent; 4] {
	[
		InputEvent::Key {
			key: daisy_rdp::Key::ControlLeft,
			down: true,
			repeat: false,
		},
		InputEvent::Key {
			key: daisy_rdp::Key::V,
			down: true,
			repeat: false,
		},
		InputEvent::Key {
			key: daisy_rdp::Key::V,
			down: false,
			repeat: false,
		},
		InputEvent::Key {
			key: daisy_rdp::Key::ControlLeft,
			down: false,
			repeat: false,
		},
	]
}

/// 注入完 Ctrl+V 之后，如果**物理** Ctrl 还按着，补一次按下。
///
/// 注入序列的最后一步是 `Ctrl↑`，它会让远端认为 Ctrl 已抬起；
/// 但用户是按着 Ctrl 敲的 V，物理上仍按着。不补这一步，接着按 Z
/// 远端收到的就是裸 `z`，而不是 `Ctrl+Z`——按着修饰键的下一个动作直接失效。
///
/// 补发之后，等用户真的松手，物理 `Ctrl↑` 到来，远端状态自然配对。
fn restore_ctrl_after_paste(held: Option<daisy_rdp::Key>) -> Option<InputEvent> {
	held.map(|key| InputEvent::Key {
		key,
		down: true,
		repeat: false,
	})
}

/// RDP 线程回报的状态事件。帧不走这里，见模块文档。
#[derive(Debug, Clone)]
pub enum RdpStatus {
	/// 已连上，`width`/`height` 是当前桌面尺寸。
	Connected {
		/// 桌面宽度（像素）。
		width: u32,
		/// 桌面高度（像素）。
		height: u32,
	},
	/// 连接失败或中途断开，文案直接展示。
	Ended(String),
	/// disp 通道已连上，可以调分辨率了（S8-64）。
	///
	/// 通道连接晚于 `Connected`——服务端要先接受这个动态通道。
	/// 界面在此之前不点亮分辨率控件，点下去只会得到一个「还没连上」。
	DisplayReady,
	/// 分辨率调整失败（通道没连上、服务端不认、参数越界）。文案直接展示。
	ResizeFailed(String),
	/// 首次见到这台机器的证书（S8-65）：指纹待上层写库。
	///
	/// **写库不在会话线程做**——那要拿 `Database`（SQLCipher 连接不是 `Send`），
	/// 也不该让一次证书记录拖慢连接。会话线程只把事实报上去，落库交给界面层。
	CertificateFirstSeen {
		/// 主机地址。
		host: String,
		/// 端口。
		port: u16,
		/// 规范化指纹（小写、无分隔符）。
		fingerprint: String,
		/// 证书使用者。
		subject: Option<String>,
		/// 签发者。
		issuer: Option<String>,
	},
}

/// 一条 RDP 会话。
pub struct RdpSession {
	/// 给线程下指令。
	commands: Sender<Command>,
	/// 状态事件（不丢）。
	statuses: Receiver<RdpStatus>,
	/// 最新帧（容量 1，满了丢旧的——由线程侧 `try_send` 决定）。
	frames: Receiver<(u64, RgbaFrame)>,
	/// 最新光标位图（覆盖式槽，见 [`CursorSlot`]）。
	cursors: Arc<CursorSlot>,
	/// 剪贴板通道（S8-63）。装不装得上由服务端决定，失败不影响会话。
	///
	/// 与光标同理：界面要能随时读，而会话线程要能随时写，所以共享一份。
	clipboard: Arc<daisy_rdp::Clipboard>,
	/// 服务端上报的光标坐标（x 占高 32 位、y 占低 32 位）。
	///
	/// 用原子而不是通道：位置每帧都可能变，而界面本来就每帧画一次，
	/// 走通道会多一次无谓的排队与同步。
	/// 线程句柄。`Drop` 时 join，确保 FreeRDP 上下文在正确的线程里释放。
	join: Option<JoinHandle<()>>,
}

/// 光标位图的跨线程槽：**只留最新一张，旧的被新来的直接覆盖**。
///
/// **为什么不能像画面那样用 `sync_channel(1)` + 丢弃**：画面是**流**——
/// 丢一帧，下一帧自然会补上；光标形状是**状态**——丢一次「缩放 → 箭头」
/// 的切换，界面上就永远停在缩放光标上，直到下一次形状变化才可能纠正。
/// 真机上就是这么坏的：日志里成片出现「光标位图被丢弃（UI 侧尚未取走上一张）」，
/// 而 `last_cursor_seq` 在发送前就已经推进，丢掉的那张**再也不会重发**。
///
/// 光标位图很小（几百 KB 以内），留一份在 `Mutex` 里没有内存压力，
/// 换来的是「形状变化绝不丢」。
#[derive(Debug, Default)]
struct CursorSlot {
	inner: std::sync::Mutex<Option<(u64, CursorSprite)>>,
}

impl CursorSlot {
	/// 存入最新一张，覆盖任何还没被取走的旧图。
	fn put(&self, seq: u64, sprite: CursorSprite) {
		if let Ok(mut slot) = self.inner.lock() {
			*slot = Some((seq, sprite));
		}
	}

	/// 取走当前这张（取走后槽位清空，下次变化才会再填）。
	fn take(&self) -> Option<(u64, CursorSprite)> {
		self.inner.lock().ok().and_then(|mut slot| slot.take())
	}
}

impl RdpSession {
	/// 起一条会话。**立即返回**——连接在后台线程里做，结果经通道回来。
	pub fn spawn(params: ConnectionParams, expected_cert: Option<String>) -> Self {
		let (command_tx, command_rx) = mpsc::channel();
		let (status_tx, status_rx) = mpsc::channel();
		// 容量 1：UI 每帧最多取一帧画面，攒着没有意义
		let (frame_tx, frame_rx) = mpsc::sync_channel(1);
		// 光标是覆盖式槽而不是通道：形状是状态，丢一次切换就永远停在错的光标上
		let cursors = Arc::new(CursorSlot::default());
		let clipboard = Arc::new(daisy_rdp::Clipboard::new());
		// u64::MAX 表示「还没收到过坐标」

		let join = std::thread::Builder::new()
			.name("daisy-rdp-session".to_string())
			.spawn({
				let cursors = Arc::clone(&cursors);
				let clipboard = Arc::clone(&clipboard);
				move || {
					run(
						params,
						expected_cert,
						command_rx,
						status_tx,
						frame_tx,
						&cursors,
						&clipboard,
					);
					// 会话线程退出（连接失败/断开/关闭）：唤醒所有挂在
					// 延迟渲染 read_chunk 上的 COM/AppKit 线程，绝不留
					// 永久阻塞——否则关窗口又变成「未响应」。
					clipboard.invalidate_remote_content();
				}
			})
			.expect("创建 RDP 会话线程失败");

		Self {
			commands: command_tx,
			statuses: status_rx,
			frames: frame_rx,
			cursors,
			clipboard,
			join: Some(join),
		}
	}

	/// 取走全部已到达的状态事件（后到的覆盖先到的）。
	pub fn drain_statuses(&self) -> Vec<RdpStatus> {
		self.statuses.try_iter().collect()
	}

	/// 取走最新一帧。返回 `None` 表示这期间没有新画面。
	///
	/// 循环取空：容量 1 的通道里最多一条，但语义上是"取到没有为止"，
	/// 免得将来改容量时这里悄悄落后。
	pub fn take_frame(&self) -> Option<(u64, RgbaFrame)> {
		let mut latest = None;
		while let Ok(frame) = self.frames.try_recv() {
			latest = Some(frame);
		}
		latest
	}

	/// 取走最新一张光标位图。返回 `None` 表示形状没变或还没收到。
	pub fn take_cursor(&self) -> Option<(u64, CursorSprite)> {
		self.cursors.take()
	}

	/// 取走远端剪贴板文本（S8-63）。`None` 表示没有新内容。
	pub fn take_remote_clipboard(&self) -> Option<String> {
		self.clipboard.take_remote()
	}

	/// 发送一段文本到远端剪贴板（S8-63）。
	///
	/// **不阻塞**：只把文本交给会话线程，由它在下一帧泵循环里发出。
	pub fn send_clipboard(&self, text: String) {
		self.clipboard.send_text(text);
	}

	/// 发送一组本地文件到远端剪贴板（S10）。
	///
	/// 与文本同理**不阻塞**：格式列表由会话线程在泵循环里上报，
	/// 服务端等用户在远端粘贴时才来逐块取文件内容。
	/// 这里**不注入 Ctrl+V**——文件的粘贴由用户自己在远端按（Finder
	/// 复制后焦点切到 RDP 按 Cmd+V，键盘链路本来就会转发）。
	pub fn send_clipboard_files(&self, files: Vec<daisy_rdp::LocalFile>) {
		self.clipboard.send_files(files);
	}

	/// 取走下载完成的远端文件（S10）。`None` 表示没有新东西。
	pub fn take_remote_files(&self) -> Option<Vec<daisy_rdp::RemoteFile>> {
		self.clipboard.take_remote_files()
	}

	/// 取走远端文件清单（延迟渲染模式）。上层据此挂本地延迟渲染剪贴板。
	#[allow(dead_code)] // 阶段 2/3 的本地延迟渲染剪贴板使用
	pub fn take_remote_manifest(&self) -> Option<Vec<daisy_rdp::RemoteFileMeta>> {
		self.clipboard.take_remote_manifest()
	}

	/// 打开/关闭延迟渲染模式（本地延迟渲染剪贴板就绪后打开）。
	#[allow(dead_code)] // 阶段 2/3 打开
	pub fn set_deferred_mode(&self, enabled: bool) {
		self.clipboard.set_deferred_mode(enabled);
	}

	#[allow(dead_code)] // 阶段 2/3 查询
	pub fn deferred_mode(&self) -> bool {
		self.clipboard.deferred_mode()
	}

	/// 同步读取远端文件的指定区间（延迟渲染回调调用，会阻塞直到数据到达）。
	#[allow(dead_code)] // 阶段 2/3 的 IStream::Read / writePromiseToURL 使用
	pub fn read_remote_chunk(
		&self,
		list_index: u32,
		offset: u64,
		len: u32,
	) -> Result<Vec<u8>, String> {
		self.clipboard.read_chunk(list_index, offset, len)
	}

	/// 底层剪贴板通道的 Arc（延迟渲染的 ChunkSource 持有它，跨 COM 线程读字节）。
	#[allow(dead_code)] // 阶段 2/3 使用
	pub fn clipboard_handle(&self) -> std::sync::Arc<daisy_rdp::Clipboard> {
		Arc::clone(&self.clipboard)
	}

	/// 取走远端文件下载失败的文案（一次）。
	pub fn take_download_error(&self) -> Option<String> {
		self.clipboard.take_download_error()
	}

	/// 请求远端切换桌面分辨率（S8-64）。
	///
	/// **不阻塞**：交给会话线程发。失败经 [`RdpStatus::ResizeFailed`] 回来。
	pub fn resize(&self, width: u32, height: u32) {
		// 通道断了也只是这条指令没人收，界面不需要为此做特殊处理
		let _ = self.commands.send(Command::Resize { width, height });
	}

	/// 服务端有没有主动广播过剪贴板变化（远端 → 本地方向是否可用）。
	pub fn clipboard_server_broadcast(&self) -> bool {
		self.clipboard.server_broadcast_ever()
	}

	/// 服务端广播之后我们有没有真的收到数据。
	pub fn clipboard_remote_data_received(&self) -> bool {
		self.clipboard.remote_data_received()
	}

	/// 本会话有没有向远端上报过格式列表。
	///
	/// 没报过时远端 rdpclip 不会监听会话剪贴板，见 [`Self::arm_clipboard`]。
	pub fn clipboard_announced(&self) -> bool {
		self.clipboard.announced_ever()
	}

	/// 上报一条空文本，只为让远端 rdpclip 开始监听（详见
	/// `daisy_rdp::Clipboard::arm`）。**只在本地确实没有可传内容时调**。
	pub fn arm_clipboard(&self) {
		self.clipboard.arm();
	}

	/// 最近一条服务端报文（类型，距今多久）——诊断「远端没反应」用。
	pub fn clipboard_last_server_message(&self) -> Option<(&'static str, std::time::Duration)> {
		self.clipboard.last_server_message()
	}

	/// 完整的一次「粘贴」：送文本 + 让远端执行 Ctrl+V。
	///
	/// 走同一条命令是为了保证顺序——见 [`Command::Paste`]。
	pub fn paste(&self, text: String) {
		if self
			.commands
			.send(Command::Paste {
				content: PasteContent::Text(text),
				announce: true,
			})
			.is_err()
		{
			tracing::debug!("RDP 会话线程已退出，粘贴被丢弃");
		}
	}

	/// 把一组本地文件送进远端剪贴板并补发 Ctrl+V（S10）。
	///
	/// 用户在 Daisy 窗口里按 Cmd+V 时走这条——**文件不能当文本处理**。
	pub fn paste_files(&self, files: Vec<daisy_rdp::LocalFile>, announce: bool) {
		if self
			.commands
			.send(Command::Paste {
				content: PasteContent::Files(files),
				announce,
			})
			.is_err()
		{
			tracing::debug!("RDP 会话线程已退出，文件粘贴被丢弃");
		}
	}

	/// 请求断开。线程收尾后 `Drop` 里 join。
	pub fn disconnect(&self) {
		let _ = self.commands.send(Command::Disconnect);
	}

	/// 注入一次键鼠输入。
	///
	/// **不要在这里等回执**：输入是高频事件（拖动鼠标每帧好几条），
	/// 等回执会把 UI 拖死。失败由会话线程记日志，会话结束则由状态通道
	/// 统一告诉界面。
	pub fn send_input(&self, event: InputEvent) {
		if self.commands.send(Command::Input(event)).is_err() {
			// 线程已经退出：界面可能还慢一帧才发现，属正常竞态
			tracing::debug!("RDP 会话线程已退出，输入事件被丢弃");
		}
	}
}

/// 关闭时最多等会话线程多久。
///
/// 这里必须**有上限**：`disconnect()` 只是往队列里塞一条命令（合作式），
/// 会话线程若正卡在 `Command::Paste` 的等回执/注入/验证（累计可达 4 秒）
/// 或阻塞式 `freerdp_disconnect`，无超时 `join()` 会把 UI 主线程一起拖死——
/// 真机表现就是「点关闭后窗口标题变成『未响应』」。超过这个时限就放弃
/// join：会话线程结束时会自己在它自己的线程里释放 FreeRDP 上下文， Arc
/// 也会保住共享状态，不会 use-after-free，只是这次不再同步等待它。
const CLOSE_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

impl Drop for RdpSession {
	fn drop(&mut self) {
		tracing::info!("[CLOSE] RdpSession::drop begin");
		self.disconnect();
		// **先掐断按需读取**：延迟渲染下 OLE/AppKit 线程可能正挂在
		// `read_chunk` 的 recv_timeout 上，会话线程若卡在 freerdp_disconnect
		// 就永远没人来喂数据——那是 15s 级别的 UI 卡死（Windows 会弹
		// 「未响应」）。这里立刻让所有等待者失败、并让后续读取秒失败。
		self.clipboard
			.invalidate_remote_content_within(std::time::Duration::from_millis(200));
		tracing::info!("[CLOSE] clipboard invalidated");
		// Windows：把我们挂在系统剪贴板上的延迟渲染对象撤掉。留着的话
		// OLE 会在进程/会话收尾时向已断开的会话索要文件字节。
		#[cfg(target_os = "windows")]
		{
			crate::deferred_clipboard_win::clear_deferred();
			tracing::info!("[CLOSE] deferred clipboard cleared");
		}
		// macOS：同上。放手 delegate 后 Finder 粘贴立刻失败，不会挂在
		// `read_chunk` 上等满 8 s 超时。
		#[cfg(target_os = "macos")]
		{
			crate::deferred_clipboard_mac::clear_deferred();
			tracing::info!("[CLOSE] file promise clipboard cleared");
		}
		let Some(join) = self.join.take() else {
			tracing::info!("[CLOSE] no join handle");
			return;
		};
		// 先给一个短暂的同步窗口：绝大多数情况下会话线程下一轮泵就退出了，
		// join 立刻返回，FreeRDP 上下文仍保证在正确的线程里释放。
		std::thread::sleep(std::time::Duration::from_millis(20));
		if join.is_finished() {
			tracing::info!("[CLOSE] session thread already finished");
			let _ = join.join();
			return;
		}
		// 没退就限时轮询，别在 UI 线程上死等。
		let deadline = std::time::Instant::now() + CLOSE_JOIN_TIMEOUT;
		while std::time::Instant::now() < deadline {
			std::thread::sleep(std::time::Duration::from_millis(25));
			if join.is_finished() {
				tracing::info!("[CLOSE] session thread finished in time");
				let _ = join.join();
				return;
			}
		}
		// 超时：放弃 join（detach）。线程会在自己退出时释放上下文与 Arc。
		tracing::warn!(
			"关闭 RDP 会话：等待 {CLOSE_JOIN_TIMEOUT:?} 后会话线程仍未退出，放弃同步等待以免窗口无响应"
		);
		drop(join);
	}
}

/// 线程主体：连接、泵事件、把帧与状态送回 UI。
fn run(
	params: ConnectionParams,
	expected_cert: Option<String>,
	commands: Receiver<Command>,
	statuses: Sender<RdpStatus>,
	frames: SyncSender<(u64, RgbaFrame)>,
	cursors: &CursorSlot,
	clipboard: &std::sync::Arc<daisy_rdp::Clipboard>,
) {
	let mut client = match RdpClient::new() {
		Ok(client) => client,
		Err(error) => {
			let _ = statuses.send(RdpStatus::Ended(format!("初始化失败：{error}")));
			return;
		},
	};
	// 剪贴板用界面那份（Arc 共享）而不是 client 自建的——
	// 否则界面写进去的内容到不了真正装着回调的那一个上。
	client.use_clipboard(Arc::clone(clipboard));
	// 启用剪贴板：必须在 connect **之前**调——通道要挂进 FreeRDP 的
	// LoadChannels 钩子，连接过程中它才有机会装（详见 enable_clipboard 的文档）。
	client.enable_clipboard();
	// 动态分辨率（S8-64）同理：drdynvc 与 disp 都得在钩子里装。
	client.enable_display();

	// 证书校验必须在连接**之前**把期望值交给回调：握手一开始就会问
	client.trust_certificate(expected_cert.as_deref());
	if let Err(error) = client.connect(&params) {
		let _ = statuses.send(RdpStatus::Ended(error.to_string()));
		return;
	}

	// 首连要上报指纹（界面写库并提示）；一致就什么也不做
	if let Some(observation) = client.certificate_observation() {
		match observation.verdict {
			daisy_rdp::Verdict::FirstSight => {
				tracing::info!(
					"[证书] 首次连接 {}:{}，指纹 {}",
					observation.host,
					observation.port,
					daisy_rdp::format_fingerprint(&observation.fingerprint)
				);
				let _ = statuses.send(RdpStatus::CertificateFirstSeen {
					host: observation.host,
					port: observation.port,
					fingerprint: observation.fingerprint,
					subject: observation.subject,
					issuer: observation.issuer,
				});
			},
			daisy_rdp::Verdict::Match => {
				tracing::debug!("[证书] 与库中记录一致");
			},
			// 变更会在 `connect` 里就变成错误返回，走不到这里
			daisy_rdp::Verdict::Changed { .. } => {},
		}
	}
	let _ = statuses.send(RdpStatus::Connected {
		width: params.width,
		height: params.height,
	});

	let mut last_seq = 0;
	// 压着待推的帧（帧合并，见主循环里的说明）
	let mut pending_frame: Option<(u64, RgbaFrame)> = None;
	// 上一次真正把帧交给界面的时刻
	let mut last_push_at = std::time::Instant::now() - FRAME_MIN_INTERVAL;
	let mut last_cursor_seq = u64::MAX; // 与任何真实序号都不同，保证首次必推
	// disp 通道是否已经连上（只回报一次）。
	let mut display_ready = false;
	// **物理** Ctrl 是否还按着（注入的 Ctrl 不计入）。
	//
	// 只在这一层维护是安全的：转发和注入都在同一个线程里按队列顺序执行，
	// 谁先谁后是确定的，不存在跨线程竞态。
	let mut physical_ctrl: Option<daisy_rdp::Key> = None;
	loop {
		// 先批量消费 UI 指令，再泵事件——输入攒在队列里等到下一轮才发，
		// 拖动鼠标时会明显发涩
		let mut should_stop = false;
		loop {
			match commands.try_recv() {
				Ok(Command::Disconnect) => {
					should_stop = true;
					break;
				},
				Ok(Command::Paste { content, announce }) => {
					// 日志里用来描述「推了什么」：文本给字数，文件给个数。
					let pushed = match &content {
						PasteContent::Text(text) => format!("{} 字", text.chars().count()),
						PasteContent::Files(files) => format!("{} 个文件", files.len()),
					};
					// 顺序要紧：先把内容放进去，当场把格式列表发出去，
					// **等远端确认收到这批格式**，最后才注入 Ctrl+V。
					//
					// `announce == false`：这批内容刚上报过，**不要再报一遍**。
					// 重复上报会让 rdpclip 陷入「反复索要描述表但从不取内容」，
					// 文件永远不落盘（真机对照实测）。
					if announce {
						match content {
							PasteContent::Text(text) => client.send_clipboard(text),
							PasteContent::Files(files) => {
								tracing::info!(
									"[剪贴板] 粘贴的是 {} 个文件，上报文件格式列表（不发文本）",
									files.len()
								);
								client.send_clipboard_files(files);
							},
						}
						client.flush_clipboard_now();
					} else {
						tracing::info!(
							"[剪贴板] 粘贴：这批内容刚上报过（{pushed}），不重报格式列表，直接按键"
						);
					}
					// **等格式列表回执，不等数据响应。**
					//
					// 服务端只在「用户粘贴」时才发 `CB_FORMAT_DATA_REQUEST`
					// 来索要数据，而那次粘贴就是**我们自己注入的 Ctrl+V**。
					// 所以这里若反过来「等数据到了再按键」会形成死锁：
					// 按键先到 → 远端剪贴板还是空的 → 粘贴落空、不发取数请求
					// → 永远等不到数据。
					//
					// 正确顺序：格式列表 → 回执(OK) → 注入 Ctrl+V
					//          → 服务端发 FormatDataRequest → 我们应答数据
					//
					// 回执很快（实测 5ms），设个上限兜底即可。
					// `announce == false` 时没发格式列表，自然没有回执可等。
					if announce {
						let mut waited = 0u32;
						while !client.clipboard_format_list_acked() && waited < ACK_WAIT_MAX_MS {
							match client.pump(PASTE_POLL_MS) {
								Ok(true) => {},
								Ok(false) | Err(_) => break,
							}
							waited += PASTE_POLL_MS;
						}
						if client.clipboard_format_list_acked() {
							tracing::info!(
								"[剪贴板] 远端已确认格式列表（{waited}ms），注入 Ctrl+V"
							);
						} else {
							tracing::warn!(
								"[剪贴板] {ACK_WAIT_MAX_MS}ms 内远端未确认格式列表，仍注入 Ctrl+V"
							);
						}
					}
					// **组合键必须逐个发，中间要 pump。**
					//
					// `send_input` 是同步直接写通道的。四条按键（Ctrl↓ V↓ V↑ Ctrl↑）
					// 连着发、毫秒级间隔，RDP 输入通道可能把它们当成同一批——
					// 远端还没处理完 Ctrl 的按下就收到了 V，于是**Ctrl 被丢掉、
					// 远端粘出字面的字母v**（用户实测现象）。
					//
					// 组合键对时序敏感：真实键盘敲 Ctrl+V 时，Ctrl 会比 V 早
					// 按下几十毫秒。这里每步之间 pump 一次（pump 会走完整的
					// 发送路径，把按键真正刷出去）。
					let started = std::time::Instant::now();
					for (index, event) in paste_keys().into_iter().enumerate() {
						let label = match &event {
							InputEvent::Key { key, down, .. } => {
								format!("{key:?}{}", if *down { "↓" } else { "↑" })
							},
							_ => "?".to_string(),
						};
						if let Err(error) = client.send_input(event) {
							tracing::warn!(
								"[剪贴板] 注入第 {} 步（{label}）失败：{error}",
								index + 1
							);
						} else {
							tracing::info!(
								"[剪贴板] 注入第 {} 步 {label}（+{:?}）",
								index + 1,
								started.elapsed()
							);
						}
						if index < 3 {
							// **必须 pump，不能只 sleep**：`send_input` 是同步
							// 写通道，但按键要等事件循环泵一次才真正上线。
							// 这里原先是 `input_gap`（纯 sleep），注释写着
							// 「每步之间 pump 一次」而实现是空等——按键积在
							// 缓冲里，服务端根本没收到，粘贴自然无反应。
							// pump 的等待本身也就充当了步间间隔。
							if let Err(error) = client.pump(KEY_GAP_MS) {
								tracing::warn!("[剪贴板] 注入中断（{error}）");
								break;
							}
						}
					}
					tracing::info!(
						"[剪贴板] Ctrl+V 注入完毕，总耗时 {:?}（预期 ≥{}ms）",
						started.elapsed(),
						KEY_GAP_MS * 3
					);

					// **恢复物理修饰键**：注入序列末尾的 Ctrl↑ 把远端 Ctrl
					// 抬起了，可用户手里还按着（他是按着 Ctrl 敲的 V）。
					// 不补这一下，紧接着的 Ctrl+Z 会变成裸 z。
					if let Some(event) = restore_ctrl_after_paste(physical_ctrl) {
						if let Err(error) = client.send_input(event) {
							tracing::warn!("[剪贴板] 恢复物理 Ctrl 失败：{error}");
						} else {
							tracing::info!(
								"[剪贴板] 物理 {physical_ctrl:?} 仍按着，已补发按下以恢复远端修饰键状态"
							);
						}
					}

					// **自我验证**：等服务端来取数。
					//
					// 第二十六轮把判据换了。原来的判据是「服务端广播回来的
					// 内容与推出去的一致」，但那是错的——服务端把我们的
					// 格式列表装进 Windows 剪贴板后，**不会再广播回同一个
					// 客户端**（它自己就是这次变化的发起方）。所以那条判据
					// 永远等不到，每次都报「2000ms 内服务端没广播」，
					// 把真正有用的信号盖住了。
					//
					// 正确的判据是 `CB_FORMAT_DATA_REQUEST`：服务端**只有
					// 在用户真的粘贴、需要渲染延迟数据时**才会来要数据。
					// 收到了，就证明 Windows 剪贴板里确实有我们的格式、
					// 粘贴动作确实落到了能接收文本的窗口上。
					verify_forward_paste(&client, &pushed);
				},
				Ok(Command::Resize { width, height }) => {
					if let Err(error) = client.resize_desktop(width, height) {
						tracing::warn!("[分辨率] {width}×{height} 请求失败：{error}");
						let _ = statuses.send(RdpStatus::ResizeFailed(error.to_string()));
					}
				},
				Ok(Command::Input(event)) => {
					// 记住物理 Ctrl 的真实状态：粘贴后要靠它把远端修饰键恢复。
					if let InputEvent::Key { key, down, .. } = &event
						&& matches!(
							key,
							daisy_rdp::Key::ControlLeft | daisy_rdp::Key::ControlRight
						) {
						if *down {
							physical_ctrl = Some(*key);
						} else if physical_ctrl == Some(*key) {
							physical_ctrl = None;
						}
					}
					// 输入失败不结束会话：远端正好在断时 pump 会给出更准确的
					// 原因，这里只记日志
					if let Err(error) = client.send_input(event) {
						tracing::debug!("注入输入事件失败：{error}");
					}
				},
				Err(TryRecvError::Empty) => break,
				// UI 侧已释放（通道断开）也当作断开指令
				Err(TryRecvError::Disconnected) => {
					should_stop = true;
					break;
				},
			}
		}
		if should_stop {
			break;
		}

		// 帧要等一拍再推，见下面的「帧合并」；没有待推帧时按正常节奏等
		let pump_wait = if pending_frame.is_some() {
			FRAME_MIN_INTERVAL_MS
		} else {
			PUMP_WAIT_MS
		};
		match client.pump(pump_wait) {
			Ok(true) => {},
			Ok(false) => {
				let _ = statuses.send(RdpStatus::Ended("会话已结束".to_string()));
				return;
			},
			Err(error) => {
				let _ = statuses.send(RdpStatus::Ended(error.to_string()));
				return;
			},
		}

		// disp 通道就绪：只在**第一次**连上时告诉界面，之后不再发。
		// 通道连接晚于 `Connected`（要等服务端接受这个动态通道），
		// 界面据此点亮分辨率控件。
		if !display_ready && client.display_connected() {
			display_ready = true;
			let _ = statuses.send(RdpStatus::DisplayReady);
		}

		// 光标位图：形状变了才推。位置**不进通道**——界面直接用自己记的
		// 本地指针坐标（就是刚发给远端的那些），既精确又零延迟。
		//
		// 覆盖式槽（不是通道）：形状是**状态**，丢掉一次「缩放 → 箭头」的
		// 切换，界面上就会永远停在缩放光标上。真机日志里成片的
		// 「光标位图被丢弃」就是这么坏的。
		if let Some((seq, cursor)) = client.latest_cursor()
			&& seq != last_cursor_seq
		{
			last_cursor_seq = seq;
			cursors.put(seq, cursor);
		}

		if let Some((seq, frame)) = client.latest_frame()
			&& seq != last_seq
		{
			last_seq = seq;
			// 在这里就转成 RGBA：`FrameBuffer` 的 XRGB32 行宽语义留在协议层，
			// 跨线程传的是已规整的紧凑像素。
			//
			// 转不出来（缓冲不完整，通常发生在分辨率刚变那一瞬）就**整帧跳过**：
			// 推半张空白画面比丢一帧难看得多——真机上表现为画面被撕成两半。
			match to_rgba(&frame) {
				// **先压着，不立刻推**：拖动窗口时服务端会连发好几个 UPDATE
				// PDU，每个 PDU 结束都会触发一次 EndPaint，我们的快照就落在
				// 「这一批里的中间状态」上——真机看到的就是窗口已到新位置、
				// 旧位置还留着残影。合并到下一拍再推，推出去的是这一拍内
				// **所有** PDU 处理完之后的最新状态，中间态就没人看见。
				Some(rgba) => pending_frame = Some((seq, rgba)),
				None => tracing::debug!(
					"画面帧不完整（{}x{} stride={} 数据 {} 字节），跳过这一帧",
					frame.width,
					frame.height,
					frame.stride,
					frame.data.len()
				),
			}
		}

		// 距离上一次推帧够久了才把压着的那一帧交出去（上限约 60fps）。
		// 没有这条，界面会被中间态刷得发抖；有这条，最坏只是迟到 16ms。
		if let Some((seq, rgba)) = pending_frame.take() {
			if last_push_at.elapsed() >= FRAME_MIN_INTERVAL {
				if frames.try_send((seq, rgba)).is_err() {
					tracing::debug!("画面帧被丢弃（UI 侧尚未取走上一帧）");
				}
				last_push_at = std::time::Instant::now();
			} else {
				pending_frame = Some((seq, rgba));
			}
		}
	}

	// 主动断开走这里，不算错误
	let _ = statuses.send(RdpStatus::Ended("已断开".to_string()));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn disconnect_is_idempotent_and_joins() {
		// 不联网：连一个必然失败的目标，只验证线程能起来、能收尾
		let params = ConnectionParams::new("127.0.0.1", "u", "p").with_resolution(64, 64);
		let session = RdpSession::spawn(params, None);

		// 等线程自己结束（连接会被拒），或主动断开
		std::thread::sleep(std::time::Duration::from_millis(200));
		session.disconnect();
		let statuses = session.drain_statuses();
		assert!(
			!statuses.is_empty(),
			"线程收尾前应至少回报一条状态（连接失败或已断开）"
		);
		drop(session); // 触发 join
	}

	/// 粘完之后，物理按着的 Ctrl 必须被重新按下——否则下一个组合键失效。
	///
	/// 用户实测习惯：按着 Ctrl 敲 V。注入序列末尾会发 Ctrl↑，
	/// 若此时物理 Ctrl 还按着，远端就认为 Ctrl 抬起了，
	/// 紧接着按 Z 变成裸 `z`。
	#[test]
	fn paste_restores_ctrl_that_is_still_held() {
		let event = restore_ctrl_after_paste(Some(daisy_rdp::Key::ControlLeft))
			.expect("物理 Ctrl 还按着时必须补发");
		match event {
			InputEvent::Key { key, down, repeat } => {
				assert_eq!(key, daisy_rdp::Key::ControlLeft);
				assert!(down, "补发的是「按下」，让远端回到真实状态");
				assert!(!repeat);
			},
			other => panic!("补发必须是按键事件，实际是 {other:?}"),
		}

		// 右键 Ctrl 同样要恢复（外接键盘右手按 Ctrl 的场景）
		assert!(restore_ctrl_after_paste(Some(daisy_rdp::Key::ControlRight)).is_some());

		// 没按着（Cmd+V、或已经松手）就不能补，否则远端会卡住 Ctrl
		assert!(
			restore_ctrl_after_paste(None).is_none(),
			"没按着 Ctrl 却补发按下，远端 Ctrl 会永远保持按下"
		);
	}

	/// 造一张指定宽度的光标位图（不透明白），便于断言"拿到的是哪一张"。
	fn sprite(width: u16) -> CursorSprite {
		let count = usize::from(width) * usize::from(width);
		CursorSprite {
			width,
			height: width,
			hot_x: 0,
			hot_y: 0,
			pixels: vec![0xFF; count * 4],
		}
	}

	/// 槽里没被取走时，新来的形状必须**覆盖**旧的，而不是被丢掉。
	///
	/// 这正是「移出窗口边缘后光标不换回箭头」的成因：原来用容量 1 的通道，
	/// UI 还没取走上一张时新形状直接 `try_send` 失败被丢，而 `last_cursor_seq`
	/// 已经推进、那张再也不会重发，于是界面永远停在旧形状上。
	#[test]
	fn cursor_slot_keeps_the_newest_shape() {
		let slot = CursorSlot::default();

		// 第一次：箭头
		slot.put(1, sprite(32));
		// UI 还没取，紧接着来了缩放光标
		slot.put(2, sprite(48));
		// 再紧接着又切回箭头（复用同一个形状）
		slot.put(3, sprite(32));

		// 只该拿到最后一次，且序号要对得上
		let (seq, got) = slot.take().expect("应当拿到最新形状");
		assert_eq!(seq, 3);
		assert_eq!(got.width, 32);
		// 取走之后槽位清空，不会重复上传同一张
		assert!(slot.take().is_none(), "取走后应当清空");
	}

	/// 多线程同时写也应当始终拿到某一张完整形状，不能拿到半张或空。
	#[test]
	fn cursor_slot_is_safe_under_concurrent_writes() {
		let slot = Arc::new(CursorSlot::default());
		let writers: Vec<_> = (0..4u64)
			.map(|worker| {
				let slot = Arc::clone(&slot);
				let base = worker * 1000;
				std::thread::spawn(move || {
					for i in 0..64u16 {
						slot.put(base + u64::from(i), sprite(i % 8 + 1));
					}
				})
			})
			.collect();
		for handle in writers {
			handle.join().expect("写线程不应 panic");
		}
		// 取到的必须是一张完整位图，长度与标称尺寸吻合
		if let Some((_, got)) = slot.take() {
			assert_eq!(
				got.pixels.len(),
				usize::from(got.width) * usize::from(got.height) * 4
			);
		}
	}
}
