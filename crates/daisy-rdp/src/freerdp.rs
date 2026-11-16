//! FreeRDP 库模式封装：实例生命周期 + 回调 trampoline。
//!
//! 对应 STEPS.md 步骤 58。
//!
//! ## 生命周期（RAII）
//!
//! ```text
//!   RdpClient::new
//!     └─ freerdp_new()                分配实例
//!     └─ 设置 ContextSize = sizeof(rdpContext) + sizeof(usize)
//!     └─ freerdp_context_new()        分配 context，触发 ContextNew 回调
//!
//!   RdpClient::connect()
//!     └─ 写入 settings（host/port/user/...）
//!     └─ freerdp_connect()            触发 PreConnect → 握手 → PostConnect
//!
//!   Drop
//!     └─ freerdp_disconnect()         若已连接
//!     └─ freerdp_context_free()       释放 context
//!     └─ freerdp_free()               释放实例
//! ```
//!
//! ## 回调 trampoline
//!
//! FreeRDP 在自己的工作线程里回调 `PreConnect` / `PostConnect` / `BeginPaint` /
//! `EndPaint` / `DesktopResize`。这些回调收到的是 `rdpContext*`，我们要在
//! 其扩展内存里找回 Rust 侧的 [`ClientState`]。
//!
//! 做法：`ContextSize` 设为 `sizeof(rdpContext) + sizeof(usize)`，多出来的
//! 那段存一个 `*mut ClientState`。trampoline 里按偏移取回，转回 `&ClientState`。
//!
//! 线程安全：`ClientState` 用 `Arc` 包裹、内部字段用原子量或 `Mutex`，
//! 保证跨线程（FreeRDP 工作线程 ↔ 我们的消费线程）访问安全。

use std::mem;
use std::ptr;
use std::sync::Arc;

use crate::cursor::{CursorBitmap, CursorSprite, decode};
use crate::display::physical_mm;
use crate::error::{RdpError, Result};
use crate::ffi;
use crate::input::InputEvent;

/// FreeRDP 的 `PROXY_TYPE_NONE`——关掉代理，否则本机代理会劫持 RDP 流量。
const PROXY_TYPE_NONE: u32 = 0;
/// `PIXEL_FORMAT_XRGB32` 的数值。XRGB32 在内存里是 `X,R,G,B`。
///
/// FreeRDP 里它是**宏**而非常量，bindgen 导不出宏，只能在这里按定义复刻：
///
/// ```c
/// #define FREERDP_PIXEL_FORMAT(_bpp, _type, _a, _r, _g, _b) \
///     ((_bpp << 24) | (_type << 16) | (_a << 12) | (_r << 8) | (_g << 4) | (_b))
/// #define PIXEL_FORMAT_XRGB32 \
///     FREERDP_PIXEL_FORMAT(32, FREERDP_PIXEL_FORMAT_TYPE_ARGB /* = 1 */, 0, 8, 8, 8)
/// ```
///
/// 名字里是 X 而 type 是 ARGB：`_a = 0` 正表示该字节（X）不参与合成。
///
/// ⚠️ 这里**曾经写成 `0x00000000`**——那恰好是 `PIXEL_FORMAT_NONE` 的值，
/// 于是 `gdi_init` 把目标格式判成 UNKNOWN、PostConnect 直接失败
/// （实测报 `ERRCONNECT_POST_CONNECT_FAILED` 且日志里
/// `Local framebuffer format UNKNOWN`）。手写常量必须对着头文件算，
/// 不能凭印象填——见 `pixel_format_matches_the_header` 单测。
const PIXEL_FORMAT_XRGB32: u32 = freerdp_pixel_format(32, 1, 0, 8, 8, 8);

/// 复刻 FreeRDP 的 `FREERDP_PIXEL_FORMAT` 宏。
///
/// `kind` 取 `FREERDP_PIXEL_FORMAT_TYPE_*`（A=0 / ARGB=1 / ABGR=2 / RGBA=3 / BGRA=4）。
const fn freerdp_pixel_format(bpp: u32, kind: u32, a: u32, r: u32, g: u32, b: u32) -> u32 {
	(bpp << 24) | (kind << 16) | (a << 12) | (r << 8) | (g << 4) | b
}
/// FreeRDP 事件等待的上限。
const MAXIMUM_WAIT_OBJECTS: usize = 64;

/// 临时清空代理环境变量的 guard。
///
/// FreeRDP 的 `proxy_prepare` 会无条件读 `https_proxy`/`HTTPS_PROXY` 环境变量，
/// 若存在就把 `ProxyType` 从 `NONE` 改成 `HTTP`（自动发现 HTTPS 代理）。
/// 这会让 Daisy 的直连 RDP 被本机代理劫持——S1-6 的 C 程序就是靠
/// `unset http_proxy https_proxy` 才连通的。
///
/// Daisy 的代理走自己的 `Dialer`（S7），是显式配置项。因此直连时必须
/// 屏蔽环境变量，连接建立后由 guard 恢复原值。
struct ProxyEnvGuard {
	saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl ProxyEnvGuard {
	/// 清空所有常见的代理环境变量，记录原值以便恢复。
	fn clear() -> Self {
		const KEYS: &[&str] = &[
			"http_proxy",
			"https_proxy",
			"HTTP_PROXY",
			"HTTPS_PROXY",
			"all_proxy",
			"ALL_PROXY",
			"no_proxy",
			"NO_PROXY",
		];

		let mut saved = Vec::with_capacity(KEYS.len());
		for key in KEYS {
			let old = std::env::var_os(key);
			// SAFETY: 在连接建立前的单线程初始化阶段调用，无并发环境变量访问。
			unsafe { std::env::remove_var(key) };
			saved.push((*key, old));
		}
		Self { saved }
	}
}

impl Drop for ProxyEnvGuard {
	fn drop(&mut self) {
		// 恢复原值（清空前存在过的变量）
		for (key, value) in self.saved.drain(..) {
			// SAFETY: 连接建立后、guard 释放时恢复；本 guard 持有期间无其它线程改环境变量。
			unsafe {
				match value {
					Some(v) => std::env::set_var(key, v),
					None => std::env::remove_var(key),
				}
			}
		}
	}
}

/// context扩展内存里放的东西（`ContextSize` 比 `sizeof(rdpContext)` 多出这么多）。
///
/// 存的都是**指向 Arc 内部数据的裸指针**——Arc 本体由 [`RdpClient`] 持有，
/// 生命周期覆盖整个实例，所以回调期间这些指针始终有效。
struct ClientExt {
	/// 帧、光标等跨线程状态。
	state: *const ClientState,
	/// 剪贴板通道（S8-63）。`enable_clipboard` 之前是空指针。
	clipboard: *const crate::clipboard::Clipboard,
	/// 动态分辨率（S8-64）。`enable_display` 之前是空指针。
	display: *const crate::display::DisplayControl,
	/// 证书指纹校验（S8-65）。`new()` 里就填好，回调随连接发生。
	certificate: *const crate::certificate::CertificateGuard,
}

/// 取context 扩展内存的可写引用。
///
/// # Safety
/// `context` 必须是我们创建的、且 `ContextSize` 已按 `sizeof(ClientExt)` 扩展过。
unsafe fn ext_slot_mut(context: *mut ffi::rdpContext) -> &'static mut ClientExt {
	// SAFETY: context 有效，扩展内存已预留。
	unsafe {
		&mut *(context as *mut u8)
			.add(mem::size_of::<ffi::rdpContext>())
			.cast::<ClientExt>()
	}
}

/// 取 context 扩展内存的只读引用。
///
/// # Safety
/// 同 [`ext_slot_mut`]。
unsafe fn ext_slot(context: *mut ffi::rdpContext) -> &'static ClientExt {
	// SAFETY: context 有效，扩展内存已预留。
	unsafe { &*ext_slot_mut(context) }
}

/// 跨线程共享的客户端状态。
///
/// 由 [`RdpClient`] 持有、经 trampoline 传给回调。回调在 FreeRDP 工作线程
/// 执行，读写在原子量 / `Mutex` 上。
struct ClientState {
	/// 最新一帧的像素数据（`stride * height` 字节，XRGB32 内存序）。
	frame: std::sync::Mutex<Option<FrameBuffer>>,
	/// 帧序号，每次 `EndPaint` 递增。
	///
	/// 界面靠它区分「真有新帧」与「还是上一帧」——一帧 1080p 就是 8 MB，
	/// 每帧无条件克隆 + 重传纹理纯属浪费。
	frame_seq: std::sync::atomic::AtomicU64,
	/// 是否已观察到 `DesktopResize`（分辨率变更）。
	resized: std::sync::atomic::AtomicBool,
	/// 服务端下发的光标位图（已解码成RGBA）。
	///
	/// RDP 不保证把光标合成进画面帧——实测移动指针后服务端一帧新画面都不发。
	/// 所以位图走独立消息（见 `cursor.rs`），界面拿到后自己画。
	cursor: std::sync::Mutex<Option<CursorSprite>>,
	/// 光标序号，变化即代表形状换了。
	cursor_seq: std::sync::atomic::AtomicU64,
	/// 服务端指针缓存：下标 → 位图。
	///
	/// `Pointer Cached Update` **只带一个下标、不带位图**（见
	/// `pointer_cached_trampoline`）。拿不到当初那张图，光标形状就会卡在
	/// 上一次切换的那一个——移出窗口边缘不换回箭头就是踩在这里。
	///
	/// FreeRDP 自带的 `pointer_cache` 本该负责这件事，但它的写入回调在
	/// `PostConnect` 里被我们覆盖了（见 `post_connect_trampoline` 里的
	/// `PointerColor = Some(...)` 等），所以那个缓存永远不会被填充，得自己维护。
	cursor_cache: std::sync::Mutex<std::collections::HashMap<u32, CursorSprite>>,
}

/// 指针缓存最多留多少条。
///
/// 服务端给的 `cacheIndex` 是 `UINT32`，理论上能指向任意下标。不设上限的话，
/// 一个异常服务端就能让缓存无限膨胀——96×96 的 32bpp 光标约 36 KB，
/// 六万多个下标就是 2 GB。
const CURSOR_CACHE_MAX: usize = 256;

impl ClientState {
	fn new() -> Self {
		Self {
			frame: std::sync::Mutex::new(None),
			frame_seq: std::sync::atomic::AtomicU64::new(0),
			resized: std::sync::atomic::AtomicBool::new(false),
			cursor: std::sync::Mutex::new(None),
			cursor_seq: std::sync::atomic::AtomicU64::new(0),
			cursor_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
		}
	}
}

/// 一帧的像素缓冲。
pub struct FrameBuffer {
	/// 宽度（像素）。
	pub width: u32,
	/// 高度（像素）。
	pub height: u32,
	/// 行宽（字节），不等于 `width * 4`。
	pub stride: u32,
	/// 像素数据，长度 `stride * height`，XRGB32 内存序（X,R,G,B）。
	pub data: Vec<u8>,
}

