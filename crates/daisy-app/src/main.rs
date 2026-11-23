//! Daisy 桌面应用入口。
//!
//! 布局（egui 0.36：面板统一为 `Panel`，`CentralPanel` 必须最后添加）：
//!
//! ```text
//! ┌──────────┬────────────────────────────────┐
//! │ 主机/代理 │ 标签页栏（会话 + 「+ 本地终端」）  │ Panel::top
//! │          ├────────────────────────────────┤
//! │（侧边栏） │        活动终端                 │ Panel::left + CentralPanel
//! │          │                                │
//! ├──────────┴────────────────────────────────┤
//! │ 状态栏（库状态 / 主机数 / 会话数 / 提示）    │ Panel::bottom
//! └───────────────────────────────────────────┘
//! ```
//!
//! **面板的添加顺序决定嵌套关系**：状态栏最先（才能横跨整个宽度），
//! 其次侧边栏，然后标签栏——这时标签栏只占侧边栏右侧那块，
//! 与下方终端左对齐。顺序一反，标签就骑到分组树上面去了。
//!
//! 会话生命周期：本地 PTY 与 SSH 远端 shell 都实现 `daisy_term::SessionLink`，
//! 因此界面层只见 `TerminalView`，分不清字节来自本地还是远程。
//!
//! 设 `DAISY_SMOKE_MS=<毫秒>` 可让窗口在指定时间后自动关闭，
//! 用于无人值守地验证「窗口能正常启动」。CI 与本地质量门都依赖它。

// Windows 上是**图形界面程序**，不是控制台程序。
//
// 不加这句，双击 daisy.exe 会先弹一个黑框标题栏控制台，窗口关掉以后
// 那个黑框还挂在桌面上——看起来像程序没退干净。加了以后由系统按
// GUI 子系统加载（`IMAGE_SUBSYSTEM_WINDOWS_GUI`）。
//
// 代价：Windows 上 `eprintln!` 不再有可见去处（没有控制台可写）。
// 这正是我们把日志落盘的根本原因——诊断信息走日志文件，不走标准错误。
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use daisy_core::config::{AppConfig, AppPaths};
use daisy_core::logging::{self, Logging};
use daisy_core::model::{AuthMethod, Group, Host, Protocol, Proxy, Resolution, ScaleSettings};
use daisy_net::{ProxyAuth, ProxyEndpoint, build_dialer};
use daisy_rdp::{ConnectionParams, RdpTarget};
use daisy_ssh::{AuthSpec, HostKeyPolicy, RsaSigningPolicy, SshParams, SshTarget, TrustedHostKey};
use daisy_store::HostSecrets;

mod connect;
#[cfg(target_os = "macos")]
mod deferred_clipboard_mac;
#[cfg(target_os = "windows")]
mod deferred_clipboard_win;
mod design;
mod error;
mod font_catalog;
mod fonts;
mod pasteboard_files;
mod placement;
mod rdp_session;
mod session_mgr;
mod sftp_session;
mod store;
mod terminal;
mod theme;
mod views;

use connect::ProbeOutcome;
use session_mgr::SessionManager;
use store::Store;
use theme::{ColorMode, ScaleBase};
use views::host_form::{HostFormInput, HostFormOutcome, HostFormState};
use views::host_list::{HostListAction, HostListState};
use views::proxy_form::{ProxyFormInput, ProxyFormOutcome, ProxyFormState};
use views::proxy_list::{ProxyListAction, ProxyListState};
use views::rdp_view::RdpAction;
use views::settings::{self, SettingsOutcome};
use views::toast::ToastState;

/// 烟雾测试时长所在的环境变量名。
const SMOKE_ENV: &str = "DAISY_SMOKE_MS";

/// 无人值守验证：往当前活动终端里敲一串文本。
///
/// 合成键盘事件要辅助功能授权（`CGEventPost` 实测连光标都移不动），
/// 脚本化点击也被拦，所以「敲一句 `exit`，看断开提示长什么样」这件事
/// 只能从应用自己的输入通道灌进去。取值按字面文本处理，`\n` 换成回车。
const SMOKE_TERM_INPUT_ENV: &str = "DAISY_SMOKE_TERM_INPUT";

/// 首帧之后等多久才注入文本。
///
/// 登录 shell 要读 profile、装插件，打出提示符的时间从几百毫秒到几秒都有
/// （实测同一台机器冷启动 4 秒还在跑）。给 2.5 秒是个折中：太早写进去，
/// 字符会先被 tty 的行规程回显、等壳起来才执行；太晚则截不到图。
const SMOKE_TERM_INPUT_DELAY: Duration = Duration::from_millis(2500);

/// 窗口默认尺寸（逻辑点）。仅在配置里没有可用尺寸时使用。
const WINDOW_SIZE: [f32; 2] = [1180.0, 760.0];

/// 窗口最小尺寸（逻辑点）。
const WINDOW_MIN_SIZE: [f32; 2] = [860.0, 520.0];

/// 恢复窗口尺寸时允许的最大值（逻辑点）。
///
/// 配置是可以手改的：写个 100000 进去，窗口就会大到系统再也摆不下。
/// 上限取 10000——比任何单屏都大，又不会让窗口系统算出离谱的整数。
const WINDOW_MAX_SIZE: f32 = 10_000.0;

/// 窗口尺寸稳定多久之后才写进配置。
///
/// 拖边缘时 `screen_rect` 每帧都在变，逐帧落盘等于每帧重写一次 config.toml；
/// 攒够这段安静时间再写一次，代价只是「拖完手松开后 0.6 秒才存」。
/// 期间要自己申请重绘——没有输入事件时事件循环会睡过去，睡过去就永远等不到这一帧。
const WINDOW_SIZE_SAVE_DELAY: Duration = Duration::from_millis(600);

/// 等待落盘期间的重绘间隔。
const WINDOW_SIZE_REPAINT: Duration = Duration::from_millis(120);

/// 侧边栏默认宽度。
///
/// 300 而不是 250：15px 字号下 `Administrator@192.168.10.20:3389` 这类
/// 「用户@地址:端口」在 250 里放不下，会被截成半截。
const SIDEBAR_WIDTH: f32 = 300.0;

/// 烟雾测试下的窗口位置。固定住才能做可复现的自动化截图。
const SMOKE_WINDOW_POS: [f32; 2] = [80.0, 80.0];

/// 烟雾测试下的窗口尺寸。
const SMOKE_WINDOW_SIZE: [f32; 2] = [1200.0, 800.0];

/// 日志默认级别。配置文件读出来之前用它，之后由设置页的配置接管。
const DEFAULT_LOG_LEVEL: &str = "info";

/// 「跟随窗口」分辨率下的初始桌面尺寸。
///
/// 真正的跟随窗口要等会话建立后读本地窗口大小再动态下发（步骤 64），
/// 这里先给一个常见尺寸，避免用 0 或极小值去建连。
const FIT_WINDOW_START_WIDTH: u32 = 1280;
const FIT_WINDOW_START_HEIGHT: u32 = 800;

/// 侧边栏当前显示的页。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SidebarTab {
	/// 主机列表。
	#[default]
	Hosts,
	/// 代理列表。
	Proxies,
}

/// 一次连接的计划：直连（可带代理）、经跳板机，或 RDP 桌面。
///
/// 拆成枚举而不是在 `SshParams` 上叠条件——跳板机路径要的是两套参数
/// （跳板机一套、目标一套），塞不进单个 `SshParams`；RDP 更是另一套东西。
enum ConnectionPlan {
	/// 直连目标（dialer 可能已换成代理）。
	Direct(SshParams),
	/// 经跳板机连目标。
	Jump {
		/// 跳板机自己的连接参数。
		jump: SshParams,
		/// 真正要连的目标。
		target: SshTarget,
		/// 目标的认证方式。
		auth: AuthSpec,
		/// 目标的主机密钥策略。必须显式携带，否则跳板机路径的
		/// known_hosts 核对会静默失效。
		host_key: HostKeyPolicy,
	},
	/// RDP 远程桌面。
	Rdp(ConnectionParams),
}

/// 分组名输入表单（新建与重命名共用）。
///
/// 只有一个输入框，不值得独立视图文件；`editing` 区分两种模式：
/// `Some(id)` 是重命名，`None` 是新建（此时 `parent` 决定挂在哪一级）。
struct GroupFormState {
	/// `Some(id)` 表示重命名该分组。
	editing: Option<i64>,
	/// 新建时的上级分组；`None` 建顶层分组。
	parent: Option<i64>,
	/// 输入中的名称。
	name: String,
}

/// 把 FreeRDP 自己的日志导到文件。
///
/// **必须在任何 FreeRDP 调用之前调用**（见 `main` 里的说明）：
/// 根 logger 只初始化一次，由 `freerdp_new()` 内部的首次 `WLog_Get` 触发，
/// 那时机已经晚于连接前能设的环境变量。
///
/// **默认就开，级别 WARN**。连接失败的真实原因全在这一层：
/// CredSSP 的 `SEC_E_ALGORITHM_MISMATCH`、TLS 版本不匹配、证书问题，
/// FreeRDP 只写 WLog，不写进它返回给我们的错误码。而 GUI 双击启动时
/// stderr 谁也看不见——不落盘就等于没有。WARN 级别的量很小（一次连接失败
/// 几行），和 DEBUG 那种每秒几百条的协议报文不是一回事。
///
/// 要看逐条协议报文时用 `DAISY_FREERDP_LOG=1`：那会由 `daisy-rdp` 把
/// `cliprdr` 一个 tag 单独特抬到 DEBUG（见 `enable_freerdp_protocol_log`），
/// 而不是把全局抬到 DEBUG——否则画面/键盘日志会把真正要看的内容淹掉。
///
/// 落在应用日志目录里，设置页的「打开日志目录」就能直接看到。
fn configure_freerdp_logging(log_dir: &std::path::Path) -> std::path::PathBuf {
	let path = log_dir.join("freerdp.log");
	// SAFETY: 只在 main 开头单线程调用一次，之后不再改这些变量。
	// FreeRDP 读它们的方式是 getenv，不会保留指针。
	unsafe {
		if std::env::var("DAISY_FREERDP_LOG").ok().as_deref() == Some("1") {
			// 调试模式：CONSOLE appender + 全局 DEBUG。文件 appender 在本机
			// 实测不落盘（文件建了但 0 字节）；按 tag 单独 SetLogLevel 也
			// 实测无效（cliprdr 一条 DEBUG 都不出，原因未明）——全局 DEBUG
			// 是唯一可靠的路，诊断用完即关
			std::env::set_var("WLOG_APPENDER", "CONSOLE");
			std::env::set_var("WLOG_LEVEL", "DEBUG");
		} else {
			std::env::set_var("WLOG_APPENDER", "FILE");
			std::env::set_var("WLOG_FILEAPPENDER_OUTPUT_FILE_PATH", &path);
			std::env::set_var("WLOG_LEVEL", "WARN");
		}
	}
	// 目录与空文件都先建好：路径写错或无权限时能立刻看出来，
	// 而不用等到连接失败、想查原因时才发现日志压根没落盘。
	if let Err(error) = std::fs::create_dir_all(log_dir).and_then(|()| std::fs::write(&path, b"")) {
		// 这时 tracing 还没初始化，只能走标准错误
		eprintln!("FreeRDP 日志无法创建（{}）：{error}", path.display());
	}
	path
}