/// 连接参数。
///
/// 对应 STEPS.md 步骤 59：host / port / user / password / domain / 分辨率 / 色深。
/// 地址走 [`crate::RdpTarget`]，三种写法（域名 / IPv4 / IPv6）都支持。
#[derive(Clone)]
pub struct ConnectionParams {
	/// 主机名或 IP（IPv6 不带方括号，交给 `FreeRDP_ServerHostname`）。
	pub host: String,
	/// 端口，默认 3389。
	pub port: u16,
	/// 用户名。
	pub user: String,
	/// 密码。不参与 `Debug`，见下方手写实现。
	pub password: String,
	/// 域名（通常为空）。
	pub domain: String,
	/// 桌面宽度（像素）。
	pub width: u32,
	/// 桌面高度（像素）。
	pub height: u32,
	/// 是否映射剪贴板。
	///
	/// 对应 mstsc 连接前的「剪贴板」勾选项。为 false 时连 `cliprdr`
	/// 通道都不安装——不装就不会有协议往返，比装了再忽略更干净。
	pub clipboard: bool,
}

impl ConnectionParams {
	/// 最简构造：只给主机、用户、密码，其余用默认值。
	#[must_use]
	pub fn new(
		host: impl Into<String>,
		user: impl Into<String>,
		password: impl Into<String>,
	) -> Self {
		Self {
			host: host.into(),
			port: crate::DEFAULT_RDP_PORT,
			user: user.into(),
			password: password.into(),
			domain: String::new(),
			width: 1024,
			height: 768,
			// 剪贴板默认开：远程桌面的主要用途就是跨机搬文本
			clipboard: true,
		}
	}

	/// 是否映射剪贴板（对应 mstsc 连接前的勾选项）。
	#[must_use]
	pub fn with_clipboard(mut self, clipboard: bool) -> Self {
		self.clipboard = clipboard;
		self
	}

	/// 指定端口。
	#[must_use]
	pub fn with_port(mut self, port: u16) -> Self {
		self.port = port;
		self
	}

	/// 指定分辨率。
	#[must_use]
	pub fn with_resolution(mut self, width: u32, height: u32) -> Self {
		self.width = width;
		self.height = height;
		self
	}

}

impl std::fmt::Debug for ConnectionParams {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		// 密码绝不进日志
		f.debug_struct("ConnectionParams")
			.field("host", &self.host)
			.field("port", &self.port)
			.field("user", &self.user)
			.field("domain", &self.domain)
			.field("width", &self.width)
			.field("height", &self.height)
			.field("password", &"<已脱敏>")
			.finish()
	}
}

/// 一个 FreeRDP 客户端实例的 RAII 封装。
pub struct RdpClient {
	instance: *mut ffi::freerdp,
	state: Arc<ClientState>,
	connected: bool,
	/// 剪贴板通道（S8-63）。由外部注入（见 [`RdpClient::use_clipboard`]），
	/// 不注入则 `connect` 时自建。
	clipboard: Option<Arc<crate::clipboard::Clipboard>>,
	/// 最近一次的连接参数（决定要不要装剪贴板通道）。
	///
	/// `connect` 之前 `enable_clipboard` 就要用到，所以得单独记一份——
	/// 参数本身是 `connect` 的入参，那时还没存进结构体。
	connect_params: Option<ConnectionParams>,
	/// 剪贴板回调是否已经接管。
	///
	/// 通道要等握手之后才打开，所以接管必然晚于 `connect`——这个标记
	/// 让我们只尝试一次，不用每帧都去查通道表。
	clipboard_bound: std::sync::atomic::AtomicBool,
	/// 动态分辨率（S8-64）。只保留一份 `Arc`，指针通过扩展内存给回调。
	display: Option<Arc<crate::display::DisplayControl>>,
	/// drdynvc 的回调是否已经接管（同剪贴板：握手后才拿得到接口）。
	display_bound: std::sync::atomic::AtomicBool,
	/// 证书指纹校验（S8-65）。连接前写入期望值，连接后取观察结果。
	certificate: Arc<crate::certificate::CertificateGuard>,
}

// `*mut freerdp` 跨线程移动需要显式声明 Send。
unsafe impl Send for RdpClient {}

/// cliprdr 客户端的日志 tag。
///
/// **这个字符串不能猜。** FreeRDP 里是
/// `CHANNELS_TAG("cliprdr.client")`（`cliprdr_main.c` 的 `VirtualChannelEntryEx`
/// 与 `cliprdr_process_*` 都用它取 logger），而两个宏是：
/// ```c
/// #define FREERDP_TAG(tag) "com.freerdp." tag
/// #define CHANNELS_TAG(tag) FREERDP_TAG("channels.") tag   // include/freerdp/channels/log.h
/// ```
/// 展开后是 **`com.freerdp.channels.cliprdr.client`**——
/// 中间的 `channels.` 极易漏掉。写错时 `WLog_Get` **不报错**，
/// 只是新建一个没人写的 logger，于是日志文件生成却是 0 字节，
/// 看起来像「没消息」，实际是「写错了地方」。
const CLIPRDR_CLIENT_LOG_TAG: &std::ffi::CStr = c"com.freerdp.channels.cliprdr.client";

/// 是否要打 FreeRDP 协议层日志。
///
/// **默认关**：那是排查通道问题的手术刀，不是日常运行的一部分。
/// 光剪贴板一个通道，一次粘贴就有十几条 DEBUG；常开既占磁盘，
/// 也让人看不见真正该看的应用日志。
///
/// 需要时 `DAISY_FREERDP_LOG=1 daisy-app` 启动，
/// 日志落到 `$TMPDIR/daisy-freerdp-cliprdr.log`（落脚点由应用层设）。
pub fn protocol_log_enabled() -> bool {
	parse_switch(std::env::var("DAISY_FREERDP_LOG").ok().as_deref())
}

/// 环境变量开关的取值规则：**只有明确表示「开」才开**。
///
/// 未设 / 空串 / `0` / `off` / `false` / `no` 一律算关——
/// 一个打错的变量名不该顺手打开一堆 DEBUG 日志。
fn parse_switch(value: Option<&str>) -> bool {
	let Some(raw) = value else {
		return false;
	};
	!matches!(
		raw.trim().to_ascii_lowercase().as_str(),
		"" | "0" | "off" | "false" | "no"
	)
}

/// 只把 cliprdr 客户端这一个 tag 的日志级别开到 DEBUG。
///
/// **落盘路径不在这里设**：FreeRDP 的根 logger 只初始化一次，
/// 由首次 `WLog_Get`（发生在 `freerdp_new()` 内部）触发，
/// 任何「连接前」的调用都太晚——必须由应用层在 `main` 开头设好
/// `WLOG_APPENDER` / `WLOG_FILEAPPENDER_OUTPUT_FILE_PATH`。
///
/// 只开这一个 tag：整个 FreeRDP 的 DEBUG 里画面/键盘每秒几百条，
/// 会把真正有用的剪贴板报文淹掉。
fn enable_freerdp_protocol_log() {
	if !protocol_log_enabled() {
		return;
	}
	// SAFETY: WLog_Get 按名字取/建 logger，返回的句柄由 FreeRDP 自己持有，
	// 这里只设级别，不接管生命周期。
	unsafe {
		let log = ffi::WLog_Get(CLIPRDR_CLIENT_LOG_TAG.as_ptr());
		if log.is_null() {
			tracing::warn!("[剪贴板] 取 FreeRDP cliprdr logger 失败");
			return;
		}
		// DEBUG 才含逐条报文；INFO 只有收发摘要
		if ffi::WLog_SetStringLogLevel(log, c"DEBUG".as_ptr()) != 0 {
			tracing::info!(
				"[剪贴板] 已打开 FreeRDP 剪贴板协议日志（DEBUG，tag={}）",
				CLIPRDR_CLIENT_LOG_TAG.to_string_lossy()
			);
		} else {
			tracing::warn!("[剪贴板] 打开 FreeRDP 协议日志失败");
		}
	}
}

impl RdpClient {
	/// 创建实例与 context。
	///
	/// 这一步只做分配与回调挂载，不建立网络连接。
	pub fn new() -> Result<Self> {
		// SAFETY: freerdp_new 无前置条件，返回新实例或 NULL。
		let instance = unsafe { ffi::freerdp_new() };
		if instance.is_null() {
			return Err(RdpError::Gdi("freerdp_new 失败".to_string()));
		}

		let state = Arc::new(ClientState::new());

		// 扩展 ContextSize：多出的空间放一个 [`ClientExt`]。
		// 注意这里存的是「指向 Arc 内部数据的裸指针」——Arc 本体由 RdpClient
		// 持有，生命周期覆盖整个实例，因此回调期间指针始终有效。
		let certificate = Arc::new(crate::certificate::CertificateGuard::new());
		let ext = ClientExt {
			state: Arc::as_ptr(&state),
			// 剪贴板 / 动态分辨率都要等对应的 `enable_*` 才建好（`connect` 之前），
			// 这里先留空，由那次调用填。
			clipboard: std::ptr::null(),
			display: std::ptr::null(),
			// 证书校验必须**连接之前**就位：握手一开始就会问
			certificate: Arc::as_ptr(&certificate),
		};
		let context_size = mem::size_of::<ffi::rdpContext>() + mem::size_of::<ClientExt>();

		// SAFETY: instance 非空，写字段是 FreeRDP 的既有约定。
		unsafe {
			(*instance).ContextSize = context_size;
			(*instance).ContextNew = Some(context_new_trampoline);
			// 证书校验：**不注册这两个回调就等于不校验**（见 `certificate` 模块说明）
			(*instance).VerifyCertificateEx = Some(verify_certificate_trampoline);
			(*instance).VerifyChangedCertificateEx = Some(verify_changed_certificate_trampoline);
		}

		// SAFETY: instance 非空且 ContextSize 已设。
		let ok = unsafe { ffi::freerdp_context_new(instance) };
		if ok == 0 {
			// 失败也要释放实例，避免泄漏
			// SAFETY: instance 有效。
			unsafe { ffi::freerdp_free(instance) };
			return Err(RdpError::Gdi("freerdp_context_new 失败".to_string()));
		}

		// 把扩展数据写进 context 扩展内存。
		// SAFETY: context 非空，扩展内存已由 ContextSize 预留。
		unsafe {
			*ext_slot_mut((*instance).context) = ext;
		}

		Ok(Self {
			instance,
			state,
			connected: false,
			clipboard: None,
			clipboard_bound: std::sync::atomic::AtomicBool::new(false),
			display: None,
			display_bound: std::sync::atomic::AtomicBool::new(false),
			certificate,
			connect_params: None,
		})
	}

	/// 连接前告知「这台机器以前记录的证书指纹」。
	///
	/// 传 `None` 表示库里没有记录——首次连接，判定为
	/// [`crate::certificate::Verdict::FirstSight`]，连上后由上层写库。
	///
	/// **必须在 `connect` 之前调用**：握手一开始就会问证书。
	pub fn trust_certificate(&self, fingerprint: Option<&str>) {
		self.certificate.trust(fingerprint);
	}

	/// 连接之后取走证书观察结果（首连 / 一致 / 变更）。
	///
	/// 取一次就清空：调用方拿到之后要么写库、要么据此报错，不该反复读。
	#[must_use]
	pub fn certificate_observation(&self) -> Option<crate::certificate::Observation> {
		self.certificate.take()
	}

	/// 写连接参数并建立连接。
	///
	/// `params.password` 只在写入 settings 期间短暂存在，函数返回后
	/// 本地 CString 立即释放（密码不复用）。
	pub fn connect(&mut self, params: &ConnectionParams) -> Result<()> {
		// GCC 的缩放**恒 100**：非 100 会让服务端丢弃之后所有 disp 布局
		// （连分辨率变更一起），而它自己又不会让会话真按该比例渲染。
		// `gcc.c:1511` 证明这个字段确实编进了 Client Core Data —— 服务端
		// 收到但不兑现（2026-11-15 用 200% 实测：图标仍 100%）。
		// 会话内放大由客户端兑现（请求更小的桌面 + 本机放大）。
		const GCC_SCALE: u32 = 100;
		// 存一份给 `enable_clipboard` 用（它必须在 connect 之前调用）
		self.connect_params = Some(params.clone());

		// FreeRDP 的 proxy_prepare 会**无条件**读 https_proxy/HTTPS_PROXY 环境变量，
		// 若存在就把 ProxyType 从 NONE 改成 HTTP（自动发现 HTTPS 代理）。
		// Daisy 的代理走自己的 Dialer（S7），直连时绝不能被环境变量劫持，
		// 因此这里临时清空这些变量，连接期间不受影响。
		let _proxy_guard = ProxyEnvGuard::clear();

		// SAFETY: instance/context 有效。
		let settings = unsafe { (*self.instance).context.as_ref() }.map(|c| c.settings);
		let Some(settings) = settings else {
			return Err(RdpError::InvalidParams("settings 为空".to_string()));
		};

		// 用 CString 保证 NUL 结尾，避免 set_string 读越界。
		let host_c = std::ffi::CString::new(params.host.as_str())
			.map_err(|_| RdpError::InvalidParams("主机名含 NUL 字节".to_string()))?;
		let user_c = std::ffi::CString::new(params.user.as_str())
			.map_err(|_| RdpError::InvalidParams("用户名含 NUL 字节".to_string()))?;
		let pass_c = std::ffi::CString::new(params.password.as_str())
			.map_err(|_| RdpError::InvalidParams("密码含 NUL 字节".to_string()))?;
		let domain_c = std::ffi::CString::new(params.domain.as_str())
			.map_err(|_| RdpError::InvalidParams("域名含 NUL 字节".to_string()))?;

		// SAFETY: settings 非空，各 CString 有效。
		let ok = unsafe {
			ffi::freerdp_settings_set_string(
				settings,
				ffi::FreeRDP_Settings_Keys_String::FreeRDP_ServerHostname,
				host_c.as_ptr(),
			) != 0 && ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_ServerPort,
				u32::from(params.port),
			) != 0 && ffi::freerdp_settings_set_string(
				settings,
				ffi::FreeRDP_Settings_Keys_String::FreeRDP_Username,
				user_c.as_ptr(),
			) != 0 && ffi::freerdp_settings_set_string(
				settings,
				ffi::FreeRDP_Settings_Keys_String::FreeRDP_Password,
				pass_c.as_ptr(),
			) != 0 && ffi::freerdp_settings_set_string(
				settings,
				ffi::FreeRDP_Settings_Keys_String::FreeRDP_Domain,
				domain_c.as_ptr(),
			) != 0 && ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopWidth,
				params.width,
			) != 0 && ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopHeight,
				params.height,
			) != 0
			// **GCC 的缩放必须保持 100**。真机（Server 2019）实测：这里填
			// 非 100 时，服务端把之后**所有** disp 显示器布局（连分辨率
			// 变更一起）静默丢弃；而 GCC 的缩放本身又不会让会话真按它
			// 渲染。DPI 缩放唯一可靠的通道是 disp 布局（见
			// `DisplayControl::set_size`），物理尺寸与它自洽。
			&& ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopScaleFactor,
				GCC_SCALE,
			) != 0 && ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_DeviceScaleFactor,
				GCC_SCALE,
			) != 0 && ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopPhysicalWidth,
				physical_mm(params.width),
			) != 0 && ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopPhysicalHeight,
				physical_mm(params.height),
			) != 0 && ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_ColorDepth,
				32,
			) != 0 && ffi::freerdp_settings_set_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_ProxyType,
				PROXY_TYPE_NONE,
			) != 0 && ffi::freerdp_settings_set_bool(
				settings,
				ffi::FreeRDP_Settings_Keys_Bool::FreeRDP_SoftwareGdi,
				1,
			) != 0 && ffi::freerdp_settings_set_bool(
				settings,
				ffi::FreeRDP_Settings_Keys_Bool::FreeRDP_Authentication,
				1,
			) != 0 && ffi::freerdp_settings_set_bool(
				settings,
				ffi::FreeRDP_Settings_Keys_Bool::FreeRDP_NlaSecurity,
				1,
			) != 0 && ffi::freerdp_settings_set_bool(
				settings,
				ffi::FreeRDP_Settings_Keys_Bool::FreeRDP_TlsSecurity,
				1,
			) != 0 && ffi::freerdp_settings_set_bool(
				settings,
				ffi::FreeRDP_Settings_Keys_Bool::FreeRDP_RdpSecurity,
				1,
			) != 0
		};

		if !ok {
			return Err(RdpError::InvalidParams("写入连接参数失败".to_string()));
		}

		// SAFETY: instance 有效，settings 已写入。
		let connected = unsafe { ffi::freerdp_connect(self.instance) };
		if connected == 0 {
			// 证书被拒时 FreeRDP 只会报一句泛泛的连接错误码，真正的原因在我们这边：
			// 取一次观察结果，把「记录的是 X、这次是 Y」说清楚
			if let Some(observation) = self.certificate.take()
				&& let crate::certificate::Verdict::Changed { expected } = &observation.verdict
			{
				return Err(RdpError::Certificate {
					reason: format!(
						"服务端证书与记录不符——记录为 {}，本次为 {}。可能是重装过系统，\
						 也可能有人在冒充这台机器；确认无误后可先删除该主机的证书记录再连。",
						crate::certificate::format_fingerprint(expected),
						crate::certificate::format_fingerprint(&observation.fingerprint),
					),
				});
			}

			// SAFETY: context 有效。
			let code = unsafe { ffi::freerdp_get_last_error((*self.instance).context) };
			let message = unsafe { ffi::freerdp_get_last_error_string(code) };
			let message = if message.is_null() {
				"未知错误".to_string()
			} else {
				// SAFETY: freerdp_get_last_error_string 返回静态字符串。
				unsafe { std::ffi::CStr::from_ptr(message) }
					.to_string_lossy()
					.into_owned()
			};
			return Err(Self::classify_connect_error(code, message));
		}

		self.connected = true;
		Ok(())
	}

	/// 注入外部剪贴板（S8-63）。
	///
	/// **必须在 `connect` 之前调**：通道装载时要把回调接到这个实例上，
	/// 界面之后读写的也必须是同一个。不注入的话 `connect` 会自建一个，
	/// 界面写的内容就到不了真正装着回调的那一个上。
	pub fn use_clipboard(&mut self, clipboard: std::sync::Arc<crate::clipboard::Clipboard>) {
		self.clipboard = Some(clipboard);
	}

	/// 启用剪贴板通道（S8-63）。**必须在 [`Self::connect`] 之前调。**
	///
	/// **为什么不能连接之后再装**：`freerdp_connect` 内部会调
	/// `utils_reload_channels`（`libfreerdp/core/utils.c:574`），它**free 掉整个
	/// `context->channels` 再重建**，然后调 `instance->LoadChannels` 钩子。
	/// 通道必须在这个钩子里装——连接完成后手动装会拿到
	/// `CHANNEL_RC_ALREADY_CONNECTED`（实测报的正是这个错误）。
	///
	/// 没注入剪贴板就自建一个，库单独用（如探针）时也能有剪贴板。
	pub fn enable_clipboard(&mut self) {
		// 用户关掉了映射（对应 mstsc 那个勾选项）：连通道都不装。
		// 不装的好处是完全没有协议往返，也不必再维护"装了但不用"的状态。
		if self.connect_params.as_ref().is_some_and(|p| !p.clipboard) {
			tracing::debug!("剪贴板映射已关闭，不安装 cliprdr 通道");
			return;
		}
		let clipboard = self
			.clipboard
			.clone()
			.unwrap_or_else(|| std::sync::Arc::new(crate::clipboard::Clipboard::new()));
		// SAFETY: instance 有效；扩展内存已按 ClientExt 预留；Arc 由 self 持有。
		unsafe {
			let context = (*self.instance).context;
			if !context.is_null() {
				ext_slot_mut(context).clipboard = Arc::as_ptr(&clipboard);
				// **必须在 connect 之前宣告「我要剪贴板重定向」**。
				// `RedirectClipboard` 进的是 GCC 能力协商报文——服务端据此
				// 决定要不要给我们发剪贴板。不宣告的话通道能开、能握手、
				// 能发格式列表（服务端还回 OK），但**永远不来索要数据**。
				// 症状极隐蔽：一切看起来正常，只有内容送不过去。
				let settings = (*context).settings;
				if !settings.is_null() {
					ffi::freerdp_settings_set_bool(
						settings,
						ffi::FreeRDP_Settings_Keys_Bool::FreeRDP_RedirectClipboard,
						1,
					);
				}
				// **必须显式开剪贴板功能位**。`ClipboardFeatureMask` 默认是 0，
				// 而 FreeRDP 在 cliprdr 的 8 处（`cliprdr_main.c` / `cliprdr_format.c`）
				// 都要读它来决定放不放行——为 0 就一路丢弃，
				// 表现是「通道握手正常、回调都进得去，但收发全无动静」。
				// 不设的话第一版就是这样：协议跑通，界面一点反应都没有。
				let settings = (*context).settings;
				if !settings.is_null() {
					ffi::freerdp_settings_set_uint32(
						settings,
						ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_ClipboardFeatureMask,
						crate::clipboard::FEATURE_MASK_ALL,
					);
				}
			}
			(*self.instance).LoadChannels = Some(load_channels_trampoline);
		}
		// 日志的落盘路径由应用层在 main 开头设好（早于任何 FreeRDP 调用），
		// 这里只负责把 cliprdr 这个 tag 的级别开到 DEBUG。
		enable_freerdp_protocol_log();
		self.clipboard = Some(clipboard);
	}

	/// 启用动态分辨率（S8-64）。**必须在 [`Self::connect`] 之前调。**
	///
	/// 和剪贴板同理：`freerdp_connect` 会重建 `context->channels`，
	/// 真正的通道加载必须发生在 `LoadChannels` 钩子里（见 [`load_channels_trampoline`]）。
	/// 这里只做两件事：把实例挂进扩展内存、确保钩子已装上。
	pub fn enable_display(&mut self) {
		let display = self
			.display
			.clone()
			.unwrap_or_else(|| std::sync::Arc::new(crate::display::DisplayControl::new()));
		// SAFETY: instance 有效；扩展内存已按 ClientExt 预留；Arc 由 self 持有。
		unsafe {
			let context = (*self.instance).context;
			if !context.is_null() {
				ext_slot_mut(context).display = std::sync::Arc::as_ptr(&display);
			}
			// 与剪贴板共用同一个钩子（重复设置是覆盖，无害）。
			(*self.instance).LoadChannels = Some(load_channels_trampoline);
		}
		self.display = Some(display);
	}

	/// 把 FreeRDP 连接错误码归类成更可读的 [`RdpError`].
	///
	/// FreeRDP 的 `freerdp_get_last_error` 返回 `MAKE_FREERDP_ERROR(CONNECT, x)`
	/// 包装后的值（高 16 位是类别），低 16 位才是 `ERRCONNECT_*`。
	fn classify_connect_error(code: u32, message: String) -> RdpError {
		// `freerdp_get_last_error` 返回的是 `MAKE_FREERDP_ERROR(类别, 码)`，
		// 高位带类别标记（实测连接失败给出 0x00020001）。低 16 位才是
		// `ERRCONNECT_*` 的裸值，见 include/freerdp/error.h。
		let err = code & 0xFFFF;

		// 先处理「说清楚就能自己解决」的那批。文案按
		// `ERRCONNECT_*` 的语义写，一种情况一句，不合并——合并回去就又变成
		// 「用户名或密码不正确」这种误导性提示了。
		let auth_reason = match err {
			0x0009 // AUTHENTICATION_FAILED（NLA/CredSSP 的通用失败）
			| 0x0014 // LOGON_FAILURE
			| 0x0015 // WRONG_PASSWORD
			| 0x0016 => Some("用户名或密码不正确"), // ACCESS_DENIED
			0x000E // PASSWORD_EXPIRED
			| 0x000F => Some("密码已过期，请先在 Windows 上修改密码再连"), // PASSWORD_CERTAINLY_EXPIRED
			0x0012 => Some("该帐户已被禁用，请联系管理员"), // ACCOUNT_DISABLED
			0x0013 => Some("密码已到期，必须先修改密码才能登录"), // PASSWORD_MUST_CHANGE
			0x0017 => Some("登录受到限制（可能是登录时段或工作站限制）"), // ACCOUNT_RESTRICTION
			0x0018 => Some("帐户已被锁定，请稍后再试或联系管理员"), // ACCOUNT_LOCKED_OUT
			0x0019 => Some("帐户已过期，请联系管理员"), // ACCOUNT_EXPIRED
			0x001A => Some("该帐户没有远程桌面登录权限（「允许通过远程桌面服务登录」）"), // LOGON_TYPE_NOT_GRANTED
			0x001B => Some("没有提供凭据，或凭据缺失"), // NO_OR_MISSING_CREDENTIALS
			0x001E => Some("服务端要求更强的认证（NLA / CredSSP），当前连接不被接受"), // HYBRID_REQUIRED_BY_SERVER
			_ => None,
		};
		if let Some(reason) = auth_reason {
			return RdpError::AuthRejected {
				code,
				reason: reason.to_string(),
			};
		}

		// 剩下的是「笼统」的那批：FreeRDP 只知道失败在哪一阶段，不知道原因。
		// 把常见原因摊开写，尤其是 CredSSP 的加密 oracle 策略——它属于
		// SSPI 层失败，FreeRDP 连具体错误码都不设，只在这里写成笼统失败。
		let stage = match err {
			0x0001 => "预连接阶段失败",
			0x0003 => "连接建立后初始化失败（常见于图形层）",
			0x0006 => "TCP 连接失败",
			0x0007 => "MCS 连接初始化失败",
			0x0008 => "TLS 握手失败",
			0x000C => "安全层协商失败",
			0x000D => "传输层失败",
			0x0004 | 0x0005 => "域名解析失败",
			0x001C => "服务端激活超时",
			0x001D => "目标机器正在启动",
			_ => "",
		};
		let hint = match err {
			0x000C => {
				"常见原因：CredSSP 加密 oracle 修复策略不一致（客户端与服务端的补丁级别不同），或服务端禁用了 NLA。"
			},
			0x0008 => "常见原因：服务端只接受 NLA / TLS，或 TLS 版本不匹配。",
			0x0006 | 0x0007 | 0x000D => "常见原因：端口填错、防火墙拦截、目标机器未开机。",
			0x0001 => {
				"常见原因：认证方式与服务端不匹配（服务端可能要 NLA / CredSSP），或凭据被拒绝。"
			},
			0x0003 => "常见原因：分辨率或色深不被接受，或显示通道初始化失败。",
			_ => "",
		};

		let detail = if stage.is_empty() {
			message
		} else {
			format!("{stage}（{message}）")
		};
		let detail = if hint.is_empty() {
			detail
		} else {
			format!("{detail}\n{hint}")
		};
		RdpError::Connect {
			code,
			message: detail,
		}
	}

	/// **真正阻塞** `ms` 毫秒，只做输入发送、不处理事件。
	///
	/// [`Self::pump`] 的语义是「有事件就立刻返回」，
	/// **不能用来做按键之间的定时等待**——它一有事件就回来了，
	/// 实测按键间隔根本加不上（本该 90ms，实际 0.6ms）。
	///
	/// 组合键对时序敏感：Ctrl 按下与 V 按下挨得太近，
	/// 远端会当成同一批处理，于是 Ctrl 被丢掉、只看到字母本身
	/// （用户实测：Windows 里粘出字母 v）。这里用真正的睡眠，
	/// 且期间不碰事件循环，确保顺序与间隔都是确定的。
	pub fn input_gap(&self, ms: u32) {
		if ms > 0 {
			std::thread::sleep(std::time::Duration::from_millis(u64::from(ms)));
		}
	}

	/// 泵事件循环直到有新帧或超时。返回是否还有数据要处理。
	///
	/// 阻塞最多 `wait_ms` 毫秒。返回 `true` 表示事件循环仍在运行，
	/// `false` 表示会话已结束或出错。
	pub fn pump(&self, wait_ms: u32) -> Result<bool> {
		// SAFETY: context 有效。
		let context = unsafe { (*self.instance).context };
		if context.is_null() {
			return Err(RdpError::Disconnected {
				reason: "context 为空".to_string(),
			});
		}

		let mut handles = [ptr::null_mut::<std::ffi::c_void>(); MAXIMUM_WAIT_OBJECTS];
		// SAFETY: handles 缓冲有效，count 为上限。
		let count = unsafe {
			ffi::freerdp_get_event_handles(context, handles.as_mut_ptr(), handles.len() as u32)
		};

		if count == 0 {
			return Err(RdpError::Disconnected {
				reason: "无事件句柄".to_string(),
			});
		}

		// SAFETY: handles[..count] 有效。
		let wait_result =
			unsafe { ffi::WaitForMultipleObjects(count, handles.as_ptr(), 0, wait_ms) };

		// WAIT_FAILED = 0xFFFFFFFF
		if wait_result == u32::MAX {
			return Err(RdpError::Disconnected {
				reason: "等待事件失败".to_string(),
			});
		}

		// SAFETY: context 有效。
		let ok = unsafe { ffi::freerdp_check_event_handles(context) };
		if ok == 0 {
			// SAFETY: context 有效。
			let code = unsafe { ffi::freerdp_get_last_error(context) };
			let message = unsafe { ffi::freerdp_get_last_error_string(code) };
			let message = if message.is_null() {
				"未知错误".to_string()
			} else {
				unsafe { std::ffi::CStr::from_ptr(message) }
					.to_string_lossy()
					.into_owned()
			};
			return Err(RdpError::Disconnected { reason: message });
		}

		self.flush_clipboard();
		self.bind_display();

		Ok(true)
	}

	/// 接管 drdynvc 的通道连接回调（S8-64）。每帧一次，接管后不再查表。
	fn bind_display(&self) {
		let Some(display) = self.display.clone() else {
			return;
		};
		if self
			.display_bound
			.load(std::sync::atomic::Ordering::Relaxed)
		{
			return;
		}
		// SAFETY: instance 有效；且只在会话线程里调用（pump 的契约）。
		let ok = unsafe { display.try_bind(&mut *self.instance) };
		self.display_bound
			.store(ok, std::sync::atomic::Ordering::Relaxed);
	}

	/// 请求远端切换桌面分辨率（S8-64）。
	///
	/// **必须在会话线程调**：`DispClientContext` 不是 `Send`，和剪贴板同理。
	/// 通道没连上时返回错误，调用方据此提示，而不是静默吞掉。
	pub fn resize_desktop(&self, width: u32, height: u32) -> Result<()> {
		let Some(display) = self.display.clone() else {
			return Err(RdpError::Gdi(
				"没有启用动态分辨率（先在 connect 之前调 enable_display）".to_string(),
			));
		};
		display.set_size(width, height)
	}

	/// 动态分辨率通道是否已连上。
	pub fn display_connected(&self) -> bool {
		self.display
			.clone()
			.is_some_and(|display| display.is_connected())
	}

	/// 把待发的剪贴板文本发往远端（S8-63）。
	///
	/// **必须在会话线程调**——`CliprdrClientContext` 不是 `Send`，
	/// 和输入注入同理。每帧一次，没待发内容就是空转。
	/// 立刻把待发的剪贴板内容上报一次（不等到下一帧）。
	///
	/// 供「送文本 + 让远端粘贴」这类必须**严格保序**的场景调用：
	/// 按键走输入通道是立即的，而内容若留在 `pending_out` 里就要等下一次
	/// 泵循环才发出去——那样远端会先收到粘贴动作、再收到内容，
	/// 粘出来是空白。`flush_clipboard` 是私有的，这里是给会话线程的入口。
	pub fn flush_clipboard_now(&self) {
		self.flush_clipboard();
	}

	fn flush_clipboard(&self) {
		let Some(clipboard) = self.clipboard.clone() else {
			return;
		};
		// 通道还没打开就等下一帧——`openDataList` 要握手之后才被填，
		// 所以接管必然晚于 `connect`。接管成功后就不再查通道表。
		let bound = self
			.clipboard_bound
			.load(std::sync::atomic::Ordering::Relaxed);
		if !bound {
			// SAFETY: instance 有效；只在会话线程里被调用。
			// SAFETY: instance 有效，且只在会话线程里调用（pump 的契约）。
			let ok = unsafe { clipboard.try_bind(&mut *self.instance) };
			self.clipboard_bound
				.store(ok, std::sync::atomic::Ordering::Relaxed);
			if !ok {
				return;
			}
		}
		// 拿 context->channels → 通道接口，指针类型转换后交给剪贴板模块
		// SAFETY: instance 有效，connected 为真时 context 非空。
		let interface = unsafe {
			let channels = (*self.instance).context.as_ref().map(|c| c.channels);
			channels
				.filter(|channels| !channels.is_null())
				.map(|channels| {
					ffi::freerdp_channels_get_static_channel_interface(
						channels,
						c"cliprdr".as_ptr(),
					)
				})
				.filter(|interface| !interface.is_null())
		};
		let context = interface.map(|ptr| unsafe { &mut *(ptr as *mut ffi::CliprdrClientContext) });
		clipboard.flush_outgoing(context);
	}

	/// 取走远端剪贴板文本（S8-63）。`None` 表示没有新内容。
	pub fn take_remote_clipboard(&self) -> Option<String> {
		self.clipboard.as_ref()?.take_remote()
	}

	/// 上一条上报的内容**是否已经应答出去了**。
	///
	/// 仅用于诊断与自我验证；**注入 Ctrl+V 的判据是
	/// [`Self::clipboard_format_list_acked`]**——等这个会形成死锁。
	pub fn clipboard_remote_took_data(&self) -> bool {
		self.clipboard
			.as_ref()
			.is_some_and(|clipboard| clipboard.remote_took_data())
	}

	/// 服务端是否已确认我们最近一次上报的格式列表。
	///
	/// **注入 Ctrl+V 之前必须等它为 `true`**：服务端只在用户粘贴时
	/// 才发 `CB_FORMAT_DATA_REQUEST` 索要数据，而那次粘贴正是
	/// 我们注入的按键。先按键后上报 = 远端粘贴到空剪贴板，
	/// 且永远等不到取数请求。
	pub fn clipboard_format_list_acked(&self) -> bool {
		self.clipboard
			.as_ref()
			.is_some_and(|clipboard| clipboard.format_list_acked())
	}

	/// 服务端有没有主动广播过剪贴板变化（远端 → 本地方向是否可用）。
	pub fn clipboard_server_broadcast(&self) -> bool {
		self.clipboard
			.as_ref()
			.is_some_and(|clipboard| clipboard.server_broadcast_ever())
	}

	/// 发送一段文本到远端剪贴板（S8-63）。
	pub fn send_clipboard(&self, text: String) {
		if let Some(clipboard) = &self.clipboard {
			clipboard.send_text(text);
		}
	}

	/// 发送一组本地文件到远端剪贴板（S10）。
	pub fn send_clipboard_files(&self, files: Vec<crate::clipboard::LocalFile>) {
		if let Some(clipboard) = &self.clipboard {
			clipboard.send_files(files);
		}
	}

	/// 会话是否已断开。
	pub fn is_disconnected(&self) -> bool {
		// SAFETY: context 有效。
		unsafe { (*self.instance).context.as_ref() }
			.map(|c| unsafe { ffi::freerdp_shall_disconnect_context(c) != 0 })
			.unwrap_or(true)
	}

	/// 发一个输入事件（按键 / 指针 / 滚轮）。
	///
	/// 必须在**创建它的线程**里调用——`RdpClient` 不是 `Send`，FreeRDP 也
	/// 要求输入与事件泵同线程。app 侧靠 `rdp_session` 的专用线程保证这点。
	///
	/// 未连接或拿不到 `rdpInput` 时返回错误，不静默丢弃——界面据此能提示
	/// 「会话已断开」，而不是让用户对着没反应的画面点。
	pub fn send_input(&self, event: InputEvent) -> Result<()> {
		if !self.connected {
			return Err(RdpError::Disconnected {
				reason: "尚未连接".to_string(),
			});
		}

		// SAFETY: instance 由 Self 持有且非空；connect 成功后 context 有效。
		let context = unsafe { (*self.instance).context };
		if context.is_null() {
			return Err(RdpError::Disconnected {
				reason: "context 为空".to_string(),
			});
		}
		// SAFETY: context 有效，input 由 rdpRdp 持有。
		let input = unsafe { (*context).input };
		if input.is_null() {
			return Err(RdpError::Input("输入接口尚未就绪".to_string()));
		}

		let ok = if let InputEvent::Key { key, down, repeat } = event {
			// 走 `_ex`：扩展位与 KBD_FLAGS_* 的组装交给 FreeRDP，
			// 我们只负责把「哪个键 + 按下/抬起/重复」说清楚
			// SAFETY: input 非空，回调是 FreeRDP 自己的实现。
			unsafe {
				ffi::freerdp_input_send_keyboard_event_ex(
					input,
					down as ffi::BOOL,
					repeat as ffi::BOOL,
					key.scancode().as_u32(),
				)
			}
		} else {
			let (Some(flags), Some((x, y))) = (event.mouse_flags(), event.position()) else {
				return Err(RdpError::Input("指针事件缺少坐标或标志位".to_string()));
			};
			// SAFETY: input 非空。
			unsafe { ffi::freerdp_input_send_mouse_event(input, flags, x, y) }
		};

		if ok == 0 {
			return Err(RdpError::Input("远端拒绝或通道已关闭".to_string()));
		}
		Ok(())
	}

	/// 取最新一帧与它的序号（拷贝）。
	///
	/// 序号让调用方能跳过「还是上一帧」的情况——帧数据是整屏拷贝，
	/// 每帧无条件重取既费内存又费带宽。
	pub fn latest_frame(&self) -> Option<(u64, FrameBuffer)> {
		let guard = self.state.frame.lock().ok()?;
		let seq = self
			.state
			.frame_seq
			.load(std::sync::atomic::Ordering::Acquire);
		guard.as_ref().map(|f| {
			(
				seq,
				FrameBuffer {
					width: f.width,
					height: f.height,
					stride: f.stride,
					data: f.data.clone(),
				},
			)
		})
	}

	/// 取最新一张光标位图与它的序号（拷贝）。
	///
	/// 序号让界面只在**形状变了**时重传纹理——光标位图很小，但鼠标移动很频繁，
	/// 每帧无条件重传纯属浪费。
	pub fn latest_cursor(&self) -> Option<(u64, CursorSprite)> {
		let guard = self.state.cursor.lock().ok()?;
		let seq = self
			.state
			.cursor_seq
			.load(std::sync::atomic::Ordering::Acquire);
		guard.as_ref().cloned().map(|cursor| (seq, cursor))
	}

	/// 当前帧序号。`0` 表示还没有过任何一帧。
	///
	/// 界面先比这个值，与上次渲染时相同就什么都不用做。
	pub fn frame_seq(&self) -> u64 {
		self.state
			.frame_seq
			.load(std::sync::atomic::Ordering::Acquire)
	}

	/// 是否观察到过分辨率变更。
	pub fn resized(&self) -> bool {
		self.state
			.resized
			.load(std::sync::atomic::Ordering::Acquire)
	}
}