fn main() -> eframe::Result {
	// **必须在任何 FreeRDP 调用之前**把 FreeRDP 自己的协议日志导到文件。
	//
	// FreeRDP 的 WLog 根 logger 由 `WLog_InitializeRoot` 用
	// `InitOnceExecuteOnce` 初始化，而它第一次被触发是在 `freerdp_new()`
	// 内部——那已经晚于「连接前设置」的任何时机。设晚了根 logger 就永远
	// 是 CONSOLE，而 GUI 应用双击启动时 stderr 谁也看不见，
	// 于是协议日志全部丢失（实测：日志文件压根没生成）。
	//
	// S8-63 排查剪贴板必须靠它：只有这一层能看到「报文到底有没有到」，
	// 我们自己的回调日志只能证明「回调有没有被调用」。
	// 数据目录先解析：FreeRDP 的日志要落到应用日志目录里
	let paths = resolve_paths();
	let freerdp_log = configure_freerdp_logging(&paths.log_dir());
	let logging = match Logging::init(&paths, DEFAULT_LOG_LEVEL) {
		Ok(logging) => Some(logging),
		Err(error) => {
			// 日志都起不来时不能再用 tracing 报错——直接写标准错误
			eprintln!("日志初始化失败（{error}），日志将只输出到终端");
			None
		},
	};

	let smoke_after = smoke_duration();
	if let Some(after) = smoke_after {
		tracing::info!("烟雾测试模式：{after:?} 后自动关闭窗口");
	}
	// 把「哪个二进制在跑」写进日志。同一个版本号下可能同时存在 target/debug、
	// dist/Daisy.app、以及被拷到别处的副本；出问题时这一行能直接排除
	// 「其实跑的是旧包」这类误会——这个坑真实发生过
	tracing::info!(
		"{} {} 启动（{}）",
		daisy_core::APP_NAME,
		daisy_core::APP_VERSION,
		executable_path()
	);
	tracing::info!("日志目录：{}", paths.log_dir().display());
	tracing::info!("FreeRDP 日志：{}", freerdp_log.display());

	// 窗口按**上次退出时的尺寸**打开。配置读不到或坏了就用默认尺寸——
	// 一个手改坏的 config.toml 不该让窗口根本开不出来。
	// （`bootstrap` 里还会再读一次：这里只为了赶在建窗口之前拿到尺寸。）
	let startup_config = match AppConfig::load(&paths.config_file()) {
		Ok(config) => config,
		Err(error) => {
			tracing::warn!("读取配置失败（{error}），窗口用默认尺寸");
			AppConfig::default()
		},
	};

	let viewport = if smoke_after.is_some() {
		// 烟雾测试要求可复现：固定位置与尺寸，便于自动化截图比对
		egui::ViewportBuilder::default()
			.with_inner_size(SMOKE_WINDOW_SIZE)
			.with_position(SMOKE_WINDOW_POS)
			.with_title(window_title())
	} else {
		egui::ViewportBuilder::default()
			.with_inner_size(initial_window_size(&startup_config))
			.with_min_inner_size(WINDOW_MIN_SIZE)
			.with_title(window_title())
	}
	.with_icon(window_icon());

	let options = eframe::NativeOptions {
		viewport,
		..Default::default()
	};

	let result = eframe::run_native(
		daisy_core::APP_NAME,
		options,
		Box::new(move |cc| {
			configure_style(&cc.egui_ctx);
			// 中文字体必须在第一帧之前装好，否则界面会出现方块。
			// 终端字体先按默认装；配置里若指定了别的，`apply_config` 会再换一次。
			fonts::install_fonts(&cc.egui_ctx, None);
			Ok(Box::new(DaisyApp::new(smoke_after, paths, logging)))
		}),
	);
	// 关闭排障打点：这条能出现，说明 winit/eframe 已经退出，之后只剩
	// `DaisyApp` 的字段 drop；见不到它则卡在事件循环或 wgpu 收尾。
	match &result {
		Ok(()) => tracing::info!("[关闭] run_native 已正常返回，开始 drop DaisyApp"),
		Err(error) => tracing::warn!("[关闭] run_native 返回错误：{error}"),
	}
	result
}

/// 当前可执行文件的路径。拿不到时给个占位，不让日志因为取路径失败而缺行。
fn executable_path() -> String {
	std::env::current_exe()
		.map(|path| path.display().to_string())
		.unwrap_or_else(|_| "路径未知".to_string())
}

fn window_title() -> String {
	format!("{} {}", daisy_core::APP_NAME, daisy_core::APP_VERSION)
}

/// 启动时请求的窗口尺寸。
///
/// 配置里存的是**逻辑点**（egui 的坐标系），而 `ViewportBuilder` 收的是
/// 系统的「原生点」——两者差的正是界面缩放：`pixels_per_point = 基准 × ui_scale`，
/// 同一块像素能容下的点就少了 `ui_scale` 倍。不乘回去，开了 1.25 倍缩放的用户
/// 每启动一次窗口就小一圈（实测：1200 点 → 960 点 → 768 点）。
fn initial_window_size(config: &AppConfig) -> [f32; 2] {
	let window = config.window;
	if !window.is_usable() {
		return WINDOW_SIZE;
	}
	let scale = if config.ui_scale.is_finite() && config.ui_scale > 0.0 {
		config.ui_scale
	} else {
		1.0
	};
	[
		(window.width * scale).clamp(WINDOW_MIN_SIZE[0], WINDOW_MAX_SIZE),
		(window.height * scale).clamp(WINDOW_MIN_SIZE[1], WINDOW_MAX_SIZE),
	]
}

/// 面板边框：底色 + 内边距。
///
/// 显式指定底色而不是继承 `panel_fill`——标签栏、侧边栏、状态栏
/// 需要三种不同的明度层次，否则几块区域和内容区糊成一片。
fn panel_frame(fill: egui::Color32, x: i8, y: i8) -> egui::Frame {
	egui::Frame::NONE
		.fill(fill)
		.inner_margin(egui::Margin::symmetric(x, y))
}

/// 窗口图标边长。
const WINDOW_ICON_SIZE: u32 = 256;

/// 窗口图标：原始 RGBA 像素，由 `scripts/make-icon.py` 生成。
///
/// 存原始像素而不是 PNG 是为了不引入解码依赖——32 KB 的素材换掉一个 crate 值得。
/// macOS 的 dock 图标由 `.app` bundle 的 `Info.plist` + `.icns` 决定（见 S10 打包），
/// 这里的窗口图标主要给 Windows / Linux，同时为 S11 备好资源。
const WINDOW_ICON_RGBA: &[u8] = include_bytes!("../../../assets/icons/daisy-icon.rgba");

fn window_icon() -> egui::IconData {
	egui::IconData {
		rgba: WINDOW_ICON_RGBA.to_vec(),
		width: WINDOW_ICON_SIZE,
		height: WINDOW_ICON_SIZE,
	}
}

/// 无人值守验证用的窗口尺寸环境变量（形如 `1000x700`）。
///
/// 「记住窗口大小」从外面没法验：改窗口要拖边缘或走系统菜单，而合成鼠标事件
/// 被辅助功能权限拦着（见 `docs/UI-POLISH.md`）。这个钩子把「改尺寸」这一步
/// 从应用内部做掉，剩下的「观察 → 稳定 → 落盘」走的是真实链路——
/// 于是「改了尺寸，下次开还是这个尺寸」可以被脚本完整跑一遍。
const SMOKE_RESIZE_ENV: &str = "DAISY_SMOKE_RESIZE";

/// 钩子生效前的等待：等首帧画完、配置加载完再改，免得和初始化抢着改窗口。
const SMOKE_RESIZE_DELAY: Duration = Duration::from_millis(1200);

/// 读取烟雾尺寸钩子；未设置或格式不对时返回 `None`。
fn smoke_resize_size() -> Option<[f32; 2]> {
	let raw = std::env::var(SMOKE_RESIZE_ENV).ok()?;
	let (width, height) = raw.trim().split_once(['x', 'X', ','])?;
	let width: f32 = width.trim().parse().ok()?;
	let height: f32 = height.trim().parse().ok()?;
	(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0)
		.then_some([width, height])
}

/// 无人值守验证：按时间表切换终端字体。
///
/// 「换一次字体再换回来，排版就乱了」这类问题只能整条链路跑一遍才看得见——
/// 换字体要动设置窗口的下拉，而合成点击被辅助功能权限拦着（见
/// `docs/UI-POLISH.md`）。这个钩子把「改字体」这一步从应用内部做掉：
///
/// ```text
/// DAISY_SMOKE_FONTS="1500:Courier New;2500:"
///                    ↑毫秒:字体名        ↑空名字 = 回到内置默认
/// ```
///
/// 它走的是与设置窗口完全相同的路径：改 `config.terminal.font_family`，
/// 剩下的交给 `sync_config` → `apply_config`。
const SMOKE_FONTS_ENV: &str = "DAISY_SMOKE_FONTS";

/// 解析字体切换钩子，返回 `(相对启动的毫秒数, 字体名)` 列表。
fn smoke_font_switches() -> Vec<(Instant, Option<String>)> {
	let Ok(raw) = std::env::var(SMOKE_FONTS_ENV) else {
		return Vec::new();
	};
	let now = Instant::now();
	let mut steps = Vec::new();
	for item in raw.split(';') {
		let Some((at, family)) = item.split_once(':') else {
			continue;
		};
		let Ok(at) = at.trim().parse::<u64>() else {
			continue;
		};
		let family = family.trim();
		steps.push((
			now + Duration::from_millis(at),
			(!family.is_empty()).then(|| family.to_owned()),
		));
	}
	steps.sort_by_key(|(at, _)| *at);
	steps
}

/// 读取烟雾测试时长；未设置或解析失败时返回 `None`。
fn smoke_duration() -> Option<Duration> {
	std::env::var(SMOKE_ENV)
		.ok()
		.and_then(|raw| raw.trim().parse::<u64>().ok())
		.map(Duration::from_millis)
}

/// 无人值守验证：给「新建 / 编辑主机」表单预填一次内容。
///
/// 密码点的密度、测试连接的绿字/红字，都只有**填了东西**才看得见；而脚本化
/// 敲键被系统权限拦着（见 `docs/UI-POLISH.md`），只能让应用自己填：
///
/// - `DAISY_SMOKE_FORM_PASSWORD=...` —— 预填密码
/// - `DAISY_SMOKE_PROBE=1` —— 自动点一次「测试连接」
fn apply_host_form_smoke_hooks(state: &mut HostFormState) {
	if let Ok(password) = std::env::var("DAISY_SMOKE_FORM_PASSWORD") {
		state.set_password(&password);
	}
	if std::env::var("DAISY_SMOKE_PROBE").is_ok() {
		state.request_probe();
	}
}

/// 解析数据目录。
///
/// `DAISY_DATA_DIR` 是开发与无人值守验证的覆盖口——库、配置、日志
/// 三者共用同一个 `AppPaths`，避免出现「库是临时的、日志是本机的」分裂。
///
/// 此时日志还没初始化，出错只能写标准错误。
fn resolve_paths() -> AppPaths {
	match std::env::var_os("DAISY_DATA_DIR") {
		Some(base) if !base.is_empty() => AppPaths::from_base(base),
		_ => AppPaths::discover().unwrap_or_else(|error| {
			eprintln!("解析数据目录失败（{error}），回退当前目录 .daisy");
			AppPaths::from_base(".daisy")
		}),
	}
}

/// 配置全局样式：度量（间距、尺寸）与字号。
///
/// **配色不在这里**——深浅两套 `Visuals` 由 [`DaisyApp::apply_config`] 按配置
/// 设置。两者写在同一个 `Style` 上但字段不重叠，互不覆盖。
///
/// egui 的默认值是按「信息密度优先」定的（`item_spacing` 垂直只有 3px、
/// `button_padding` 只有 1px、正文 13px），西文小字下勉强能用，
/// 中文在这个密度下会糊成一团。这里统一放宽。
fn configure_style(ctx: &egui::Context) {
	// `all_styles_mut` 改的是**深浅两套**样式——间距与字号在两个主题下
	// 必须一致，否则切主题时布局会跳
	ctx.all_styles_mut(|style| {
		let spacing = &mut style.spacing;
		// 垂直 3px → 8px：列表、分组树"挤在一起"的主因就在这一项
		spacing.item_spacing = egui::vec2(10.0, 8.0);
		// 默认 (4, 1) 让按钮文字几乎贴住边框
		spacing.button_padding = egui::vec2(12.0, 6.0);
		// 默认高度 18px，中文按钮显得矮扁
		spacing.interact_size = egui::vec2(28.0, 26.0);
		// 树形层级缩进，比默认 18 略宽，父子关系更好认
		spacing.indent = 20.0;
		spacing.window_margin = egui::Margin::same(12);
		spacing.menu_margin = egui::Margin::same(10);
		// 常驻滚动条：多主机列表里看不到滚动条会让人以为没有更多内容
		spacing.scroll = egui::style::ScrollStyle::solid();

		// 字号整体上调一档：中文在小字号下的辨识度远低于西文
		style.text_styles = [
			(egui::TextStyle::Small, egui::FontId::proportional(11.0)),
			(egui::TextStyle::Body, egui::FontId::proportional(15.0)),
			(egui::TextStyle::Button, egui::FontId::proportional(15.0)),
			(egui::TextStyle::Heading, egui::FontId::proportional(22.0)),
			(egui::TextStyle::Monospace, egui::FontId::monospace(14.0)),
		]
		.into();
	});
}

/// 主应用状态。
struct DaisyApp {
	/// 到点后自动关闭窗口；`None` 表示正常运行。
	smoke_deadline: Option<Instant>,
	/// 加密库。第一帧才开——开库可能建文件，不该阻塞窗口出现。
	store: Option<Store>,
	/// 开库失败的文案。界面上直接显示，其余功能降级。
	store_error: Option<String>,
	/// 数据目录。配置、数据库与日志都在它下面。
	paths: AppPaths,
	/// 当前配置（设置窗口直接改它）。
	config: AppConfig,
	/// 已落盘并生效的配置。每帧与 `config` 比对，不同就应用+保存。
	applied_config: AppConfig,
	/// 当前已经装进 egui 的终端字体（`None` = 内置默认）。
	///
	/// 外层 `Option` 表示「还没记录过」：启动时 `run_native` 已经按默认装过一遍，
	/// 所以初值是 `Some(None)`，免得首帧白重装一次 22MB 的中文字体。
	terminal_font: Option<Option<String>>,
	/// 系统像素密度基准。界面缩放是相对它的倍率。
	scale_base: ScaleBase,
	/// 主机缓存。每次增删改后从库里重读，保证与落库内容一致。
	hosts: Vec<Host>,
	/// 分组缓存。同上。
	groups: Vec<Group>,
	/// 代理缓存。
	proxies: Vec<Proxy>,
	/// 会话（标签页）。
	sessions: SessionManager,
	/// 侧边栏状态（主机页）。
	host_list: HostListState,
	/// 侧边栏状态（代理页）。
	proxy_list: ProxyListState,
	/// 侧边栏当前页。
	sidebar_tab: SidebarTab,
	/// 打开中的主机表单。
	form: Option<HostFormState>,
	/// 待确认删除的主机。
	pending_delete: Option<Host>,
	/// 打开中的分组名表单（新建 / 重命名共用）。
	group_form: Option<GroupFormState>,
	/// 待确认删除的分组。
	pending_delete_group: Option<Group>,
	/// 打开中的代理表单。
	proxy_form: Option<ProxyFormState>,
	/// 待确认删除的代理。
	pending_delete_proxy: Option<Proxy>,
	/// 设置窗口是否打开。
	settings_open: bool,
	/// SFTP 文件管理对话框（同一时刻只开一个）。
	sftp: Option<views::sftp_view::SftpState>,
	/// 进行中的连通性探测（结果接收端）。
	probe_rx: Option<Receiver<ProbeOutcome>>,
	/// 表单预填钩子是否已经执行过。
	///
	/// 只跑一次：预填内容（密码、自动点测试连接）每帧重放会不停触发探测。
	smoke_form_hooks_done: bool,
	/// 全局提示（右下角浮层，自动过期淡出）。
	toasts: ToastState,
	/// 是否已经开过初始标签页。
	bootstrapped: bool,
	/// 窗口落位是否已检查。只做一次：重复检查会把用户手动
	/// 摆好的位置也强行居中（副屏可见时那是合法摆放）。
	window_checked: bool,
	/// `DAISY_LOG` 是否在启动时指定了级别。指定了就以环境变量为准，
	/// 不被配置里的 `log_level` 覆盖——否则调试时改配置才能看日志，
	/// 而看日志本身又需要先知道问题出在哪。
	log_env_override: bool,
	/// 待注入终端的那串文本（见 [`SMOKE_TERM_INPUT_ENV`]）。
	smoke_term_input: Option<String>,
	/// 是否已经注入过。只敲一次。
	smoke_term_sent: bool,
	/// 到哪一刻才注入。
	smoke_term_at: Option<Instant>,
	/// 观察到但还没落盘的窗口尺寸，以及它从哪一刻开始稳定。
	window_size_pending: Option<(egui::Vec2, Instant)>,
	/// 尺寸钩子还没跑的话，到哪一刻跑（见 [`SMOKE_RESIZE_ENV`]）。
	smoke_resize_at: Option<Instant>,
	/// 字体切换钩子剩下的步骤（见 [`SMOKE_FONTS_ENV`]），按时间升序。
	smoke_fonts: Vec<(Instant, Option<String>)>,
	/// 有一批单元格尺寸要等新字体生效之后再量（见 [`Self::apply_config`]）。
	appearance_pending: bool,
	/// 日志句柄。持有写盘守卫与级别热替换句柄。
	///
	/// **必须声明在结构体最后**：Rust 的字段 drop 顺序＝声明顺序，而关闭
	/// 过程（各标签页、RDP/PTY 线程收尾）恰恰是最需要日志的时刻。放在
	/// `sessions` 之前会让写盘守卫先 drop，那段日志全部丢掉——排查「关窗口
	/// 卡死」时就完全看不到卡在哪一步。初始化失败时为 `None`：日志不可用
	/// 不该挡住主功能。
	logging: Option<Logging>,
}

impl DaisyApp {
	fn new(smoke_after: Option<Duration>, paths: AppPaths, logging: Option<Logging>) -> Self {
		Self {
			smoke_deadline: smoke_after.map(|after| Instant::now() + after),
			store: None,
			store_error: None,
			paths,
			logging,
			config: AppConfig::default(),
			applied_config: AppConfig::default(),
			// 启动时 `run_native` 已按内置默认装过一次字体
			terminal_font: Some(None),
			// 真值在 bootstrap 里（要等 ctx）填；此处先给 1.0 占位
			scale_base: ScaleBase::new(1.0),
			hosts: Vec::new(),
			groups: Vec::new(),
			proxies: Vec::new(),
			sessions: SessionManager::new(),
			host_list: HostListState::default(),
			proxy_list: ProxyListState::default(),
			sidebar_tab: SidebarTab::Hosts,
			form: None,
			pending_delete: None,
			group_form: None,
			pending_delete_group: None,
			proxy_form: None,
			pending_delete_proxy: None,
			settings_open: false,
			sftp: None,
			probe_rx: None,
			smoke_form_hooks_done: false,
			toasts: ToastState::default(),
			bootstrapped: false,
			window_checked: false,
			log_env_override: Logging::env_override(),
			smoke_term_input: None,
			smoke_term_sent: false,
			smoke_term_at: None,
			window_size_pending: None,
			smoke_resize_at: smoke_resize_size().map(|_| Instant::now() + SMOKE_RESIZE_DELAY),
			smoke_fonts: smoke_font_switches(),
			appearance_pending: false,
		}
	}

	/// 第一帧：开库、加载配置、开本地终端。
	fn bootstrap(&mut self, ctx: &egui::Context) {
		if self.bootstrapped {
			return;
		}
		self.bootstrapped = true;

		match Store::open_at(&self.paths) {
			Ok(store) => {
				self.store = Some(store);
				self.refresh_hosts();
				self.refresh_groups();
				self.refresh_proxies();
			},
			Err(error) => {
				tracing::error!("打开加密库失败：{error}");
				self.store_error = Some(error.to_string());
			},
		}

		// 配置：读不到或解析失败都用默认值，不让坏配置挡住启动
		match AppConfig::load(&self.paths.config_file()) {
			Ok(config) => self.config = config,
			Err(error) => {
				tracing::warn!("读取配置失败（{error}），使用默认配置");
				self.config = AppConfig::default();
			},
		}
		// 必须在任何 set_pixels_per_point 之前取
		self.scale_base = ScaleBase::capture(ctx);
		self.apply_config(ctx);

		// 启动即有一个本地终端：既有内容可看，也顺带验证 PTY 链路
		self.sessions.open_local(ctx);

		// 无人值守验证钩子：给活动终端敲一串文本（见 [`SMOKE_TERM_INPUT_ENV`]）
		if let Ok(text) = std::env::var(SMOKE_TERM_INPUT_ENV)
			&& !text.is_empty()
		{
			tracing::info!("终端注入钩子：{text:?}");
			self.smoke_term_input = Some(text);
			self.smoke_term_at = Some(Instant::now() + SMOKE_TERM_INPUT_DELAY);
		}

		// 无人值守验证钩子：自动连接指定名称的主机。
		// 走的是与用户双击列表**完全相同**的 connect_host 路径，
		// 因此截图验证到的就是真实交互链路。
		if let Some(name) = std::env::var("DAISY_SMOKE_AUTOCONNECT")
			.ok()
			.filter(|name| !name.trim().is_empty())
		{
			match self.hosts.iter().find(|host| host.name == name).cloned() {
				Some(host) => {
					tracing::info!("自动连接钩子：连接「{name}」");
					self.connect_host(&host);
				},
				None => {
					// 把库里的名字按转义形式打出来：肉眼看不见的差异（尾随空格、
					// 全角空格、零宽字符）正是"明明有这台机器却找不到"的常见原因
					tracing::warn!(
						"自动连接钩子：找不到名为「{name}」的主机；库里有 {:?}",
						self.host_names()
					);
				},
			}
		}

		// 无人值守验证钩子：预先选中指定主机。
		//
		// 选中态（左侧强调条 + 底色 + 底部操作条）是侧边栏最要紧的视觉反馈，
		// 而点击无法脚本化——没有这个钩子，这部分改动只能靠碰巧选中才看得到。
		if let Some(name) = std::env::var("DAISY_SMOKE_SELECT")
			.ok()
			.filter(|name| !name.trim().is_empty())
		{
			match self.hosts.iter().find(|host| host.name == name) {
				Some(host) => self.host_list.select(host.id),
				None => tracing::warn!("选中钩子：找不到名为「{name}」的主机"),
			}
		}

		// 无人值守验证钩子：启动即打开某个窗口。
		//
		// 表单与设置页平时要点按钮才能看到，而 AppleScript 点击被系统权限拦，
		// 做不到自动化截图——这个开关让布局改动能被截图核对。
		if let Some(what) = std::env::var("DAISY_SMOKE_OPEN")
			.ok()
			.filter(|what| !what.trim().is_empty())
		{
			let what = what.trim();
			if let Some(name) = what.strip_prefix("edit-host:") {
				match self.hosts.iter().find(|host| host.name == name).cloned() {
					Some(host) => self.open_edit_form(&host),
					None => tracing::warn!("打开窗口钩子：找不到名为「{name}」的主机"),
				}
			} else if let Some(name) = what.strip_prefix("files:") {
				// 文件管理同样只能点出来：走与「文件」按钮同一条路径
				match self.hosts.iter().find(|host| host.name == name).cloned() {
					Some(host) => self.open_sftp(&host),
					None => tracing::warn!("打开窗口钩子：找不到名为「{name}」的主机"),
				}
			} else if let Some(name) = what.strip_prefix("delete-host:")
				&& let Some(id) = self
					.hosts
					.iter()
					.find(|host| host.name == name)
					.and_then(|host| host.id)
			{
				// 走与右键菜单**同一条**动作，截到的就是真实链路
				self.on_host_list_action(HostListAction::Delete(id));
			} else {
				match what {
					"settings" => self.settings_open = true,
					"new-host" => self.form = Some(HostFormState::new_host()),
					"new-proxy" => self.proxy_form = Some(ProxyFormState::new_proxy()),
					// 确认框与分组名输入框平时要点右键菜单才出得来，
					// 靠这两个钩子才能截图核对里面的排版
					"new-group" => {
						self.on_host_list_action(HostListAction::NewGroup { parent: None })
					},
					"delete-group" => {
						if let Some(id) = self.groups.first().and_then(|group| group.id) {
							self.on_host_list_action(HostListAction::RequestDeleteGroup { id });
						}
					},
					_ => tracing::warn!("无法识别的 DAISY_SMOKE_OPEN：{what}"),
				}
			}
		}
	}