impl Drop for RdpClient {
	fn drop(&mut self) {
		// SAFETY: instance 有效。
		unsafe {
			if self.connected {
				ffi::freerdp_disconnect(self.instance);
			}
			ffi::freerdp_context_free(self.instance);
			ffi::freerdp_free(self.instance);
		}
	}
}

// ==================== 回调 trampoline ====================

/// 从 context 扩展内存取回 `ClientState`。
///
/// SAFETY: `context` 必须是我们用 [`RdpClient::new`] 创建、且扩展内存里
/// 写入了有效 `*const ClientState` 的 context。
unsafe fn state_of(context: *mut ffi::rdpContext) -> &'static ClientState {
	let ext = unsafe { ext_slot(context) };
	// SAFETY: 指针由 RdpClient 持有其 Arc，生命周期覆盖整个会话。
	unsafe { &*ext.state }
}

/// `ContextNew`：挂载 PreConnect / PostConnect / PostDisconnect。
///
/// SAFETY: 由 FreeRDP 调用，instance/context 有效。
unsafe extern "C" fn context_new_trampoline(
	instance: *mut ffi::freerdp,
	_context: *mut ffi::rdpContext,
) -> ffi::BOOL {
	// SAFETY: instance 有效，且只在 ContextNew 时调用一次。
	unsafe {
		(*instance).PreConnect = Some(pre_connect_trampoline);
		(*instance).PostConnect = Some(post_connect_trampoline);
		(*instance).PostDisconnect = Some(post_disconnect_trampoline);
	}
	1
}