	/// 打开文件管理（SFTP）。
	///
	/// 用与终端**同一份**连接参数（含代理与主机密钥策略）：
	/// 文件功能不该绕开主机密钥校验。跳板机路径暂不支持——那需要
	/// 先建跳板机通道再在其上开 sftp，属于另一条链路。
	fn open_sftp(&mut self, host: &Host) {
		if host.protocol != Protocol::Ssh {
			self.toast_error("远程桌面没有文件通道");
			return;
		}
		let Some(store) = self.store.as_ref() else {
			self.toast_error("加密库未打开，无法读取凭据");
			return;
		};
		match build_connection_plan(store, host, &self.hosts) {
			Ok(ConnectionPlan::Direct(params)) => {
				self.sftp = Some(views::sftp_view::SftpState::open(host.name.clone(), params));
			},
			Ok(ConnectionPlan::Jump { .. }) => {
				self.toast_error("经跳板机的文件管理暂未支持，请先直连这台主机");
			},
			Ok(ConnectionPlan::Rdp(_)) => {
				self.toast_error("远程桌面没有文件通道");
			},
			Err(reason) => self.toast_error(reason),
		}
	}

	/// 打开「编辑主机」表单，并把已记录的证书指纹一起带进去（只有 RDP 有）。
	///
	/// 指纹由应用层读：视图不碰数据库；这里顺便把「记录属于哪个地址」也告诉
	/// 表单，改了地址之后旧指纹就不会继续显示（见 `host_form::cert_applies`）。
	fn open_edit_form(&mut self, host: &Host) {
		let mut form = HostFormState::edit(host);
		if host.protocol == daisy_core::model::Protocol::Rdp {
			let shown = self
				.rdp_cert_of(&host.host, host.port)
				.map(|fingerprint| daisy_rdp::format_fingerprint(&fingerprint));
			form.set_known_cert(shown, &host.host, host.port);
		}
		self.form = Some(form);
	}

	/// 处理表单里的「重新信任」：删掉这台机器的证书记录。
	///
	/// 删掉之后下次连接按「首次见到」处理——接受并重新记录，同时再提示一次。
	/// 这条路径是给「机器重装过、指纹确实变了」用的；没有它，用户就只能去改数据库。
	fn forget_rdp_cert(&mut self, host: &str, port: u16) -> Result<(), String> {
		let store = self
			.store
			.as_ref()
			.ok_or_else(|| "加密库没有打开，无法修改证书记录".to_string())?;
		match store.forget_rdp_cert(host, port) {
			Ok(true) => {
				tracing::info!("已删除 {host}:{port} 的 RDP 证书记录，下次连接按首连处理");
				self.toast(format!("已重新信任 {host}，下次连接会记录新的证书指纹"));
				Ok(())
			},
			// 记录本来就不存在：目标已达成，不当失败
			Ok(false) => Ok(()),
			Err(error) => Err(format!("删除证书记录失败：{error}")),
		}
	}

	/// 从库里重读主机列表。
	fn refresh_hosts(&mut self) {
		let Some(store) = self.store.as_ref() else {
			return;
		};
		match store.hosts() {
			Ok(hosts) => self.hosts = hosts,
			Err(error) => {
				tracing::warn!("读取主机列表失败：{error}");
				self.toast_error(format!("读取主机列表失败：{error}"));
			},
		}
	}

	/// 从库里重读分组列表。
	fn refresh_groups(&mut self) {
		let Some(store) = self.store.as_ref() else {
			return;
		};
		match store.groups() {
			Ok(groups) => self.groups = groups,
			Err(error) => {
				tracing::warn!("读取分组列表失败：{error}");
				self.toast_error(format!("读取分组列表失败：{error}"));
			},
		}
	}

	/// 从库里重读代理列表。
	fn refresh_proxies(&mut self) {
		let Some(store) = self.store.as_ref() else {
			return;
		};
		match store.proxies() {
			Ok(proxies) => self.proxies = proxies,
			Err(error) => {
				tracing::warn!("读取代理列表失败：{error}");
				self.toast_error(format!("读取代理列表失败：{error}"));
			},
		}
	}

	/// 把配置落到 egui、终端渲染器与日志上。
	///
	/// `color_scheme` 与主题联动——两套 Palette 就是主题本身，
	/// 单独留一个不同步的配色名只会造成「选了却没变」。
	fn apply_config(&mut self, ctx: &egui::Context) {
		let mode = ColorMode::resolve(self.config.theme, ctx);

		self.config.terminal.color_scheme = match mode {
			ColorMode::Light => "daisy-light",
			ColorMode::Dark => "daisy-dark",
		}
		.to_string();

		// 设计系统接管配色与排版：色板、圆角、间距、字号全在 design.rs
		design::apply(ctx, mode.into());
		ctx.set_pixels_per_point(self.scale_base.scaled(self.config.ui_scale));

		// 终端字体换了才重装字体库：`set_fonts` 会让所有文本重新排版并重建图集，
		// 改个日志级别不该付这个代价。
		//
		// **换字体这趟不能量单元格尺寸**：`ctx.set_fonts` 只是把新的
		// `FontDefinitions` 排进 `new_font_definitions`，下一趟 `begin_pass`
		// 才真正换上。此刻量 `'M'` 量到的还是旧字体——宽出来的那一档会一直
		// 用到下次重量：整屏按错的列宽排，列越靠右偏得越多，宽字符之间被
		// 拉开成两三格。字号没变、只是换了字体时最容易中招（两者宽度相差
		// 一个数量级的字体才有肉眼可见的偏移）。
		//
		// 所以这里只置标志，下一帧开头再量（那时新字体已经生效）。
		let family = self.config.terminal.font_family.clone();
		let mut remeasure = false;
		if self.terminal_font.as_ref() != Some(&family) {
			let installed = fonts::install_fonts(ctx, family.as_deref());
			if let Some(warning) = installed.warning {
				self.toast_error(warning);
			}
			self.terminal_font = Some(family);
			remeasure = true;
		}

		// 配色与字号无关，随改随生效
		self.sessions.set_palette(mode.terminal_palette());
		if remeasure {
			self.appearance_pending = true;
			ctx.request_repaint();
		} else {
			self.sessions
				.set_font_size(ctx, self.config.terminal.font_size);
		}
		// 环境变量指定过级别时不动它——调试入口优先
		if !self.log_env_override
			&& let Some(logging) = &self.logging
		{
			logging.set_level(&self.config.log_level);
		}

		self.applied_config = self.config.clone();
	}

	/// 每帧调用：配置变了就应用并落盘。
	fn sync_config(&mut self, ctx: &egui::Context) {
		if self.config == self.applied_config {
			return;
		}
		self.apply_config(ctx);
		if let Err(error) = self.config.save(&self.paths.config_file()) {
			tracing::warn!("保存配置失败：{error}");
			self.toast_error(format!("设置已生效，但保存失败：{error}"));
		}
	}

	/// 记住窗口尺寸：等它稳定一小段时间再写进配置。
	///
	/// 三条不能省的判断：
	/// ① **冒烟测试不记**——那套流程把窗口钉成 1200×800 好做截图比对，
	///    记下来就把固定尺寸带进了用户的正常启动；
	/// ② **最大化不记**——记下来的是一个「恢复后不会最大化」的超大窗口，
	///    下次开出来比屏幕还大；
	/// ③ **小于最小尺寸不记**——最小化时屏幕矩形会退化成 0。
	fn remember_window_size(&mut self, ctx: &egui::Context) {
		if self.smoke_deadline.is_some() {
			return;
		}
		let maximized = ctx
			.input(|input| input.viewport().maximized)
			.unwrap_or(false);
		if maximized {
			self.window_size_pending = None;
			return;
		}

		// `viewport_rect` 就是窗口内容区的完整尺寸，和建窗口时
		// `with_inner_size` 是同一口径（`content_rect` 会被灵动岛/安全区再切一刀，
		// 存它会导致每开一次窗口就少一点）
		let size = ctx.viewport_rect().size();
		if size.x < WINDOW_MIN_SIZE[0] || size.y < WINDOW_MIN_SIZE[1] {
			return;
		}
		let size = egui::vec2(size.x.round(), size.y.round());

		// 已经和配置一致：没有要存的东西
		if (size.x - self.config.window.width).abs() < 1.0
			&& (size.y - self.config.window.height).abs() < 1.0
		{
			self.window_size_pending = None;
			return;
		}

		match self.window_size_pending {
			// 尺寸没再变，且已经安静够久 → 落盘（`sync_config` 会看到 config 变了）
			Some((pending, since)) if pending == size => {
				if since.elapsed() >= WINDOW_SIZE_SAVE_DELAY {
					self.config.window.width = size.x;
					self.config.window.height = size.y;
					self.window_size_pending = None;
					tracing::debug!("已记住窗口尺寸：{}×{}", size.x, size.y);
				} else {
					ctx.request_repaint_after(WINDOW_SIZE_REPAINT);
				}
			},
			// 刚开始变（或又变了）：重新计时
			_ => {
				self.window_size_pending = Some((size, Instant::now()));
				ctx.request_repaint_after(WINDOW_SIZE_REPAINT);
			},
		}
	}

	/// 弹一条普通提示。
	fn toast(&mut self, message: impl Into<String>) {
		self.toasts.info(message);
	}

	/// 弹一条错误提示（红色描边，比普通提示醒目）。
	fn toast_error(&mut self, message: impl Into<String>) {
		self.toasts.error(message);
	}

	/// 处理侧边栏（主机页）动作。
	fn on_host_list_action(&mut self, action: HostListAction) {
		match action {
			HostListAction::Connect(host) => self.connect_host(&host),
			HostListAction::NewHost => {
				self.form = Some(HostFormState::new_host());
			},
			HostListAction::Edit(host) => self.open_edit_form(&host),
			HostListAction::OpenFiles(host) => self.open_sftp(&host),
			HostListAction::Delete(id) => {
				self.pending_delete = self.hosts.iter().find(|host| host.id == Some(id)).cloned();
			},
			HostListAction::NewGroup { parent } => {
				self.group_form = Some(GroupFormState {
					editing: None,
					parent,
					name: String::new(),
				});
			},
			HostListAction::RequestRenameGroup { id, current } => {
				self.group_form = Some(GroupFormState {
					editing: Some(id),
					parent: None,
					name: current,
				});
			},
			HostListAction::RequestDeleteGroup { id } => {
				self.pending_delete_group = self
					.groups
					.iter()
					.find(|group| group.id == Some(id))
					.cloned();
			},
			HostListAction::MoveHostToGroup { host_id, group } => {
				self.move_host_to_group(host_id, group);
			},
		}
	}

	/// 提交分组名表单：按 `editing` 分派到新建或重命名。
	fn submit_group_form(&mut self, form: GroupFormState) {
		let name = form.name.trim().to_string();
		if name.is_empty() {
			self.toast_error("分组名称不能为空");
			return;
		}

		let Some(store) = self.store.as_ref() else {
			self.toast_error("加密库未打开，无法保存");
			return;
		};

		let outcome = match form.editing {
			Some(id) => store
				.rename_group(id, &name)
				.map(|()| format!("已重命名「{name}」")),
			None => store
				.create_group(&name, form.parent)
				.map(|_| format!("已新建分组「{name}」")),
		};

		match outcome {
			Ok(message) => {
				self.refresh_groups();
				self.toast(message);
			},
			Err(error) => {
				tracing::warn!("保存分组失败：{error}");
				self.toast_error(format!("保存分组失败：{error}"));
			},
		}
	}

	/// 删除待确认的分组。
	fn confirm_delete_group(&mut self) {
		let Some(group) = self.pending_delete_group.take() else {
			return;
		};
		let Some(id) = group.id else {
			return;
		};

		match self.store.as_ref().map(|store| store.delete_group(id)) {
			Some(Ok(true)) => {
				// 组内主机退回「未分组」、子分组级联删除——两个缓存都要重读
				self.refresh_groups();
				self.refresh_hosts();
				self.toast(format!("已删除分组「{}」", group.name));
			},
			Some(Ok(false)) => {
				self.toast(format!("分组「{}」已不存在", group.name));
				self.refresh_groups();
			},
			Some(Err(error)) => {
				self.toast_error(format!("删除分组失败：{error}"));
			},
			None => {
				self.toast_error("加密库未打开，无法删除");
			},
		}
	}

	/// 把主机移到另一个分组。
	///
	/// 走完整的 `save_host`——`hosts::update` 会整列覆盖凭据字段，
	/// 所以必须把已存的密码/口令原样读回来一起写，否则移个分组就把
	/// 密码清空了。
	fn move_host_to_group(&mut self, host_id: i64, group: Option<i64>) {
		let Some(store) = self.store.as_ref() else {
			self.toast_error("加密库未打开，无法移动");
			return;
		};
		let Some(host) = self.hosts.iter().find(|host| host.id == Some(host_id)) else {
			return;
		};

		let mut updated = host.clone();
		updated.group_id = group;
		let secrets = existing_secrets(store, host_id, &host.auth);

		let name = updated.name.clone();
		let target = group
			.and_then(|id| self.groups.iter().find(|g| g.id == Some(id)))
			.map_or_else(|| "未分组".to_string(), |g| g.name.clone());

		match store.save_host(&updated, &secrets) {
			Ok(_) => {
				self.refresh_hosts();
				self.toast(format!("「{name}」已移动到「{target}」"));
			},
			Err(error) => {
				tracing::warn!("移动主机失败：{error}");
				self.toast_error(format!("移动失败：{error}"));
			},
		}
	}

	/// 处理侧边栏（代理页）动作。
	fn on_proxy_list_action(&mut self, action: ProxyListAction) {
		match action {
			ProxyListAction::NewProxy => {
				self.proxy_form = Some(ProxyFormState::new_proxy());
			},
			ProxyListAction::Edit(proxy) => {
				self.proxy_form = Some(ProxyFormState::edit(&proxy));
			},
			ProxyListAction::Delete(id) => {
				self.pending_delete_proxy = self
					.proxies
					.iter()
					.find(|proxy| proxy.id == Some(id))
					.cloned();
			},
		}
	}

	/// 处理 RDP 画面上报的动作。
	fn on_rdp_action(&mut self, action: RdpAction) {
		match action {
			RdpAction::ScaleChanged { host_id, scale } => self.save_host_scale(host_id, scale),
			RdpAction::ResolutionChanged {
				host_id,
				resolution,
			} => self.save_host_resolution(host_id, resolution),
			RdpAction::DpiChanged { host_id, dpi } => self.save_host_dpi(host_id, dpi),
			RdpAction::Notice { text, is_error } => {
				if is_error {
					self.toast_error(&text);
				} else {
					self.toast(&text);
				}
			},
			RdpAction::TrustCertificate {
				host,
				port,
				fingerprint,
				subject,
				issuer,
			} => self.remember_rdp_cert(
				&host,
				port,
				&fingerprint,
				subject.as_deref(),
				issuer.as_deref(),
			),
		}
	}

	/// 查这台机器记录过的 RDP 证书指纹。库没打开时按首连处理。
	fn rdp_cert_of(&self, host: &str, port: u16) -> Option<String> {
		let store = self.store.as_ref()?;
		match store.rdp_cert(host, port) {
			Ok(fingerprint) => fingerprint,
			Err(error) => {
				// 查不到不能让连接失败：按首连处理，连上后会重新记录
				tracing::warn!("读取 RDP 证书记录失败（{host}:{port}）：{error}");
				None
			},
		}
	}

	/// 首次连接的证书指纹入库，并给用户一句可核对的提示。
	fn remember_rdp_cert(
		&mut self,
		host: &str,
		port: u16,
		fingerprint: &str,
		subject: Option<&str>,
		issuer: Option<&str>,
	) {
		let Some(store) = self.store.as_ref() else {
			return;
		};
		match store.remember_rdp_cert(host, port, fingerprint, subject, issuer) {
			Ok(()) => {
				let shown = daisy_rdp::format_fingerprint(fingerprint);
				tracing::info!("已记录 {host}:{port} 的 RDP 证书指纹 {shown}");
				self.toast(format!("首次连接 {host}，已记录证书指纹 {shown}"));
			},
			Err(error) => tracing::warn!("记录 RDP 证书指纹失败：{error}"),
		}
	}

	/// 把缩放设置写回主机记录。
	///
	/// **必须带上原凭据**：`hosts::update` 是整列覆盖，只写非凭据字段会把密码
	/// 清空——回归测试见 `tests::moving_host_keeps_password`。
	fn save_host_scale(&mut self, host_id: Option<i64>, scale: ScaleSettings) {
		// 没有对应主机记录（例如演示数据造的会话）时，只在本标签页里生效
		let Some(id) = host_id else {
			return;
		};
		let Some(store) = self.store.as_ref() else {
			self.toast_error("加密库未打开，缩放设置没能保存");
			return;
		};
		let Some(host) = self.hosts.iter_mut().find(|host| host.id == Some(id)) else {
			return;
		};

		host.display.scale = scale;
		let secrets = existing_secrets(store, id, &host.auth);
		let name = host.name.clone();
		if let Err(error) = store.save_host(host, &secrets) {
			self.toast_error(format!("保存缩放设置失败：{error}"));
			return;
		}
		tracing::info!("已保存主机「{name}」的缩放设置：{:?}", scale.mode);
	}

	/// 把远端分辨率写回主机记录——「下次打开还是这个尺寸」靠的就是它。
	///
	/// 和缩放一样必须带上原凭据（`hosts::update` 整列覆盖）。
	fn save_host_resolution(&mut self, host_id: Option<i64>, resolution: Resolution) {
		// 没有对应主机记录（例如演示数据造的会话）时，只在本标签页里生效
		let Some(id) = host_id else {
			return;
		};
		let Some(store) = self.store.as_ref() else {
			self.toast_error("加密库未打开，分辨率没能记住");
			return;
		};
		let Some(host) = self.hosts.iter_mut().find(|host| host.id == Some(id)) else {
			return;
		};
		if host.display.resolution == resolution {
			return;
		}

		host.display.resolution = resolution;
		let secrets = existing_secrets(store, id, &host.auth);
		let name = host.name.clone();
		if let Err(error) = store.save_host(host, &secrets) {
			self.toast_error(format!("记住分辨率失败：{error}"));
			return;
		}
		tracing::info!("已保存主机「{name}」的分辨率：{resolution}");
	}

	/// 把「内容放大倍数」写回主机记录——与分辨率同一条链路：选了就记住，
	/// 下次连接由 `DisplayReady` 后的第一次分辨率请求带下去。
	fn save_host_dpi(&mut self, host_id: Option<i64>, dpi: u32) {
		// 没有对应主机记录（例如演示数据造的会话）时，只在本标签页里生效
		let Some(id) = host_id else {
			return;
		};
		let Some(store) = self.store.as_ref() else {
			self.toast_error("加密库未打开，缩放设置没能记住");
			return;
		};
		let Some(host) = self.hosts.iter_mut().find(|host| host.id == Some(id)) else {
			return;
		};
		if host.display.dpi == dpi {
			return;
		}

		host.display.dpi = dpi;
		let secrets = existing_secrets(store, id, &host.auth);
		let name = host.name.clone();
		if let Err(error) = store.save_host(host, &secrets) {
			self.toast_error(format!("记住缩放设置失败：{error}"));
			return;
		}
		tracing::info!("已保存主机「{name}」的内容放大倍数：{dpi}%");
	}

	/// 库里的主机名，按诊断需要以转义形式呈现。
	fn host_names(&self) -> Vec<&str> {
		self.hosts.iter().map(|host| host.name.as_str()).collect()
	}

	/// 连接一台主机。SSH 走 `daisy-ssh`；RDP 目前只支持直连。
	fn connect_host(&mut self, host: &Host) {
		// 先取只读数据再动 sessions——`store` 的借用不能跨过可变借用
		let plan = match self
			.store
			.as_ref()
			.map(|store| build_connection_plan(store, host, &self.hosts))
		{
			Some(Ok(plan)) => plan,
			Some(Err(reason)) => {
				// 一定要落日志：这类失败只弹一条 6 秒的浮层，用户回头想查
				// 「为什么连不上」时，日志里什么都没有就没法排查了
				tracing::warn!("连接「{}」失败：{reason}", host.name);
				self.toast_error(reason);
				return;
			},
			None => {
				tracing::warn!("连接「{}」失败：加密库未打开", host.name);
				self.toast_error("加密库未打开，无法连接");
				return;
			},
		};

		let endpoint = (host.host.clone(), host.port);
		match plan {
			ConnectionPlan::Direct(params) => {
				self.sessions.open_ssh(params, host.name.clone(), endpoint);
			},
			ConnectionPlan::Rdp(params) => {
				// 证书指纹在**开连之前**查好交给视图：握手一开始就要用
				let expected_cert = self.rdp_cert_of(&host.host, host.port);
				self.sessions.open_rdp(
					host.name.clone(),
					params,
					host.id,
					host.display,
					expected_cert,
				);
			},
			ConnectionPlan::Jump {
				jump,
				target,
				auth,
				host_key,
			} => {
				let spec = connect::JumpSpec {
					jump,
					target,
					auth,
					host_key,
				};
				self.sessions
					.open_ssh_via_jump(spec, host.name.clone(), endpoint);
			},
		}
	}