/// `LoadChannels` 钩子：FreeRDP 在连接过程中**重建 channels 之后**调它。
///
/// 通道必须在这里装——这是channels 刚建好、握手还没开始的位置。
/// 返回值：剪贴板装不上也返回 TRUE，因为它是可选能力，不该拖垮画面与输入。
unsafe extern "C" fn load_channels_trampoline(instance: *mut ffi::freerdp) -> ffi::BOOL {
	// SAFETY: instance 有效；扩展内存已按 ClientExt 预留。
	// 动态分辨率（S8-64）与剪贴板共用这个钩子：它俩都得在
	// `context->channels` 重建之后、握手之前装。
	if let Some(display) = unsafe { display_of(instance) } {
		match unsafe { display.prepare(&mut *instance) } {
			Ok(()) => {},
			// 与剪贴板一致：分辨率是可选能力，装不上不该拖垮画面与输入
			Err(error) => tracing::warn!("动态分辨率通道装载失败：{error}"),
		}
	}
	// SAFETY: 同上。
	let Some(clipboard) = (unsafe { clipboard_of(instance) }) else {
		tracing::warn!("LoadChannels 被调但扩展内存里没有剪贴板");
		return 1;
	};
	// SAFETY: instance 有效，此时 channels 已由 FreeRDP 建好。
	match clipboard.install(unsafe { &mut *instance }) {
		Ok(()) => 1,
		Err(error) => {
			// 返回 FALSE 会让整个连接失败——剪贴板不该拖垮画面与输入
			tracing::warn!("剪贴板通道装载失败：{error}");
			1
		},
	}
}