	/// 保存表单结果。
	///
	/// 「留空即保持原值」在这里兑现：`hosts::update` 整列覆盖 secret_enc，
	/// 直接传空会把已存密码清掉，所以编辑时必须把原值读回来一起写。
	fn save_host(&mut self, input: HostFormInput) {
		let Some(store) = self.store.as_ref() else {
			self.toast_error("加密库未打开，无法保存");
			return;
		};

		let id = input.host.id;
		let secrets = match &input.host.auth {
			AuthMethod::Password => {
				if input.password.is_empty() {
					// 未改动：读回原密码；本来就没有则为空
					id.map_or_else(
						|| HostSecrets {
							password: None,
							key_passphrase: None,
						},
						|id| existing_secrets(store, id, &input.host.auth),
					)
				} else {
					HostSecrets::with_password(&input.password)
				}
			},
			AuthMethod::PublicKey(_) => {
				let passphrase = if input.passphrase.is_empty() {
					id.and_then(|id| store.key_passphrase(id).ok().flatten())
				} else {
					Some(zeroize::Zeroizing::new(input.passphrase.clone()))
				};
				HostSecrets {
					password: None,
					key_passphrase: passphrase,
				}
			},
		};

		let name = input.host.name.clone();
		match store.save_host(&input.host, &secrets) {
			Ok(_) => {
				self.refresh_hosts();
				self.toast(format!("已保存「{name}」"));
			},
			Err(error) => {
				tracing::warn!("保存主机失败：{error}");
				self.toast_error(format!("保存失败：{error}"));
			},
		}
	}

	/// 删除待确认的主机。
	fn confirm_delete(&mut self) {
		let Some(host) = self.pending_delete.take() else {
			return;
		};
		let Some(id) = host.id else {
			return;
		};

		match self.store.as_ref().map(|store| store.delete_host(id)) {
			Some(Ok(true)) => {
				self.refresh_hosts();
				self.toast(format!("已删除「{}」", host.name));
			},
			Some(Ok(false)) => {
				self.toast(format!("「{}」已不存在", host.name));
				self.refresh_hosts();
			},
			Some(Err(error)) => {
				self.toast_error(format!("删除失败：{error}"));
			},
			None => {
				self.toast_error("加密库未打开，无法删除");
			},
		}
	}

	/// 保存代理表单结果。
	fn save_proxy(&mut self, input: ProxyFormInput) {
		let Some(store) = self.store.as_ref() else {
			self.toast_error("加密库未打开，无法保存");
			return;
		};

		let id = input.proxy.id;
		// 与主机同样：留空 = 保持原密码，得把原值读回来
		let password = if input.password.is_empty() {
			id.and_then(|id| store.proxy_password(id).ok().flatten())
				.map(|existing| existing.as_str().to_owned())
		} else {
			Some(input.password.clone())
		};

		let name = input.proxy.name.clone();
		match store.save_proxy(&input.proxy, password.as_deref()) {
			Ok(_) => {
				self.refresh_proxies();
				self.toast(format!("已保存代理「{name}」"));
			},
			Err(error) => {
				tracing::warn!("保存代理失败：{error}");
				self.toast_error(format!("保存失败：{error}"));
			},
		}
	}

	/// 删除待确认的代理。
	fn confirm_delete_proxy(&mut self) {
		let Some(proxy) = self.pending_delete_proxy.take() else {
			return;
		};
		let Some(id) = proxy.id else {
			return;
		};

		match self.store.as_ref().map(|store| store.delete_proxy(id)) {
			Some(Ok(true)) => {
				self.refresh_proxies();
				self.toast(format!("已删除代理「{}」", proxy.name));
			},
			Some(Ok(false)) => {
				self.toast(format!("代理「{}」已不存在", proxy.name));
				self.refresh_proxies();
			},
			Some(Err(error)) => {
				self.toast_error(format!("删除失败：{error}"));
			},
			None => {
				self.toast_error("加密库未打开，无法删除");
			},
		}
	}

	/// 收起连通性探测的结果（如果有）。
	fn pump_probe(&mut self) {
		let Some(rx) = self.probe_rx.as_ref() else {
			return;
		};
		// 还没结果或发送端已丢弃（表单关了）——下帧再看
		if let Ok(outcome) = rx.try_recv() {
			self.probe_rx = None;
			if let Some(state) = self.form.as_mut() {
				state.set_probe_result(outcome.reachable, outcome.detail);
			}
		}
	}

	/// 把 [`SMOKE_TERM_INPUT_ENV`] 里那串文本敲进活动终端。
	///
	/// 换行一律转成回车：终端里 Enter 的语义是 `\r`，`\n` 在 raw 模式下
	/// 只换行、不执行命令。`\n` 与字面量 `\n` 两种写法都认，
	/// 免得为了一个换行去跟 shell 的引号规则较劲。
	fn inject_smoke_terminal_input(&mut self) {
		let Some(text) = self.smoke_term_input.clone() else {
			return;
		};
		let bytes = text.replace("\\n", "\n").replace('\n', "\r").into_bytes();
		if self.sessions.write_to_active_terminal(&bytes) {
			tracing::info!("终端注入钩子：已写入 {} 字节", bytes.len());
		} else {
			tracing::warn!("终端注入钩子：活动标签页不是终端，已跳过");
		}
	}
}

impl eframe::App for DaisyApp {
	/// 每帧绘制之前的非绘制逻辑。
	fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
		// 关闭排障打点：只打一次。有这条但没「run_native 已正常返回」，
		// 说明事件循环收到了关闭请求却没走完退出流程。
		if ctx.input(|input| input.viewport().close_requested()) {
			static CLOSE_TRACED: std::sync::atomic::AtomicBool =
				std::sync::atomic::AtomicBool::new(false);
			if !CLOSE_TRACED.swap(true, std::sync::atomic::Ordering::Relaxed) {
				tracing::info!("[关闭] egui 收到 close_requested，开始收尾");
			}
		}
		// 淡出要靠连续重绘驱动——事件循环睡着了提示会卡在最后一帧
		if self.toasts.is_active() {
			ctx.request_repaint_after(Duration::from_millis(50));
		}

		if let Some(deadline) = self.smoke_deadline {
			if Instant::now() >= deadline {
				ctx.send_viewport_cmd(egui::ViewportCommand::Close);
			} else {
				// 否则事件循环会一直睡，永远醒不过来检查截止时间
				ctx.request_repaint_after(Duration::from_millis(50));
			}
		}

		// 终端注入钩子：到点敲一次，没到点就别让事件循环睡死
		if !self.smoke_term_sent
			&& let Some(at) = self.smoke_term_at
		{
			if Instant::now() >= at {
				self.smoke_term_sent = true;
				self.inject_smoke_terminal_input();
			} else {
				ctx.request_repaint_after(Duration::from_millis(50));
			}
		}

		// 字体切换钩子：按时间表改 font_family，剩下的交给 sync_config
		while let Some((at, family)) = self.smoke_fonts.first().cloned() {
			if Instant::now() < at {
				ctx.request_repaint_after(Duration::from_millis(50));
				break;
			}
			self.smoke_fonts.remove(0);
			tracing::info!("字体切换钩子：font_family = {family:?}");
			self.config.terminal.font_family = family;
		}

		// 窗口尺寸钩子：改一次窗口，之后交给「观察 → 稳定 → 落盘」的真实流程
		if let Some(at) = self.smoke_resize_at {
			if Instant::now() >= at {
				self.smoke_resize_at = None;
				if let Some([width, height]) = smoke_resize_size() {
					tracing::info!("窗口尺寸钩子：改为 {width}×{height}");
					ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
						width, height,
					)));
					// 顺便把窗口拉回前台：无人值守截图要能看见画面，
					// 否则会被别的应用整个盖住（真机踩过三次）。
					ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
				}
			} else {
				ctx.request_repaint_after(Duration::from_millis(50));
			}
		}
	}

	fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
		self.bootstrap(ui.ctx());
		// 换过字体：这一趟 `begin_pass` 已经把新字体换上了，现在量才准。
		// 放在 `sync_config` 之前——它本帧可能又置一次标志，那要留到下一帧。
		if std::mem::take(&mut self.appearance_pending) {
			self.sessions
				.set_font_size(ui.ctx(), self.config.terminal.font_size);
		}
		// 多屏副屏未激活时，系统可能把窗口摆到用户看不见的坐标。
		// 首帧检查一次，离屏才挪主屏居中；在屏内则不打扰。
		if !self.window_checked {
			self.window_checked = true;
			placement::ensure_window_visible(frame, ui.ctx(), WINDOW_SIZE);
		}
		// 先记尺寸再同步配置：这样「攒够时间」的那一帧就能立刻落盘，
		// 不用等下一次输入事件把事件循环叫醒
		self.remember_window_size(ui.ctx());
		self.sync_config(ui.ctx());
		self.pump_probe();

		// 异步连接结果。store 借用与 sessions 可变借用是不同字段，允许并存
		if let Some(store) = self.store.as_ref() {
			self.sessions.poll(ui.ctx(), store);
		}

		// 弹出去的 RDP 桌面每帧都要画，**与当前看的是哪个标签页无关**：
		// `ui_active` 只跑激活的那一个，切走之后 egui 会把没画到的视口
		// 判为「本帧未使用」直接关掉——那正是「独立窗口自己消失了」
		self.sessions.detached_viewports(ui.ctx());

		let tokens = design::tokens(ui.ctx());

		// ---- 状态栏 ----
		// 最先添加才能横跨整个窗口宽度；晚于侧边栏就只剩右侧那半截。
		egui::Panel::bottom("status_bar")
			.frame(
				panel_frame(tokens.bg_deep, 12, 5)
					.stroke(egui::Stroke::new(0.5, tokens.border_soft)),
			)
			.show(ui, |ui| {
				ui.horizontal(|ui| {
					// 一个点的信息量比「已加密」四个字更直接：
					// 一眼就知道加密库开没开
					let ok = self.store.is_some();
					let (dot, _) =
						ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
					ui.painter().circle_filled(
						dot.center(),
						3.0,
						if ok { tokens.success } else { tokens.danger },
					);
					design::caption(
						ui,
						if ok {
							"已加密"
						} else {
							"加密库未打开"
						},
						tokens.text_muted,
					);

					ui.add_space(design::GAP_MD);
					design::caption(
						ui,
						format!(
							"主机 {} · 代理 {} · 会话 {}",
							self.hosts.len(),
							self.proxies.len(),
							self.sessions.len()
						),
						tokens.text_muted,
					);

					ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
						design::caption(
							ui,
							format!("v{}", daisy_core::APP_VERSION),
							tokens.text_faint,
						);
						ui.add_space(design::GAP_XS);
						// 设置不是高频操作，放进状态栏比占着侧边栏顶格好
						if design::ghost_button(ui, "设置").clicked() {
							self.settings_open = true;
						}
					});
				});
			});

		// ---- 侧边栏 ----
		egui::Panel::left("sidebar")
			.frame(
				panel_frame(tokens.bg_surface, 10, 10)
					.stroke(egui::Stroke::new(0.5, tokens.border)),
			)
			.default_size(SIDEBAR_WIDTH)
			.resizable(true)
			.show(ui, |ui| {
				if let Some(error) = &self.store_error {
					let error = error.clone();
					ui.colored_label(tokens.danger, "加密库打不开：");
					ui.label(error);
					ui.label(
						egui::RichText::new("主机与凭据都不可用。修复后重启应用。")
							.size(design::FONT_XS)
							.color(tokens.text_muted),
					);
					return;
				}

				// 主机 / 代理：做成一个分段控件而不是两个并排按钮。
				// 后者选中时各自填一块强调色，看起来像两个独立开关，
				// 而它们其实是同一个视图的两档。
				let current = match self.sidebar_tab {
					SidebarTab::Hosts => 0,
					SidebarTab::Proxies => 1,
				};
				if let Some(index) = design::segmented(ui, &["主机", "代理"], current) {
					self.sidebar_tab = if index == 0 {
						SidebarTab::Hosts
					} else {
						SidebarTab::Proxies
					};
				}

				ui.add_space(design::GAP_SM);

				// 整个侧边栏只有这一个主操作。原来四颗等宽按钮
				// （新建主机 / 新建分组 / 编辑 / 删除）并排，
				// 「删除」和「新建」一样显眼，主次全无。
				//
				// 「新建分组」并排放在主按钮右边：它原先只是搜索框右侧的
				// 一枚裸加号，谁也读不出意思。四个字写全、紧挨着「新建主机」，
				// 用户一眼就能分清「建主机」和「建分组」。
				match self.sidebar_tab {
					SidebarTab::Hosts => {
						ui.horizontal(|ui| {
							ui.spacing_mut().item_spacing.x = design::GAP_SM;
							// 宽度：次按钮按文字量出来，主按钮吃掉剩下的
							let group_width = design::outline_button_width(ui, "新建分组");
							let primary_width =
								(ui.available_width() - group_width - design::GAP_SM).max(96.0);
							if design::block_button_sized(ui, "新建主机", primary_width).clicked()
							{
								self.on_host_list_action(HostListAction::NewHost);
							}
							if design::outline_button(ui, "新建分组", group_width).clicked() {
								self.on_host_list_action(HostListAction::NewGroup { parent: None });
							}
						});
					},
					SidebarTab::Proxies => {
						if design::block_button(ui, "新建代理").clicked() {
							self.on_proxy_list_action(ProxyListAction::NewProxy);
						}
					},
				}

				ui.add_space(design::GAP_SM);

				match self.sidebar_tab {
					SidebarTab::Hosts => {
						let action = views::host_list::ui(
							ui,
							&mut self.host_list,
							&self.hosts,
							&self.groups,
						);
						if let Some(action) = action {
							self.on_host_list_action(action);
						}
					},
					SidebarTab::Proxies => {
						let action = views::proxy_list::ui(ui, &mut self.proxy_list, &self.proxies);
						if let Some(action) = action {
							self.on_proxy_list_action(action);
						}
					},
				}
			});

		// ---- 标签页栏 ----
		// 只在内容区上方：侧边栏是独立的一列，标签不该骑在分组树上。
		// 面板按添加顺序嵌套，所以必须先铺侧边栏再加这一条。
		egui::Panel::top("tab_bar")
			.frame(panel_frame(tokens.bg_base, 10, 3))
			.show(ui, |ui| {
				self.sessions.ui_tabs(ui);
			});

		// ---- 活动终端（CentralPanel 必须最后添加）----
		// 画面里的动作（如改了缩放）要等面板的借用结束后才能处理——
		// 处理时要动 `self.hosts` 与 `self.store`
		// 底色取最深的终端色：终端与 RDP 都自己铺满画面，
		// 但空会话/加载中会露出这层底，用深色才不会和侧边栏糊在一起。
		let rdp_action = egui::CentralPanel::default()
			.frame(egui::Frame::NONE.fill(tokens.bg_deep))
			.show(ui, |ui| self.sessions.ui_active(ui))
			.inner;
		if let Some(action) = rdp_action {
			self.on_rdp_action(action);
		}

		// 终端标签页攒下的提示（复制/粘贴失败、剪贴板为空）在这里变成 toast。
		// 视图层自己弹不出浮层——它只拿得到 `Ui`，拿不到 toast 队列。
		if let Some(message) = self.sessions.take_notice() {
			self.toasts.error(message);
		}

		// ---- 全局提示浮层 ----
		// Area 是独立图层，不受 CentralPanel「必须最后」的约束
		self.toasts.ui(ui.ctx());

		// ---- 主机表单窗口 ----
		if self.form.is_some() {
			let mut state = self.form.take().expect("上面刚判断过");

			// 无人值守验证：给表单预填一次内容（见 [`apply_host_form_smoke_hooks`]）
			if !self.smoke_form_hooks_done {
				self.smoke_form_hooks_done = true;
				apply_host_form_smoke_hooks(&mut state);
			}

			// 「重新信任」：删掉这台机器的证书记录
			if let Some((host, port)) = state.take_forget_cert_request()
				&& let Err(error) = self.forget_rdp_cert(&host, port)
			{
				// 删失败要把指纹填回去，否则界面在骗人
				let shown = self
					.rdp_cert_of(&host, port)
					.map(|fingerprint| daisy_rdp::format_fingerprint(&fingerprint));
				state.set_known_cert(shown, &host, port);
				self.toast_error(error);
			}

			// 连通性测试：取走点击 → 起探测（跑在 tokio 上，不卡界面）
			if state.take_probe_request() {
				let (host, port) = state.probe_target();
				if host.is_empty() {
					state.set_probe_result(false, "先填写地址");
				} else {
					state.set_probe_testing();
					self.probe_rx = Some(self.sessions.connector().probe_tcp(host, port));
				}
			}

			match views::host_form::ui(
				ui.ctx(),
				&mut state,
				&self.proxies,
				&self.hosts,
				&self.groups,
			) {
				HostFormOutcome::Open => self.form = Some(state),
				HostFormOutcome::Cancelled => {
					self.form = None;
					self.probe_rx = None;
				},
				HostFormOutcome::Save(input) => {
					self.form = None;
					self.probe_rx = None;
					self.save_host(*input);
				},
			}
		}

		// ---- 代理表单窗口 ----
		if self.proxy_form.is_some() {
			let mut state = self.proxy_form.take().expect("上面刚判断过");
			match views::proxy_form::ui(ui.ctx(), &mut state) {
				ProxyFormOutcome::Open => self.proxy_form = Some(state),
				ProxyFormOutcome::Cancelled => {
					self.proxy_form = None;
				},
				ProxyFormOutcome::Save(input) => {
					self.proxy_form = None;
					self.save_proxy(*input);
				},
			}
		}

		// ---- 删除确认窗口（主机）----
		if let Some(host) = self.pending_delete.clone() {
			let mut confirmed = false;
			let mut cancelled = false;
			let mut open = true;
			views::dialog(ui.ctx(), "删除主机", [380.0, 200.0], &mut open, |ui| {
				ui.label(format!("确定删除「{}」吗？", host.name));
				// 地址单独一行：并进上一句会顶到 380 宽之外，
				// 折行时那个「吗？」会被甩到第二行开头，很难看。
				ui.weak(daisy_core::endpoint(&host.host, host.port));
				ui.weak("删除后无法恢复，已保存的密码会一并删除。");
				ui.add_space(8.0);
				ui.horizontal(|ui| {
					confirmed = ui.button("删除").clicked();
					cancelled = ui.button("取消").clicked();
				});
			});
			if !open {
				cancelled = true;
			}

			if confirmed {
				self.confirm_delete();
			}
			if cancelled {
				self.pending_delete = None;
			}
		}

		// ---- 删除确认窗口（代理）----
		if let Some(proxy) = self.pending_delete_proxy.clone() {
			let mut confirmed = false;
			let mut cancelled = false;
			let mut open = true;
			views::dialog(ui.ctx(), "删除代理", [420.0, 200.0], &mut open, |ui| {
				ui.label(format!("确定删除代理「{}」吗？", proxy.name));
				ui.weak(format!(
					"{} {}",
					proxy.kind,
					daisy_core::endpoint(&proxy.host, proxy.port)
				));
				ui.weak("删除后无法恢复。引用该代理的主机将回退为直连。");
				ui.add_space(8.0);
				ui.horizontal(|ui| {
					confirmed = ui.button("删除").clicked();
					cancelled = ui.button("取消").clicked();
				});
			});
			if !open {
				cancelled = true;
			}

			if confirmed {
				self.confirm_delete_proxy();
			}
			if cancelled {
				self.pending_delete_proxy = None;
			}
		}

		// ---- 分组名输入窗口（新建 / 重命名共用）----
		if let Some(mut form) = self.group_form.take() {
			let mut submit = false;
			let mut cancelled = false;
			let title = if form.editing.is_some() {
				"重命名分组"
			} else {
				"新建分组"
			};

			let mut show_open = true;
			views::dialog(ui.ctx(), title, [400.0, 205.0], &mut show_open, |ui| {
				if form.editing.is_none() && form.parent.is_some() {
					let parent = form
						.parent
						.and_then(|id| self.groups.iter().find(|g| g.id == Some(id)));
					if let Some(parent) = parent {
						ui.weak(format!("上级：{}", parent.name));
					}
				}
				views::text_row(
					ui,
					egui::TextEdit::singleline(&mut form.name).hint_text("分组名称"),
				);
				ui.add_space(8.0);
				ui.horizontal(|ui| {
					submit = ui.button("确定").clicked();
					cancelled = ui.button("取消").clicked();
				});
			});
			if !show_open {
				cancelled = true;
			}

			// 回车即提交——输入框只有一项，用户不该被迫去点按钮
			let enter = !cancelled && ui.ctx().input(|input| input.key_pressed(egui::Key::Enter));

			if submit || enter {
				self.submit_group_form(form);
			} else if !cancelled {
				self.group_form = Some(form);
			}
		}

		// ---- 删除确认窗口（分组）----
		if let Some(group) = self.pending_delete_group.clone() {
			let mut confirmed = false;
			let mut cancelled = false;
			let mut open = true;
			views::dialog(ui.ctx(), "删除分组", [420.0, 200.0], &mut open, |ui| {
				ui.label(format!("确定删除分组「{}」吗？", group.name));
				ui.weak("子分组会一并删除；组内主机不会被删除，会退回「未分组」。");
				ui.add_space(8.0);
				ui.horizontal(|ui| {
					confirmed = ui.button("删除").clicked();
					cancelled = ui.button("取消").clicked();
				});
			});
			if !open {
				cancelled = true;
			}

			if confirmed {
				self.confirm_delete_group();
			}
			if cancelled {
				self.pending_delete_group = None;
			}
		}

		// ---- 设置窗口 ----
		if self.settings_open {
			match settings::ui(ui.ctx(), &mut self.config, &self.paths.log_dir()) {
				SettingsOutcome::Open => {},
				SettingsOutcome::Closed => self.settings_open = false,
				SettingsOutcome::OpenLogDir => {
					if let Err(error) = logging::open_log_dir(&self.paths.log_dir()) {
						self.toast_error(error.to_string());
					}
				},
			}
		}

		// ---- 文件管理（SFTP）----
		//
		// 每次传输都要刷新，所以窗口打开期间按「有活动就续帧」处理
		if let Some(sftp) = &mut self.sftp {
			sftp.ui(ui.ctx());
			if !sftp.is_open() {
				self.sftp = None;
			}
		}
	}
}

/// 读回主机已存的凭据，按认证方式取对应那一项。
///
/// `hosts::update` 整列覆盖凭据字段——任何「只改非凭据字段」的保存
/// （改分组、改端口）都必须把原值带回，否则密码会被静默清空。
fn existing_secrets(store: &Store, id: i64, auth: &AuthMethod) -> HostSecrets {
	match auth {
		AuthMethod::Password => match store.password(id).ok().flatten() {
			Some(existing) => HostSecrets::with_password(existing.as_str()),
			None => HostSecrets {
				password: None,
				key_passphrase: None,
			},
		},
		AuthMethod::PublicKey(_) => HostSecrets {
			password: None,
			key_passphrase: store.key_passphrase(id).ok().flatten(),
		},
	}
}