/// 从 context 扩展内存取回动态分辨率句柄。`None` 表示没启用。
///
/// SAFETY: instance 必须是我们创建的、扩展内存已按 `ClientExt` 预留。
unsafe fn display_of(
	instance: *mut ffi::freerdp,
) -> Option<&'static crate::display::DisplayControl> {
	// SAFETY: instance 有效。
	let context = unsafe { (*instance).context };
	if context.is_null() {
		return None;
	}
	// SAFETY: context 有效，扩展内存已预留。
	let ext = unsafe { ext_slot(context) };
	if ext.display.is_null() {
		return None;
	}
	// SAFETY: 指针由 RdpClient 的 Arc 持有，生命周期覆盖整个会话。
	Some(unsafe { &*ext.display })
}

/// 从 context 扩展内存取回剪贴板。`None` 表示还没启用剪贴板。
///
/// SAFETY: instance 必须是我们创建的、扩展内存已按 `ClientExt` 预留。
unsafe fn clipboard_of(
	instance: *mut ffi::freerdp,
) -> Option<&'static crate::clipboard::Clipboard> {
	// SAFETY: instance 有效。
	let context = unsafe { (*instance).context };
	if context.is_null() {
		return None;
	}
	// SAFETY: context 有效，扩展内存已预留。
	let ext = unsafe { ext_slot(context) };
	if ext.clipboard.is_null() {
		return None;
	}
	// SAFETY: 指针由 RdpClient 的 Arc 持有，生命周期覆盖整个会话。
	Some(unsafe { &*ext.clipboard })
}

/// `PreConnect`：**明确关掉**「忽略证书」这条路（S8-65 已接管校验）。
///
/// 这个开关此前是硬开的（PLAN R17）：自签名测试机不关它连不上。现在改由
/// [`crate::certificate`] 判定——首连接受并记录、指纹变了就拒绝，
/// 所以这里反而要把它设回 0，否则 FreeRDP 会在走到回调之前就直接放行
/// （`tls.c` 里 `IgnoreCertificate` 分支在回调之前）。
///
/// SAFETY: 由 FreeRDP 调用，instance 有效。
unsafe extern "C" fn pre_connect_trampoline(instance: *mut ffi::freerdp) -> ffi::BOOL {
	// SAFETY: instance/context 有效。
	unsafe {
		let settings = (*instance).context.as_ref().map(|c| c.settings);
		match settings {
			Some(s) => ffi::freerdp_settings_set_bool(
				s,
				ffi::FreeRDP_Settings_Keys_Bool::FreeRDP_IgnoreCertificate,
				0,
			),
			None => 0,
		}
	}
}

/// `VerifyCertificateEx`：遇到**库里没有记录**的证书时调用。
///
/// 返回 `2`（只接受本次会话）而不是 `1`（接受并写盘）——事实来源是 Daisy
/// 自己的库，不能让 FreeRDP 另外存一份 `known_hosts`，否则同一台机器在
/// 别处连过一次，这里的判定就变得不可解释了。
///
/// SAFETY: 由 FreeRDP 调用；各指针按 C 约定可能为 NULL。
unsafe extern "C" fn verify_certificate_trampoline(
	instance: *mut ffi::freerdp,
	host: *const std::os::raw::c_char,
	port: u16,
	_common_name: *const std::os::raw::c_char,
	subject: *const std::os::raw::c_char,
	issuer: *const std::os::raw::c_char,
	fingerprint: *const std::os::raw::c_char,
	flags: u32,
) -> u32 {
	// SAFETY: instance 有效；各 C 字符串由调用方持有，只读。
	unsafe {
		verify_certificate(
			instance,
			host,
			port,
			subject,
			issuer,
			fingerprint,
			flags,
			"未知证书",
		)
	}
}

/// `VerifyChangedCertificateEx`：证书与 FreeRDP 自己记录的不一致时调用。
///
/// FreeRDP 的 `known_hosts` 里有旧记录**不代表我们信它**——旧值只写进日志，
/// 判定仍然只看 Daisy 库里存的指纹。
///
/// SAFETY: 同 [`verify_certificate_trampoline`]。
unsafe extern "C" fn verify_changed_certificate_trampoline(
	instance: *mut ffi::freerdp,
	host: *const std::os::raw::c_char,
	port: u16,
	_common_name: *const std::os::raw::c_char,
	subject: *const std::os::raw::c_char,
	issuer: *const std::os::raw::c_char,
	new_fingerprint: *const std::os::raw::c_char,
	old_subject: *const std::os::raw::c_char,
	_old_issuer: *const std::os::raw::c_char,
	old_fingerprint: *const std::os::raw::c_char,
	flags: u32,
) -> u32 {
	// 旧值只作日志线索：说明这台机器在**别的客户端**里被信任过
	// SAFETY: 两个指针都由 FreeRDP 提供，可能为 NULL，`c_string` 已处理。
	let (old_subject, old_fingerprint) =
		unsafe { (c_string(old_subject), c_string(old_fingerprint)) };
	tracing::debug!(
		"[证书] FreeRDP 自有记录与本次不同：旧主体 {} 旧指纹 {}",
		old_subject,
		crate::certificate::format_fingerprint(&crate::certificate::normalize(&old_fingerprint))
	);

	// SAFETY: 同 `verify_certificate_trampoline`。
	unsafe {
		verify_certificate(
			instance,
			host,
			port,
			subject,
			issuer,
			new_fingerprint,
			flags,
			"证书变更",
		)
	}
}

/// 两个证书回调的共同实现。返回 FreeRDP 期望的 `DWORD`：2 = 仅本次接受，0 = 拒绝。
///
/// SAFETY: 调用方保证 `instance` 有效。
#[allow(clippy::too_many_arguments)]
unsafe fn verify_certificate(
	instance: *mut ffi::freerdp,
	host: *const std::os::raw::c_char,
	port: u16,
	subject: *const std::os::raw::c_char,
	issuer: *const std::os::raw::c_char,
	fingerprint: *const std::os::raw::c_char,
	flags: u32,
	source: &str,
) -> u32 {
	// `VERIFY_CERT_FLAG_FP_IS_PEM`：说明设置里要求传 PEM 而不是指纹。
	// 我们没开 `CertificateCallbackPreferPEM`，正常走不到；真走到了就拒绝——
	// 拿 PEM 文本当指纹比对必然失配，至少要让它失败得明明白白。
	const VERIFY_CERT_FLAG_FP_IS_PEM: u32 = 0x200;
	if flags & VERIFY_CERT_FLAG_FP_IS_PEM != 0 {
		tracing::error!("[证书] FreeRDP 传的是 PEM 而非指纹，无法比对，拒绝连接");
		return 0;
	}

	// SAFETY: 各指针由 FreeRDP 提供，可能为 NULL，`c_string` 已处理。
	let fingerprint = unsafe { c_string(fingerprint) };
	if fingerprint.is_empty() {
		tracing::error!("[证书] 回调没有给出指纹（{source}），拒绝连接");
		return 0;
	}

	// SAFETY: instance 有效，扩展内存已按 ClientExt 预留。
	let guard = unsafe {
		let context = (*instance).context;
		if context.is_null() {
			return 0;
		}
		let ext = ext_slot(context);
		if ext.certificate.is_null() {
			return 0;
		}
		&*ext.certificate
	};

	// 握手时会话可能还没完全建立，host 偶尔为空——用空串不影响判定（判定只看指纹）
	// SAFETY: 同上面的 c_string 调用。
	let (host, subject, issuer) = unsafe { (c_string(host), c_string(subject), c_string(issuer)) };
	let accepted = guard.observe(
		&host,
		port,
		&fingerprint,
		Some(subject).filter(|text| !text.is_empty()),
		Some(issuer).filter(|text| !text.is_empty()),
	);

	tracing::info!(
		"[证书] {source}：{}:{} → {}",
		host,
		port,
		if accepted {
			"接受（仅本次）"
		} else {
			"拒绝（指纹与记录不符）"
		}
	);

	if accepted { 2 } else { 0 }
}

/// 读一个可能为 NULL 的 C 字符串；NULL 当空串。
///
/// SAFETY: `ptr` 要么为 NULL，要么是 NUL 结尾的有效 C 字符串。
unsafe fn c_string(ptr: *const std::os::raw::c_char) -> String {
	if ptr.is_null() {
		return String::new();
	}
	// SAFETY: 调用方保证非 NULL 时是有效 C 字符串。
	unsafe { std::ffi::CStr::from_ptr(ptr) }
		.to_string_lossy()
		.into_owned()
}

/// `PostConnect`：初始化 GDI，挂绘制回调。
///
/// SAFETY: 由 FreeRDP 调用，instance 有效。
unsafe extern "C" fn post_connect_trampoline(instance: *mut ffi::freerdp) -> ffi::BOOL {
	// SAFETY: gdi_init 用 XRGB32 格式初始化图形层。
	let ok = unsafe { ffi::gdi_init(instance, PIXEL_FORMAT_XRGB32) };
	if ok == 0 {
		return 0;
	}

	// SAFETY: context/update 有效。
	unsafe {
		let context = (*instance).context;
		let update = (*context).update;
		if update.is_null() {
			return 0;
		}
		(*update).BeginPaint = Some(begin_paint_trampoline);
		(*update).EndPaint = Some(end_paint_trampoline);
		(*update).DesktopResize = Some(desktop_resize_trampoline);

		// 光标：RDP 把它当独立消息发，不合成进画面，得自己收自己画
		let pointer = (*update).pointer;
		if !pointer.is_null() {
			(*pointer).PointerPosition = Some(pointer_position_trampoline);
			(*pointer).PointerSystem = Some(pointer_system_trampoline);
			(*pointer).PointerColor = Some(pointer_color_trampoline);
			(*pointer).PointerNew = Some(pointer_new_trampoline);
			(*pointer).PointerCached = Some(pointer_cached_trampoline);
			(*pointer).PointerLarge = Some(pointer_large_trampoline);
		} else {
			tracing::warn!("update->pointer 为空，光标不会显示");
		}
	}
	1
}

/// 由（指针, 长度）造切片。空指针或零长度给空切片——
/// 上游数据不可信，不能让它变成 UB。
unsafe fn slice_from<'a>(data: *const u8, length: usize) -> &'a [u8] {
	if data.is_null() || length == 0 {
		return &[];
	}
	unsafe { std::slice::from_raw_parts(data, length) }
}

/// 解码并缓存一张光标位图。
///
/// `cache_index` 是这条消息里的服务端缓存下标：留一份进 [`ClientState::cursor_cache`]，
/// 之后同一个形状再切回来时服务端只会发 `Pointer Cached Update`（只有下标），
/// 得从这份表里取回位图。
fn store_cursor(
	state: &ClientState,
	bitmap: &CursorBitmap<'_>,
	bpp: u16,
	cache_index: Option<u32>,
) {
	match decode(bitmap, bpp) {
		Some(sprite) => {
			tracing::debug!(
				"收到光标位图：{}x{} 热点({}, {}) {bpp}bpp 下标{cache_index:?}",
				sprite.width,
				sprite.height,
				sprite.hot_x,
				sprite.hot_y
			);
			if let Some(index) = cache_index
				&& let Ok(mut cache) = state.cursor_cache.lock()
			{
				// 缓存满了就整体清掉再放：光标位图都很小（几十 KB），
				// 宁可偶尔重新下发，也不要因为占满而让新形状存不进去。
				if cache.len() >= CURSOR_CACHE_MAX {
					tracing::debug!("光标缓存已满（{CURSOR_CACHE_MAX} 条），清空后重放");
					cache.clear();
				}
				cache.insert(index, sprite.clone());
			}
			if let Ok(mut slot) = state.cursor.lock() {
				*slot = Some(sprite);
				state
					.cursor_seq
					.fetch_add(1, std::sync::atomic::Ordering::Release);
			}
		},
		None => tracing::warn!(
			"光标位图解码失败（{bpp} bpp，{}x{}）",
			bitmap.width,
			bitmap.height
		),
	}
}

/// 按服务端下标取回之前见过的光标位图，并把它设为当前光标。
///
/// 返回 `false` 表示缓存里没有这张图（服务端换了下标分配策略，或我们中途
/// 清过缓存）——此时保持现有形状不变，好过画出一个错的形状。
fn restore_cached_cursor(state: &ClientState, index: u32) -> bool {
	let Some(sprite) = state
		.cursor_cache
		.lock()
		.ok()
		.and_then(|cache| cache.get(&index).cloned())
	else {
		tracing::warn!("光标切到缓存下标 {index}，但本地没有这张位图");
		return false;
	};
	tracing::debug!(
		"光标切到缓存下标 {index}：{}x{}",
		sprite.width,
		sprite.height
	);
	if let Ok(mut slot) = state.cursor.lock() {
		*slot = Some(sprite);
		state
			.cursor_seq
			.fetch_add(1, std::sync::atomic::Ordering::Release);
	}
	true
}

/// `Pointer Position Update`：只报位置，不带位图。
///
/// 位置我们本来就知道（每次移动都是自己发出去的），所以这里只记一笔日志；
/// 服务端偶尔会主动挪位置（分辨率变化后的重定位），真要跟随时再接。
unsafe extern "C" fn pointer_position_trampoline(
	_context: *mut ffi::rdpContext,
	pointer: *const ffi::POINTER_POSITION_UPDATE,
) -> ffi::BOOL {
	let update = unsafe { pointer.as_ref() };
	if let Some(update) = update {
		tracing::trace!("光标位置更新：({}, {})", update.xPos, update.yPos);
	}
	1
}

/// `Pointer System Pointer Update`：`SYSPTR_NULL` 表示服务端让光标消失。
unsafe extern "C" fn pointer_system_trampoline(
	context: *mut ffi::rdpContext,
	pointer: *const ffi::POINTER_SYSTEM_UPDATE,
) -> ffi::BOOL {
	if let Some(update) = unsafe { pointer.as_ref() }
		&& update.type_ == 0
		&& let Ok(mut slot) = unsafe { state_of(context) }.cursor.lock()
	{
		*slot = None;
		tracing::debug!("服务端要求隐藏光标");
	}
	1
}

/// `Pointer Color Update`（RDP 5.0 的经典光标，1bpp AND/XOR 双掩码）。
unsafe extern "C" fn pointer_color_trampoline(
	context: *mut ffi::rdpContext,
	pointer: *const ffi::POINTER_COLOR_UPDATE,
) -> ffi::BOOL {
	let Some(update) = (unsafe { pointer.as_ref() }) else {
		return 1;
	};
	let bitmap = CursorBitmap {
		width: update.width,
		height: update.height,
		hot_x: update.hotSpotX,
		hot_y: update.hotSpotY,
		xor_mask: unsafe { slice_from(update.xorMaskData, update.lengthXorMask as usize) },
		and_mask: unsafe { slice_from(update.andMaskData, update.lengthAndMask as usize) },
	};
	store_cursor(
		unsafe { state_of(context) },
		&bitmap,
		1,
		Some(u32::from(update.cacheIndex)),
	);
	1
}

/// `Pointer Large Update`（RDP 8.0 的大光标，位深由 `xorBpp` 给）。
unsafe extern "C" fn pointer_large_trampoline(
	context: *mut ffi::rdpContext,
	pointer: *const ffi::POINTER_LARGE_UPDATE,
) -> ffi::BOOL {
	let Some(update) = (unsafe { pointer.as_ref() }) else {
		return 1;
	};
	let bitmap = CursorBitmap {
		width: update.width,
		height: update.height,
		hot_x: update.hotSpotX,
		hot_y: update.hotSpotY,
		xor_mask: unsafe { slice_from(update.xorMaskData, update.lengthXorMask as usize) },
		and_mask: unsafe { slice_from(update.andMaskData, update.lengthAndMask as usize) },
	};
	store_cursor(
		unsafe { state_of(context) },
		&bitmap,
		update.xorBpp,
		Some(u32::from(update.cacheIndex)),
	);
	1
}

/// `Pointer Attribute Update`：`xorBpp` 在外层，颜色属性是经典光标结构。
unsafe extern "C" fn pointer_new_trampoline(
	context: *mut ffi::rdpContext,
	pointer: *const ffi::POINTER_NEW_UPDATE,
) -> ffi::BOOL {
	let Some(update) = (unsafe { pointer.as_ref() }) else {
		return 1;
	};
	let attr = &update.colorPtrAttr;
	let bitmap = CursorBitmap {
		width: attr.width,
		height: attr.height,
		hot_x: attr.hotSpotX,
		hot_y: attr.hotSpotY,
		xor_mask: unsafe { slice_from(attr.xorMaskData, attr.lengthXorMask as usize) },
		and_mask: unsafe { slice_from(attr.andMaskData, attr.lengthAndMask as usize) },
	};
	store_cursor(
		unsafe { state_of(context) },
		&bitmap,
		update.xorBpp as u16,
		Some(u32::from(attr.cacheIndex)),
	);
	1
}

/// `Pointer Cached Update`：**只给缓存下标，不带位图**。
///
/// 这是「光标形状卡住不换回」的根源：服务端对一个已经下发过的形状，
/// 第二次切回来时只发下标（`PTR_MSG_TYPE_CACHED`），不再重复发位图。
/// 之前这里只记了条trace 就返回，于是形状一直停在上一次真正拿到过位图的那一个
/// ——比如移出窗口边缘后箭头不回来。
///
/// 位图从我们自己的 [`ClientState::cursor_cache`] 取：FreeRDP 自带的
/// `pointer_cache` 在这里帮不上忙，因为它的写入回调（`update_pointer_color` 等）
/// 已经在 `PostConnect` 里被我们覆盖，缓存永远不会被填。
unsafe extern "C" fn pointer_cached_trampoline(
	context: *mut ffi::rdpContext,
	pointer: *const ffi::POINTER_CACHED_UPDATE,
) -> ffi::BOOL {
	if let Some(update) = unsafe { pointer.as_ref() } {
		restore_cached_cursor(unsafe { state_of(context) }, update.cacheIndex);
	}
	1
}

/// `PostDisconnect`：释放 GDI。
///
/// SAFETY: 由 FreeRDP 调用，instance 有效。
unsafe extern "C" fn post_disconnect_trampoline(instance: *mut ffi::freerdp) {
	// SAFETY: instance 有效。
	unsafe { ffi::gdi_free(instance) };
}

/// `BeginPaint`：标记无效区域。
///
/// SAFETY: 由 FreeRDP 调用，context 有效。
unsafe extern "C" fn begin_paint_trampoline(context: *mut ffi::rdpContext) -> ffi::BOOL {
	// SAFETY: context 有效。
	unsafe {
		let gdi = (*context).gdi;
		if gdi.is_null() {
			return 1;
		}
		// 目前无需处理无效区域，直接返回成功
	}
	1
}

/// `EndPaint`：整屏拷贝一帧到 `ClientState`。
///
/// SAFETY: 由 FreeRDP 调用，context 有效。
unsafe extern "C" fn end_paint_trampoline(context: *mut ffi::rdpContext) -> ffi::BOOL {
	// SAFETY: context 有效。
	let gdi = unsafe { (*context).gdi };
	if gdi.is_null() {
		return 1;
	}

	// SAFETY: gdi 有效。
	let (buffer, width, height, stride) = unsafe {
		let g = &*gdi;
		(g.primary_buffer, g.width, g.height, g.stride)
	};

	if buffer.is_null() || width <= 0 || height <= 0 || stride == 0 {
		return 1;
	}

	// SAFETY: state 由我们创建，指针有效。
	let state = unsafe { state_of(context) };

	let bytes = (stride as usize).saturating_mul(height as usize);
	// SAFETY: primary_buffer 指向 stride*height 字节的可读内存。
	let data = unsafe { std::slice::from_raw_parts(buffer, bytes) }.to_vec();

	if let Ok(mut guard) = state.frame.lock() {
		*guard = Some(FrameBuffer {
			width: width as u32,
			height: height as u32,
			stride,
			data,
		});
		// 序号在锁内递增：界面比对序号与读帧之间不能有窗口，
		// 否则可能读到「新序号 + 旧帧」而漏掉一帧
		state
			.frame_seq
			.fetch_add(1, std::sync::atomic::Ordering::Release);
	}
	1
}