/// 从主机与加密库构造连接计划。
///
/// 独立成函数是为了让 `connect_host` 能先取完只读数据再动 `sessions`——
/// 混在一起会撞 Rust 的借用检查。
fn build_connection_plan(
	store: &Store,
	host: &Host,
	hosts: &[Host],
) -> Result<ConnectionPlan, String> {
	if host.protocol == Protocol::Rdp {
		return build_rdp_plan(store, host);
	}

	let id = host.id.ok_or("主机尚未保存，无法连接")?;
	let user = host
		.username
		.clone()
		.ok_or("未填写登录用户名，请先编辑主机")?;

	let auth = match &host.auth {
		AuthMethod::Password => {
			let password = store
				.password(id)
				.map_err(|error| error.to_string())?
				.ok_or("未保存密码。请编辑主机填写密码后再连接")?;
			AuthSpec::Password { password }
		},
		AuthMethod::PublicKey(pk) => {
			let passphrase = store
				.key_passphrase(id)
				.map_err(|error| error.to_string())?;
			AuthSpec::PublicKey {
				path: PathBuf::from(&pk.key_path),
				passphrase,
			}
		},
	};

	// known_hosts 里只存了指纹（schema 未收算法名）。核对本来就只比指纹，
	// key_type 仅作展示，留空不影响判定。
	let policy = host_key_policy(store, host)?;

	let target = SshTarget::new(host.host.clone(), host.port, user);

	// 有跳板机就走跳板机；否则代理（如有）作用于直连。
	// 两条路消耗 auth 的方式不同（跳板机要把它原样交给内层连接），
	// 因此这里不做 auth 的克隆——分支各自取走。
	let Some(jump_id) = host.jump_host_id else {
		let params = SshParams::new(target, auth)
			.with_host_key_policy(policy)
			.with_rsa_policy(RsaSigningPolicy::MODERN);
		let params = match proxy_dialer(store, host.proxy_id)? {
			Some(dialer) => params.with_dialer(dialer),
			None => params,
		};
		return Ok(ConnectionPlan::Direct(params));
	};

	let jump_host = hosts
		.iter()
		.find(|candidate| candidate.id == Some(jump_id))
		.ok_or("跳板机不存在或已删除，请编辑主机")?;
	if jump_host.protocol != Protocol::Ssh {
		return Err("跳板机必须是 SSH 主机".to_string());
	}
	if jump_host.jump_host_id.is_some() {
		// 链式跳板机意味着跳板机自己也要经第三台，成环排查成本高，v1 明确不支持
		return Err("不支持多级跳板机（跳板机自身不能再配跳板机）".to_string());
	}

	let jump = base_ssh_params(store, jump_host)?;
	let jump = match proxy_dialer(store, jump_host.proxy_id)? {
		Some(dialer) => jump.with_dialer(dialer),
		None => jump,
	};

	Ok(ConnectionPlan::Jump {
		jump,
		target,
		auth,
		// 目标的密钥策略同样要带进跳板机路径，否则 known_hosts 核对在
		// 这条路上静默失效（见 connect_via_jump 的文档）
		host_key: policy,
	})
}

/// 构造 RDP 连接参数。
///
/// RDP 目前**只支持直连**：经代理或跳板机需要 FreeRDP 侧的通道支持，
/// 而 SSH 那套 `Dialer` 是 tokio 的，交给 FreeRDP 用不上。
fn build_rdp_plan(store: &Store, host: &Host) -> Result<ConnectionPlan, String> {
	let id = host.id.ok_or("主机尚未保存，无法连接")?;
	let user = host
		.username
		.clone()
		.ok_or("未填写登录用户名，请先编辑主机")?;
	let password = store
		.password(id)
		.map_err(|error| error.to_string())?
		.ok_or("未保存密码。请编辑主机填写密码后再连接")?;

	// 地址经 `RdpTarget` 规范化：它的 `hostname()` 不带方括号，
	// 而带方括号的 IPv6 会被 FreeRDP 的 getaddrinfo 直接拒掉
	let target = RdpTarget::new(&host.host, host.port).map_err(|error| error.to_string())?;

	let (width, height) = match host.display.resolution {
		Resolution::Fixed { width, height } => (width, height),
		// 记住了上一次跟随目标就直接用它建连——首帧即正确分辨率，
		// 没有「1280×800 首帧 → disp 收口」的跳变（用户报的「跳回去」）
		Resolution::FitWindowAt { width, height } => (width, height),
		Resolution::FitWindow => (FIT_WINDOW_START_WIDTH, FIT_WINDOW_START_HEIGHT),
	};

	let params = ConnectionParams::new(target.hostname(), user, password.as_str())
		.with_port(target.port())
		.with_resolution(width, height)
		// 主机配置里的「映射剪贴板」勾选项（对应 mstsc 连接前那个）
		.with_clipboard(host.clipboard);

	Ok(ConnectionPlan::Rdp(params))
}

/// 主机密钥核对策略：库里有记录就按指纹比对，没有就首次连接接受并上报。
fn host_key_policy(store: &Store, host: &Host) -> Result<HostKeyPolicy, String> {
	match store
		.known_host(&host.host, host.port)
		.map_err(|error| error.to_string())?
	{
		Some(known) => Ok(HostKeyPolicy {
			known: Some(TrustedHostKey {
				key_type: String::new(),
				fingerprint: known.fingerprint,
			}),
			accept_unknown: true,
		}),
		None => Ok(HostKeyPolicy::first_connection()),
	}
}

/// 构造跳板机自身用的连接参数（不含跳板机与代理选择）。
fn base_ssh_params(store: &Store, host: &Host) -> Result<SshParams, String> {
	let id = host.id.ok_or("跳板机尚未保存")?;
	let user = host.username.clone().ok_or("跳板机未填写登录用户名")?;

	let auth = match &host.auth {
		AuthMethod::Password => {
			let password = store
				.password(id)
				.map_err(|error| error.to_string())?
				.ok_or("跳板机未保存密码")?;
			AuthSpec::Password { password }
		},
		AuthMethod::PublicKey(pk) => {
			let passphrase = store
				.key_passphrase(id)
				.map_err(|error| error.to_string())?;
			AuthSpec::PublicKey {
				path: PathBuf::from(&pk.key_path),
				passphrase,
			}
		},
	};

	let policy = host_key_policy(store, host)?;
	Ok(
		SshParams::new(SshTarget::new(host.host.clone(), host.port, user), auth)
			.with_host_key_policy(policy)
			.with_rsa_policy(RsaSigningPolicy::MODERN),
	)
}

/// 按代理主键构造拨号器。没有代理或未保存时返回 `None`（直连）。
fn proxy_dialer(
	store: &Store,
	proxy_id: Option<i64>,
) -> Result<Option<Box<dyn daisy_net::Dialer>>, String> {
	let Some(proxy_id) = proxy_id else {
		return Ok(None);
	};

	let proxy = store
		.proxies()
		.map_err(|error| error.to_string())?
		.into_iter()
		.find(|proxy| proxy.id == Some(proxy_id))
		.ok_or("代理不存在或已删除，请编辑主机")?;

	let mut endpoint = ProxyEndpoint::new(proxy.kind, &proxy.host, proxy.port)
		.map_err(|error| error.to_string())?;

	// 有用户名才可能是需要认证的代理；密码没配就当匿名用
	if let Some(username) = proxy.username.clone() {
		let password = store
			.proxy_password(proxy_id)
			.map_err(|error| error.to_string())?
			.ok_or("代理配置了用户名但未保存密码")?;
		endpoint = endpoint.with_auth(ProxyAuth::new(username, password.as_str()));
	}

	Ok(Some(build_dialer(Some(endpoint), Duration::from_secs(8))))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 守住「移个分组把密码清空」这个回归点。
	///
	/// `hosts::update` 整列覆盖凭据字段：只改 `group_id` 的保存若不带回
	/// 原密码，密码就被静默清掉。`move_host_to_group` 依赖
	/// [`existing_secrets`] 读回原值——这里把这条不变量钉住。
	#[test]
	fn moving_host_keeps_password() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let store = Store::open_at(&AppPaths::from_base(dir.path())).expect("开库失败");
		let root = store.create_group("生产", None).expect("建分组失败");

		let mut host = Host::draft(Protocol::Ssh);
		host.name = "回归机".to_string();
		host.host = "10.0.0.1".to_string();
		host.username = Some("tester".to_string());
		let id = store
			.save_host(&host, &HostSecrets::with_password("keep-me"))
			.expect("保存失败");

		// 模拟「移动到分组」：只改 group_id，凭据从库里读回
		let mut moved = host.clone();
		moved.id = Some(id);
		moved.group_id = Some(root);
		let secrets = existing_secrets(&store, id, &AuthMethod::Password);
		store.save_host(&moved, &secrets).expect("保存失败");

		let reloaded = store
			.hosts()
			.expect("列表失败")
			.into_iter()
			.find(|host| host.id == Some(id))
			.expect("主机应存在");
		assert_eq!(reloaded.group_id, Some(root));
		assert_eq!(
			store
				.password(id)
				.expect("读密码失败")
				.expect("应有密码")
				.as_str(),
			"keep-me",
			"移分组不应清空密码"
		);
	}

	/// 记住的分辨率要能一路走到连接参数里。
	///
	/// 这条钉住「记忆分辨率」的整条链：工具栏选的那档 → 落库的
	/// `host.display.resolution` → 下次建连请求的宽高。
	#[test]
	fn remembered_resolution_reaches_connection_params() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let store = Store::open_at(&AppPaths::from_base(dir.path())).expect("开库失败");

		let mut host = Host::draft(Protocol::Rdp);
		host.name = "分辨率回归".to_string();
		host.host = "10.0.0.2".to_string();
		host.username = Some("tester".to_string());
		host.display.resolution = Resolution::fixed(1600, 900).expect("合法尺寸");
		host.display.dpi = 200;
		let id = store
			.save_host(&host, &HostSecrets::with_password("p"))
			.expect("保存失败");
		host.id = Some(id);

		// 重新从库里读出来，模拟「重启后打开」
		let reloaded = store
			.hosts()
			.expect("列表失败")
			.into_iter()
			.find(|host| host.id == Some(id))
			.expect("主机应存在");
		let plan = build_rdp_plan(&store, &reloaded).expect("构造连接参数失败");
		let ConnectionPlan::Rdp(params) = plan else {
			panic!("RDP 主机应给出 RDP 计划");
		};
		assert_eq!(
			(params.width, params.height),
			(1600, 900),
			"应沿用记录里的分辨率"
		);

		// 「跟随窗口」则回落到初始尺寸，而不是沿用上一次的固定值
		host.display.resolution = Resolution::FitWindow;
		host.id = Some(id);
		store
			.save_host(&host, &HostSecrets::with_password("p"))
			.expect("保存失败");
		let reloaded = store
			.hosts()
			.expect("列表失败")
			.into_iter()
			.find(|host| host.id == Some(id))
			.expect("主机应存在");
		let ConnectionPlan::Rdp(params) =
			build_rdp_plan(&store, &reloaded).expect("构造连接参数失败")
		else {
			panic!("RDP 主机应给出 RDP 计划");
		};
		assert_eq!(
			(params.width, params.height),
			(FIT_WINDOW_START_WIDTH, FIT_WINDOW_START_HEIGHT)
		);
	}

	/// 恢复窗口尺寸时要把界面缩放乘回去。
	///
	/// 配置里存的是逻辑点，而建窗口要的是系统原生点；不乘，
	/// 开了 1.25 倍缩放的用户每启动一次窗口小一圈。
	#[test]
	fn restored_window_size_scales_with_ui() {
		let mut config = AppConfig {
			window: daisy_core::config::WindowConfig {
				width: 1200.0,
				height: 800.0,
			},
			..AppConfig::default()
		};
		assert_eq!(initial_window_size(&config), [1200.0, 800.0]);

		config.ui_scale = 1.25;
		assert_eq!(initial_window_size(&config), [1500.0, 1000.0]);

		// 手改坏的配置不能变成一个看不见的窗口
		config.window = daisy_core::config::WindowConfig {
			width: 0.0,
			height: 0.0,
		};
		assert_eq!(initial_window_size(&config), WINDOW_SIZE);
	}
}