/// `DesktopResize`：服务端分辨率变更，重建 GDI 缓冲。
///
/// SAFETY: 由 FreeRDP 调用，context 有效。
unsafe extern "C" fn desktop_resize_trampoline(context: *mut ffi::rdpContext) -> ffi::BOOL {
	// SAFETY: context 有效。
	let (settings, gdi) = unsafe { ((*context).settings, (*context).gdi) };
	if settings.is_null() || gdi.is_null() {
		return 0;
	}

	// SAFETY: settings/gdi 有效。
	let (width, height) = unsafe {
		(
			ffi::freerdp_settings_get_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopWidth,
			),
			ffi::freerdp_settings_get_uint32(
				settings,
				ffi::FreeRDP_Settings_Keys_UInt32::FreeRDP_DesktopHeight,
			),
		)
	};

	// SAFETY: state 有效。
	let state = unsafe { state_of(context) };
	state
		.resized
		.store(true, std::sync::atomic::Ordering::Release);

	// SAFETY: gdi 有效。
	unsafe { ffi::gdi_resize(gdi, width, height) }
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 协议日志开关默认关——常开会把 `$TMPDIR` 灌满报文。
	///
	/// 判据只认「明确表示开」的写法：`0` / `off` / `false` / `no` / 空串
	/// 都算关，大小写和前后空格不影响。
	#[test]
	fn protocol_log_switch_defaults_to_off() {
		assert!(!parse_switch(None), "没设变量必须关");
		for off in ["", " ", "0", "off", "OFF", "false", "no", " No "] {
			assert!(!parse_switch(Some(off)), "{off:?} 应该算关");
		}
		for on in ["1", "on", "true", "yes", "DEBUG"] {
			assert!(parse_switch(Some(on)), "{on:?} 应该算开");
		}
	}

	/// 手写常量必须对得上头文件里的宏。
	///
	/// 这个值曾经被写成 0（= `PIXEL_FORMAT_NONE`），导致 PostConnect 的
	/// `gdi_init` 判成 UNKNOWN 而整条连接失败——真机上花了半轮才定位到，
	/// 所以钉在这里：算错了单元测试立刻红。
	#[test]
	fn pixel_format_matches_the_header() {
		// 按 vendor/dist/freerdp-macos-arm64/include/freerdp3/freerdp/codec/color.h：
		// (32 << 24) | (TYPE_ARGB = 1 << 16) | (0 << 12) | (8 << 8) | (8 << 4) | 8
		assert_eq!(PIXEL_FORMAT_XRGB32, 0x2001_0888);
		assert_ne!(
			PIXEL_FORMAT_XRGB32, 0x0000_0000,
			"0 是 PIXEL_FORMAT_NONE，会让 gdi_init 失败"
		);
	}

	/// 创建 / 销毁不崩溃、不泄漏（步骤 58 的验证标准）。
	///
	/// 这是纯内存操作，不联网：`freerdp_new` → `context_new` → `free`。
	/// 若生命周期顺序写错（先 free 实例再 free context 之类），这里会直接崩。
	#[test]
	fn create_and_drop_does_not_leak() {
		for _ in 0..32 {
			let client = RdpClient::new().expect("创建应成功");
			// 未连接时 latest_frame 应为 None
			assert!(client.latest_frame().is_none());
			assert!(!client.resized());
			// 未连接时应视为已断开（无 context 事件）
			drop(client);
		}
	}

	/// 造一张单色光标位图，宽高各 `size`，不透明。
	fn mono_bitmap(size: u16) -> CursorBitmap<'static> {
		// AND 全 0 → 完全不透明；XOR 全 1 → 全白。行尾按 2 字节对齐补齐。
		let stride = crate::cursor::row_bytes(size, 1);
		let len = stride * usize::from(size);
		let xor: &'static [u8] = Box::leak(vec![0xFFu8; len].into_boxed_slice());
		let and: &'static [u8] = Box::leak(vec![0x00u8; len].into_boxed_slice());
		CursorBitmap {
			width: size,
			height: size,
			hot_x: 0,
			hot_y: 0,
			xor_mask: xor,
			and_mask: and,
		}
	}

	/// 光标位图按服务端下标存取：形状切回来能取回原来那张。
	///
	/// 这正是「移出窗口边缘后光标不换回箭头」那个 bug 的修复点——
	/// 服务端对复用过的形状只发下标（`Pointer Cached Update`），不发位图。
	#[test]
	fn cached_cursor_round_trips_through_its_index() {
		let state = ClientState::new();
		let bitmap = mono_bitmap(2);
		store_cursor(&state, &bitmap, 1, Some(7));

		// 先切到别的形状（模拟移出边缘），再切回来
		store_cursor(&state, &mono_bitmap(4), 1, Some(9));
		let before = state.cursor_seq.load(std::sync::atomic::Ordering::Acquire);

		assert!(restore_cached_cursor(&state, 7), "下标 7 应当能取回");
		assert!(
			state.cursor_seq.load(std::sync::atomic::Ordering::Acquire) > before,
			"恢复形状必须递增序号，否则界面不会重画"
		);
		let guard = state.cursor.lock().expect("锁不应 poisoned");
		let sprite = guard.as_ref().expect("应当有光标");
		assert_eq!(
			(sprite.width, sprite.height),
			(2, 2),
			"应当取回下标 7 对应的 2x2，而不是下标 9 的 4x4"
		);
	}

	/// 缓存里没有的下标不能瞎猜，保持原形状。
	#[test]
	fn unknown_cursor_index_keeps_the_current_shape() {
		let state = ClientState::new();
		store_cursor(&state, &mono_bitmap(2), 1, Some(3));

		let before = state.cursor_seq.load(std::sync::atomic::Ordering::Acquire);
		assert!(
			!restore_cached_cursor(&state, 999),
			"没见过的下标应返回 false"
		);
		assert_eq!(
			state.cursor_seq.load(std::sync::atomic::Ordering::Acquire),
			before,
			"取不到位图时不能递增序号"
		);
		let guard = state.cursor.lock().expect("锁不应 poisoned");
		assert_eq!(guard.as_ref().expect("应当仍有光标").width, 2);
	}

	/// 缓存满了要还能存新形状，不能因为占满就把后来的形状全丢掉。
	#[test]
	fn cursor_cache_evicts_instead_of_refusing() {
		let state = ClientState::new();
		for index in 0..=CURSOR_CACHE_MAX as u32 {
			store_cursor(&state, &mono_bitmap(2), 1, Some(index));
		}
		// 刚好塞满之后再来一个，必须还能放进去
		store_cursor(&state, &mono_bitmap(2), 1, Some(9999));
		assert!(restore_cached_cursor(&state, 9999));
	}

	/// 错误码分类：**带类别位**的真实值（FreeRDP 给的是 `0x0002xxxx`），
	/// 分类必须按低 16 位判。
	#[test]
	fn classifies_real_error_codes_with_class_bits() {
		// 实测连接失败时 freerdp_get_last_error 返回 0x00020001
		assert!(matches!(
			RdpClient::classify_connect_error(0x0002_0001, "x".to_string()),
			RdpError::Connect { .. }
		));
		assert!(matches!(
			RdpClient::classify_connect_error(0x0002_0014, "x".to_string()),
			RdpError::AuthRejected { .. }
		));
	}

	#[test]
	fn auth_errors_say_what_to_do() {
		let cases = [
			(0x0002_0014u32, "用户名或密码"),
			(0x0002_0012, "被禁用"),
			(0x0002_0013, "修改密码"),
			(0x0002_0018, "锁定"),
			(0x0002_001A, "远程桌面登录权限"),
			(0x0002_001E, "CredSSP"),
		];
		for (code, keyword) in cases {
			let error = RdpClient::classify_connect_error(code, String::new());
			let text = error.to_string();
			assert!(
				text.contains(keyword),
				"错误码 0x{code:08X} 的文案应包含「{keyword}」，实际：{text}"
			);
		}
	}

	/// CredSSP 谈崩时 FreeRDP 只给「安全层协商失败」这种笼统码，
	/// 文案里必须点到加密 oracle 策略这条最常见的坑，否则用户无从下手。
	#[test]
	fn security_negotiation_error_mentions_credssp() {
		let error =
			RdpClient::classify_connect_error(0x0002_000C, "SEC_E_ALGORITHM_MISMATCH".to_string());
		let text = error.to_string();
		assert!(text.contains("加密 oracle"), "实际：{text}");
		assert!(
			text.contains("SEC_E_ALGORITHM_MISMATCH"),
			"原始信息要保留：{text}"
		);
	}

	#[test]
	fn tcp_failure_suggests_port_and_firewall() {
		let text = RdpClient::classify_connect_error(0x0002_0006, "connect failed".to_string())
			.to_string();
		assert!(text.contains("端口"), "实际：{text}");
	}

	/// 回调必须真的挂在 `freerdp` 实例上——删掉注册就等于「不校验」，
	/// 而这种删除不会有任何编译错误。
	#[test]
	fn certificate_callbacks_are_registered() {
		let client = RdpClient::new().expect("创建实例失败");
		// SAFETY: instance 由 RdpClient 持有且非空。
		unsafe {
			assert!(
				(*client.instance).VerifyCertificateEx.is_some(),
				"未知证书回调没注册，证书将不被校验"
			);
			assert!(
				(*client.instance).VerifyChangedCertificateEx.is_some(),
				"证书变更回调没注册"
			);
		}
	}

	/// 走一遍 trampoline 的真实路径：instance → context 扩展内存 → guard。
	///
	/// 纯函数单测覆盖不到这段指针搬运，而它正是最容易写错的地方
	/// （扩展内存偏移、空指针判断）。
	#[test]
	fn trampoline_accepts_first_sight_and_rejects_changed() {
		let client = RdpClient::new().expect("创建实例失败");
		let host = c"10.0.0.9";
		let fingerprint = c"aa:bb:cc:dd";
		let call = |expected: Option<&str>| {
			client.trust_certificate(expected);
			// SAFETY: instance 有效；三个可为 NULL 的字段传空。
			unsafe {
				verify_certificate_trampoline(
					client.instance,
					host.as_ptr(),
					3389,
					std::ptr::null(),
					std::ptr::null(),
					std::ptr::null(),
					fingerprint.as_ptr(),
					0,
				)
			}
		};

		assert_eq!(call(None), 2, "首连应接受（2 = 仅本次会话）");
		// 首连的观察结果会被上层取走，不取会留在槽里，这里手动清一次
		let _ = client.certificate_observation();

		assert_eq!(
			call(Some("aabbccdd")),
			2,
			"规范化后一致（冒号/大小写不同）应接受"
		);
		let _ = client.certificate_observation();

		assert_eq!(call(Some("deadbeef")), 0, "指纹变了必须拒绝");
		let seen = client.certificate_observation().expect("应有观察结果");
		assert!(matches!(
			seen.verdict,
			crate::certificate::Verdict::Changed { .. }
		));
	}

	/// 抗锯齿：FreeRDP 若改传 PEM（`VERIFY_CERT_FLAG_FP_IS_PEM`），
	/// 拿 PEM 文本当指纹比对必然失配。宁可明确拒绝，也不能悄悄放行。
	#[test]
	fn trampoline_rejects_pem_payload() {
		let client = RdpClient::new().expect("创建实例失败");
		let host = c"10.0.0.9";
		let pem = c"-----BEGIN CERTIFICATE-----";
		// SAFETY: 同上。
		let rc = unsafe {
			verify_certificate_trampoline(
				client.instance,
				host.as_ptr(),
				3389,
				std::ptr::null(),
				std::ptr::null(),
				std::ptr::null(),
				pem.as_ptr(),
				0x200,
			)
		};
		assert_eq!(rc, 0, "PEM 载荷必须拒绝");
	}

	/// 空指纹同理：不能因为「都为空」就判定一致。
	#[test]
	fn trampoline_rejects_empty_fingerprint() {
		let client = RdpClient::new().expect("创建实例失败");
		let host = c"10.0.0.9";
		let empty = c"";
		client.trust_certificate(Some("aabbccdd"));
		// SAFETY: 同上。
		let rc = unsafe {
			verify_certificate_trampoline(
				client.instance,
				host.as_ptr(),
				3389,
				std::ptr::null(),
				std::ptr::null(),
				std::ptr::null(),
				empty.as_ptr(),
				0,
			)
		};
		assert_eq!(rc, 0, "空指纹必须拒绝");
	}
}
