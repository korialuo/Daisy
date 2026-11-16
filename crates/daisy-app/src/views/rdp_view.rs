//! RDP 画面视图：把帧上传成纹理、绘制到面板，并把键鼠输入转发给会话。
//!
//! 与终端视图并列，同属标签页内容（见 `session_mgr` 的 `TabContent`）。
//!
//! 缩放目前只有**适应窗口**一种；分辨率切换与铺满 / 原始大小 / 自定义比例
//! 属于步骤 64b。但**鼠标坐标反变换**这一步已经做了——没有它，点击位置
//! 会随窗口大小整体偏移，输入根本没法用。

use std::time::Duration;

use daisy_core::model::{MAX_SCALE_FACTOR, MIN_SCALE_FACTOR, ScaleMode, ScaleSettings};
use daisy_rdp::{
	ConnectionParams, InputEvent, Key as RdpKey, PointerButton as RdpPointerButton, RgbaFrame,
	WHEEL_DELTA, WheelAxis,
};

use crate::design;
use crate::rdp_session::{RdpSession, RdpStatus};

/// 画面刷新时请求重绘的间隔。
///
/// 帧到达发生在 RDP 线程，UI 不主动重绘就收不到——按约 60fps 轮询。
const REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// 滚轮 `Point` 单位下，一格对应的逻辑像素。
///
/// 触控板给的是连续像素位移，没有"格"的概念，只能取个步进值近似。
const POINTS_PER_WHEEL_NOTCH: f32 = 50.0;

/// 滚轮 `Page` 单位下一步算几格（极少出现，近似即可）。
const NOTCHES_PER_WHEEL_PAGE: f32 = 3.0;

/// 鼠标按键的槽位数（左 / 中 / 右）。
const BUTTON_SLOTS: usize = 3;

/// 四档缩放的按钮。顺序即工具栏上的排列顺序。
const SCALE_MODES: [(ScaleMode, &str); 4] = [
	(ScaleMode::Fit, "适应窗口"),
	(ScaleMode::Fill, "铺满"),
	(ScaleMode::Actual, "原始大小"),
	(ScaleMode::Custom, "自定义"),
];

/// 「跟随窗口」的目标分辨率 = 可用区域的**逻辑点**尺寸。
///
/// 结果夹在 disp 允许的范围内，并**向下取偶数**——奇数宽高在某些
/// 服务端上会算出一半像素的偏移，画面整体错半列。
///
/// 不能乘 `pixels_per_point`：Retina 上那会得到一个两倍大的桌面，
/// 而远端（Windows Server）按 100% 渲染它——每个字只有十几物理像素，
/// 小到看不清，而且客户端的 DPI 缩放四条路（GCC / disp / 注册表 /
/// GUI）在这台机器上全部实测无效（2026-11-15）。
///
/// 按逻辑点请求后，远端 100% 渲染 1410×897，本机再按 2 倍设备像素
/// 铺满画布：文字视觉大小与本地界面一致（略软，但可读），这也是
/// 所有远程桌面客户端在 HiDPI 上的实际做法。
fn follow_target(avail: egui::Vec2) -> (u32, u32) {
	let clamp = |value: f32| {
		let px = value.round().max(0.0) as u32;
		px.clamp(RESOLUTION_MIN, RESOLUTION_MAX) & !1
	};
	(clamp(avail.x), clamp(avail.y))
}

/// 把「界面想要的目标尺寸」换算成**发给远端的尺寸**。
///
/// 「缩放」下拉是内容放大倍数：150% 表示「远端渲染出来的东西在本机显示时
/// 放大 1.5 倍」，因此远端桌面要**缩小**到 1/1.5，绘制层再放大铺满画布。
/// 远端（Windows Server）不兑现 disp/GCC 里的 DPI 字段，这是唯一能让字号
/// 真的变大的通道；100% 时原样返回，完全向后兼容。
fn scaled_size(size: (u32, u32), scale_percent: u32) -> (u32, u32) {
	let scale = scale_percent.max(100) as f32 / 100.0;
	let convert = |value: u32| {
		let px = (value as f32 / scale).round() as u32;
		px.clamp(RESOLUTION_MIN, RESOLUTION_MAX) & !1
	};
	(convert(size.0), convert(size.1))
}

/// [`scaled_size`] 的逆运算：把远端实际尺寸还原成「界面上的目标尺寸」。
///
/// 用在「用远端真实尺寸校准工具栏」时——缩放 150% 时远端只有 733×530，
/// 直接显示它会让分辨率下拉看起来跟用户选的 1100×796 对不上。
fn unscaled_size(size: (u32, u32), scale_percent: u32) -> (u32, u32) {
	let scale = scale_percent.max(100) as f32 / 100.0;
	let convert = |value: u32| {
		let px = (value as f32 * scale).round() as u32;
		px.clamp(RESOLUTION_MIN, RESOLUTION_MAX) & !1
	};
	(convert(size.0), convert(size.1))
}

/// 整张贴图的 UV 区间。
fn full_uv() -> egui::Rect {
	egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
}

/// 远端分辨率预设（S8-64）。顺序即下拉里的顺序。
const RESOLUTION_PRESETS: [((u32, u32), &str); 6] = [
	((1024, 768), "1024×768"),
	((1280, 720), "1280×720"),
	((1280, 800), "1280×800"),
	((1600, 900), "1600×900"),
	((1920, 1080), "1920×1080"),
	((2560, 1440), "2560×1440"),
];

/// 分辨率下拉弹层的高度：「跟随窗口 + 6 个预设 + 分隔线 + 自定义」共 8 行，
/// 一屏放全，末行不该被裁进内部滚动区（半行最难点中）。
/// 与设置页字体下拉同理，高度只走 [`egui::ComboBox::height`]——
/// 在 `show_ui` 里 `set_max_height` 会和它的滚动区打架。留了余量，
/// 内容不足时只是底部多一小段空白。
const RESOLUTION_MENU_HEIGHT: f32 = 296.0;

/// 分辨率防抖窗口。
///
/// 拖动窗口时可用区域每帧都在变，跟着每帧发请求会把 disp 通道灌满，
/// 服务端来不及重建显示上下文——现象是画面反复黑一下、最后尺寸不对。
/// 攒够这段安静时间再发。
const RESIZE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// disp 通道允许的尺寸范围（`DISPLAY_CONTROL_MIN/MAX_MONITOR_WIDTH/HEIGHT`）。
const RESOLUTION_MIN: u32 = 200;
const RESOLUTION_MAX: u32 = 8192;

/// 「缩放」下拉（内容放大倍数）的档位。与 Windows「显示设置 → 缩放」一致
/// （100–500，25 的倍数里去掉不常用的 275/325/375/425/475）。
const DPI_OPTIONS: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

/// 远端分辨率控制状态（S8-64）。
#[derive(Debug, Clone, PartialEq)]
struct Resolution {
	/// 当前选中的预设；`None` = 自定义。
	preset: Option<(u32, u32)>,
	/// 自定义宽高（数字框里的值）。
	custom: (u32, u32),
	/// 跟随窗口：按可用区域自动算，优先于 `preset`。
	follow: bool,
	/// 待发送的目标与起始时刻（防抖）。
	pending: Option<((u32, u32), std::time::Instant)>,
	/// 上一次真正发出去的目标——避免拖动时把同一个尺寸重发。
	last_sent: Option<(u32, u32)>,
	/// disp 通道是否就绪（收到 `RdpStatus::DisplayReady` 才置位）。
	ready: bool,
	/// 最近一次失败的原因，显示在工具栏上。
	error: Option<String>,
	/// 连接时请求的尺寸。用来判断「用户还没改过」——没改过才允许
	/// 用远端真实尺寸校准工具栏。
	connection_size: (u32, u32),
}

impl Resolution {
	fn new(initial: (u32, u32)) -> Self {
		Self {
			// 初始尺寸通常就是连接的 1024×768，尽量落在预设上
			preset: RESOLUTION_PRESETS
				.iter()
				.map(|(size, _)| *size)
				.find(|size| *size == initial),
			custom: initial,
			follow: false,
			pending: None,
			last_sent: Some(initial),
			ready: false,
			error: None,
			connection_size: initial,
		}
	}

	/// 用远端的真实尺寸校准工具栏（只在用户没改过分辨率时调用）。
	fn sync_to(&mut self, size: (u32, u32)) {
		self.preset = RESOLUTION_PRESETS
			.iter()
			.map(|(preset, _)| *preset)
			.find(|preset| *preset == size);
		self.custom = size;
		self.last_sent = Some(size);
	}

	/// 按主机记录里的设置初始化工具栏状态。
	///
	/// 记录里只存「跟随窗口 / 固定尺寸」，预设名要从尺寸**反查**——
	/// 不反查的话，重开一台设成 1600×900 的机器，下拉里显示的是
	/// 「1600×900」的裸数字（自定义档），跟用户上次点的预设名对不上。
	fn from_settings(settings: daisy_core::model::Resolution, initial: (u32, u32)) -> Self {
		let mut state = Self::new(initial);
		match settings {
			daisy_core::model::Resolution::FitWindow
			| daisy_core::model::Resolution::FitWindowAt { .. } => {
				state.follow = true;
				state.preset = None;
			},
			daisy_core::model::Resolution::Fixed { width, height } => {
				state.follow = false;
				state.custom = (width, height);
				state.preset = RESOLUTION_PRESETS
					.iter()
					.map(|(size, _)| *size)
					.find(|size| *size == (width, height));
			},
		}
		state
	}

	/// 当前选择对应的可落库形式。
	///
	/// 越界时退回默认（跟随窗口）：数字框的范围已夹在 disp 允许区间内，
	/// 走到越界只可能是将来放宽了范围而这里没跟上。
	fn settings(&self) -> daisy_core::model::Resolution {
		if self.follow {
			// 把最近一次实际目标一起记下：下次**建连**直接用它，
			// 首帧就是正确分辨率，不再有「1280×800 → disp 收口」的跳变
			return match self.last_sent {
				Some((width, height)) => {
					daisy_core::model::Resolution::FitWindowAt { width, height }
				},
				None => daisy_core::model::Resolution::FitWindow,
			};
		}
		let (width, height) = self.preset.unwrap_or(self.custom);
		daisy_core::model::Resolution::fixed(width, height).unwrap_or_default()
	}

	/// 记下一个待发目标（防抖起点刷新）。
	///
	/// 同一个尺寸重复点不算改动——否则每帧都会把防抖计时重置，
	/// 永远发不出去。
	fn want(&mut self, size: (u32, u32)) {
		if self.last_sent == Some(size) {
			return;
		}
		if self.pending.is_some_and(|(pending, _)| pending == size) {
			return;
		}
		self.pending = Some((size, std::time::Instant::now()));
	}
}

/// 画面超出可用区域时的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overflow {
	/// 完整放得下，居中显示。
	Fits,
	/// 比可用区域大，但**这是故意的**（铺满）——居中后由面板裁掉四周。
	Crop,
	/// 比可用区域大且不该裁——交给滚动条，否则看不全。
	Scroll,
}

/// 缩放模式算出的画面几何。
#[derive(Debug, Clone, Copy, PartialEq)]
struct ScaleGeometry {
	/// 画面在界面里占的尺寸（逻辑点）。
	size: egui::Vec2,
	/// 超出可用区域时怎么办。
	overflow: Overflow,
}

/// 按缩放模式算画面该画多大。
///
/// **两种比例的基准不同，这是刻意的**：
///
/// - 适应窗口 / 铺满：相对**可用区域**（逻辑点）——用户想的是「填满窗口」；
/// - 原始大小 / 自定义：相对**设备像素**（要除以 `pixels_per_point`）——
///   「原始大小」的定义就是远端 1 像素 = 屏幕 1 像素，在 Retina 上不除掉倍率
///   就会被放大两倍、发糊。
///
/// 两种基准在 `pixels_per_point == 1` 时完全一致，所以普通屏上看不出差别。
fn scale_geometry(
	mode: ScaleMode,
	factor: f32,
	frame: egui::Vec2,
	avail: egui::Vec2,
	pixels_per_point: f32,
) -> ScaleGeometry {
	// 尺寸为 0 时除法会得到 inf/NaN。正常路径上 `frame` 已被上层挡掉，
	// 这里再兜一次，让这个纯函数对任意输入都有定义。
	if frame.x <= 0.0 || frame.y <= 0.0 || avail.x <= 0.0 || avail.y <= 0.0 {
		return ScaleGeometry {
			size: egui::Vec2::ZERO,
			overflow: Overflow::Fits,
		};
	}
	// 倍率为 0 会把尺寸算成无穷大，兜个底
	let ppp = if pixels_per_point > 0.0 {
		pixels_per_point
	} else {
		1.0
	};

	let scale = match mode {
		ScaleMode::Fit => (avail.x / frame.x).min(avail.y / frame.y),
		ScaleMode::Fill => (avail.x / frame.x).max(avail.y / frame.y),
		ScaleMode::Actual => 1.0 / ppp,
		ScaleMode::Custom => factor / ppp,
	};
	let size = frame * scale;
	let too_big = size.x > avail.x || size.y > avail.y;

	let overflow = match mode {
		// 铺满就是故意画大再裁，不给滚动条
		ScaleMode::Fill if too_big => Overflow::Crop,
		// 原始大小 / 自定义画大了应该能看全，而不是被裁掉一角
		ScaleMode::Actual | ScaleMode::Custom if too_big => Overflow::Scroll,
		_ => Overflow::Fits,
	};

	ScaleGeometry { size, overflow }
}

/// 无人值守验证用的输入脚本环境变量。
///
/// 合成系统级的键鼠事件需要辅助功能权限，脚本化点击被系统拦下，所以
/// 「应用自己的输入通道能不能把按键送到远端」没法靠外部工具验。这个钩子
/// 让会话出图后自动注入一组**结果可辨认**的输入，截图即可自证。
///
/// 只在自动化验证时设置。取值是一段用 `;` 分隔的脚本，每步是：
///
/// - `win+r` / `ctrl+a` / `enter` —— 组合键（`+` 连接，前面的是按住不放的修饰键）
/// - `type:xxx` —— 按 US 布局敲一串可打印 ASCII（大写与符号自动按 Shift）
/// - `wait:800` —— 停 800 毫秒（等远端弹出窗口、把焦点移过去）
/// - `move:640,400` —— 把指针移到桌面坐标
/// - `click:640,400` —— 在桌面坐标上点一下左键
/// - `wheel:640,400,-5` —— 在桌面坐标上滚 5 格（负数向下/向左）
///
/// 例：`win+r; wait:900; type:shutdown /l /t 0; wait:300; enter`
const SMOKE_INPUT_ENV: &str = "DAISY_SMOKE_RDP_KEY";

/// 输入脚本里相邻两步之间的间隔。
///
/// 不能零间隔连发：远端还需要时间把上一步的结果画出来（弹出「运行」对话框、
/// 把键盘焦点移进输入框）。400ms 是「够慢、又不至于让脚本跑很久」的经验值，
/// 个别更慢的步骤用 `wait:` 自己补。
const SMOKE_STEP_GAP: Duration = Duration::from_millis(400);

/// 无人值守验证用的缩放档位环境变量。
///
/// 四档渲染要能逐个截图核对，而点按钮做不到（脚本化点击被系统权限拦）。
/// 取值：`fit` / `fill` / `actual` / `custom`，覆盖主机记录里的设置。
const SMOKE_SCALE_ENV: &str = "DAISY_SMOKE_RDP_SCALE";

/// 无人值守验证用的 DPI 缩放环境变量。
///
/// DPI 改动必须等 disp 通道就绪后随下一次分辨率请求下发，脚本化点击被
/// 系统权限拦，留这个钩子走与用户点击完全相同的路径（覆盖主机记录里的值）。
/// 取值：`200` 这种百分比；非法值忽略。
const SMOKE_DPI_ENV: &str = "DAISY_SMOKE_RDP_DPI";

/// 无人值守验证用的分辨率环境变量。
///
/// 分辨率下拉是点击操作，脚本化点击被系统权限拦，所以留一个钩子：
/// 出图后自动走一遍**与用户点击完全相同**的路径
/// （`want` → 防抖 → `session.resize` → disp），截图即可核对远端有没有跟着变。
///
/// 取值：`1280x800` 这种形式；`follow` 表示切到「跟随窗口」。
const SMOKE_RESOLUTION_ENV: &str = "DAISY_SMOKE_RDP_RESOLUTION";

/// 无人值守验证：断开时直接跳到「已确认」的画面。
///
/// 告知框要人点「确定」，而脚本化点击被系统权限拦——没有这个开关，
/// 「点完之后那张空白页 + 重新连接按钮」只能靠碰运气截图。
/// 取值随意，存在即生效。
const SMOKE_ACK_ENV: &str = "DAISY_SMOKE_RDP_ACK";

/// 无人值守验证：把一组本地文件挂上**系统剪贴板**（S10）。
///
/// Finder 复制文件这一步无法脚本化（合成点击要辅助功能授权），
/// 留这个钩子走与 Finder 完全相同的路径：`pasteboard_files::put_files`
/// 写入 → 轮询读到 → 上报远端。取值：逗号分隔的文件路径。
/// 首帧后 [`SMOKE_CLIP_FILES_DELAY`] 触发。
const SMOKE_CLIP_FILES_ENV: &str = "DAISY_SMOKE_CLIP_FILES";

/// [`SMOKE_CLIP_FILES_ENV`] 的触发延时。
const SMOKE_CLIP_FILES_DELAY: Duration = Duration::from_secs(6);

/// 无人值守验证：模拟「用户在 Daisy 窗口里按 Cmd+V」。
///
/// 真实按键无法脚本化（合成事件要辅助功能授权），只能走应用自己的钩子。
/// 它验的是 [`RdpView::paste_clipboard_to_remote`] 这条路径——**剪贴板里是
/// 文件时不能退化成文本**（真机回归过：只报文本会把远端的文件列表覆盖掉）。
/// 值班：`1` 表示首帧后 [`SMOKE_PASTE_DELAY`] 触发一次。
const SMOKE_PASTE_ENV: &str = "DAISY_SMOKE_RDP_PASTE";

/// [`SMOKE_PASTE_ENV`] 的触发延时（要晚于文件挂上剪贴板的时刻）。
const SMOKE_PASTE_DELAY: Duration = Duration::from_secs(9);

/// 无人值守验证：按 [`SMOKE_SCALE_ENV`] 指定初始缩放档。
fn smoke_dpi() -> Option<u32> {
	let raw = std::env::var(SMOKE_DPI_ENV).ok()?;
	let dpi: u32 = raw.trim().parse().ok()?;
	if daisy_core::model::validate_dpi(dpi).is_err() {
		tracing::warn!("{SMOKE_DPI_ENV} 的取值「{raw}」不在 100–500（25 的倍数），已忽略");
		return None;
	}
	tracing::info!("无人值守 DPI 钩子：{dpi}%");
	Some(dpi)
}

fn smoke_scale() -> Option<ScaleSettings> {
	let raw = std::env::var(SMOKE_SCALE_ENV).ok()?;
	let mode = match raw.trim() {
		"fit" => ScaleMode::Fit,
		"fill" => ScaleMode::Fill,
		"actual" => ScaleMode::Actual,
		"custom" => ScaleMode::Custom,
		other => {
			tracing::warn!("{SMOKE_SCALE_ENV} 的取值「{other}」不认识，已忽略");
			return None;
		},
	};
	tracing::info!("无人值守缩放钩子：初始档位 {mode:?}");
	Some(ScaleSettings {
		mode,
		// 自定义档给一个能一眼看出比例差异的值
		factor: 1.5,
	})
}

/// 首帧之后等多久才注入。
///
/// **不能一拿到首帧就发**：首帧常在远端还在建立会话时就到了，那一刻的按键
/// 会被丢掉——实测连上 0.15 秒就发 `Win+R`，远端毫无反应。真实用户总是看到
/// 桌面之后才动手，所以这里也照着"等人看完再按"的节奏来。
const SMOKE_INPUT_DELAY: Duration = Duration::from_secs(3);

/// 视图当前状态。
enum ViewState {
	/// 正在连接（或已连上但还没收到第一帧）。
	Connecting,
	/// 有画面。
	Live,
	/// 已断开。
	///
	/// 分两段是刻意的：先弹框把话说清楚（用户当时多半在做别的事，
	/// 画面自己变空白等于什么都没说）；点了「确定」画面才清空，
	/// 只留一个「重新连接」入口。
	Ended {
		/// 断开原因：连接失败、服务端关闭、网络中断……
		reason: String,
		/// 告知框是否已经被确认过。
		acknowledged: bool,
		/// 这条会话**曾经出过画面**吗。
		///
		/// 用来区分「连上之后断了」和「压根没连上」——两者都说「已断开」
		/// 会让人以为刚才那条会话是被踢掉的，而实际是地址/凭据/网络的问题。
		ever_live: bool,
	},
}

/// 视图向应用层报告的动作。
///
/// 视图只负责「画 + 收集动作」，写库交给应用层——只有它手里有 `Store`
/// 与凭据（保存主机是整列覆盖，见 `main.rs::existing_secrets`）。
#[derive(Debug, Clone, PartialEq)]
pub enum RdpAction {
	/// 用户改了缩放设置。`host_id` 为 `None` 表示没有对应的主机记录，不落库。
	ScaleChanged {
		/// 来源主机主键。
		host_id: Option<i64>,
		/// 新的缩放设置。
		scale: ScaleSettings,
	},
	/// 用户改了远端分辨率，需要写回主机记录，「下次打开还是这个尺寸」。
	///
	/// 只改会话而不落库的话，关掉标签页再连就退回默认尺寸——
	/// 用户每次都要重挑一遍（这正是「记忆分辨率」要解决的问题）。
	ResolutionChanged {
		/// 来源主机主键。
		host_id: Option<i64>,
		/// 新的分辨率来源（跟随窗口 / 固定尺寸）。
		resolution: daisy_core::model::Resolution,
	},
	/// 用户改了远端 DPI 缩放，同样要写回主机记录。
	///
	/// Retina 上「跟随窗口」把设备像素当分辨率下发，不配缩放的话远端
	/// 按 100% 渲染超大桌面、字小到看不清——这个下拉就是给用户的旋钮。
	DpiChanged {
		/// 来源主机主键。
		host_id: Option<i64>,
		/// 新的 DPI 缩放百分比（100–500）。
		dpi: u32,
	},
	/// 剪贴板操作的结果，需要给用户一个可见反馈（S8-63）。
	///
	/// **剪贴板静默失败最难查**：按了 Cmd+C 什么都没发生，
	/// 用户只会以为键盘坏了。所以成功与失败都要说一声。
	Notice {
		/// 要显示的文案。
		text: String,
		/// 是否是错误（错误用警告色）。
		is_error: bool,
	},
	/// 首次见到这台机器的证书，指纹需要记入库（S8-65）。
	///
	/// 视图不碰数据库（见 `rdp_session` 的说明），只把事实交给应用层。
	TrustCertificate {
		/// 主机地址。
		host: String,
		/// 端口。
		port: u16,
		/// 规范化指纹。
		fingerprint: String,
		/// 证书使用者。
		subject: Option<String>,
		/// 签发者。
		issuer: Option<String>,
	},
}

/// 一条 RDP 会话的视图。
pub struct RdpView {
	/// 标签标题。
	title: String,
	/// 会话（后台线程 + 通道）。
	session: RdpSession,
	/// 连接参数。**必须留一份**：`RdpSession::spawn` 会把它吃掉，
	/// 用户点「重新连接」时没有第二份可用了。
	params: ConnectionParams,
	/// 显示状态。
	state: ViewState,
	/// 已上传的纹理。
	texture: Option<egui::TextureHandle>,
	/// 纹理对应的帧序号，用来跳过重复上传。
	uploaded_seq: u64,
	/// 服务端下发的 Windows 光标（已上传成纹理）。
	cursor_texture: Option<egui::TextureHandle>,
	/// 已上传光标的序号，用来跳过重复上传。
	cursor_seq: u64,
	/// 光标位图的像素尺寸（远端桌面像素）。
	cursor_px: [u16; 2],
	/// 光标热点：位图里哪个像素对应指针位置。
	cursor_hotspot: [u16; 2],
	/// 当前画面的像素尺寸（绘制时按它做等比缩放）。
	frame_size: [u32; 2],
	/// 上一次发给远端的桌面坐标。
	///
	/// 指针每帧都在动，但换算到桌面坐标后常常是同一个像素——重复发没有
	/// 意义，还会把输入队列灌满。
	last_pointer: Option<(u16, u16)>,
	/// 滚轮不足一格的余量（触控板是连续位移，见 [`drain_notches`]）。
	wheel_accum: egui::Vec2,
	/// 已按下、尚未抬起的远端鼠标按键。
	///
	/// 追踪它是为了防「按键卡住」：拖动时按下落在画面内、抬起可能已经在
	/// 画面外，只按坐标过滤就会漏掉抬起事件，远端于是永远处于按住状态——
	/// 用户只能重连。
	remote_buttons: [bool; BUTTON_SLOTS],
	/// 本地缩放设置（不改远端分辨率）。
	scale: ScaleSettings,
	/// 来源主机。`None` 表示没有对应记录，改缩放不落库。
	host_id: Option<i64>,
	/// 库里记录的这台机器的证书指纹（S8-65）。`None` 表示首连。
	///
	/// 收到首连上报后会更新成本次的指纹——同一标签页内重连就不会重复提示。
	expected_cert: Option<String>,
	/// 待上报给应用层的动作。
	pending: Option<RdpAction>,
	/// 最近一次**已同步**的剪贴板文本（S8-63）。
	///
	/// 双向同步靠它防回环：远端推来的写进本地剪贴板后，本地轮询会又
	/// 读到同一段——不记住就会再推回去，两边乒乓。
	last_synced: String,
	/// 最近一次**已上报**给远端的那批文件的 `changeCount`（S10，防重复上报）。
	///
	/// 用序号而不是路径比对：用户对同一批文件再按一次 Cmd+C 会拿到新的
	/// 序号，理应重新上报（上一次远端可能没收到）。
	last_files_change_count: Option<i64>,
	/// 上一次**上报文件格式列表**的时刻。
	///
	/// 粘贴时要据此判断「要不要再报一遍」：**重复上报同一批文件会把
	/// Windows 的粘贴流程搞坏**——真机对照实测，粘贴时重报一次，
	/// rdpclip 就变成反复索要描述表、一次不问大小，文件永远不落盘；
	/// 不重报（只注入 Ctrl+V）则正常问大小并取内容。
	last_files_announced_at: Option<std::time::Instant>,
	/// 我们**刚写进**本地剪贴板的那批文件的 `changeCount`。
	///
	/// 远端文件下载完会挂上本地剪贴板（Finder 里 Cmd+V 就能粘），
	/// 这次写入自己也会让轮询看到一份「新的」文件列表——
	/// 用序号认出是自己写的，不往远端回推，否则乒乓。
	pushed_file_change_count: Option<i64>,
	/// 上次轮询本地剪贴板的时刻（系统没有变更事件，只能定时看）。
	last_clip_poll: std::time::Instant,
	/// 是否已提示过「远端 → 本地不可用」（每条会话只提示一次）。
	warned_no_remote_clip: bool,
	/// 这一帧是否画了远端光标（画了就必须把本机光标藏掉）。
	///
	/// 放在字段里而不是就地调用，是因为本帧后面还有工具栏/面板会再调
	/// `set_cursor_icon`——本机箭头会顶回来，和远端光标同时出现。
	hide_local_cursor: bool,
	/// 用户是否按过复制快捷键（用来判断该不该提示）。
	///
	/// **不能无条件提示**：得等用户真的在远端复制过才说，
	/// 否则又会变成第 5 轮那种「必然误报」的提示。
	user_tried_remote_copy: bool,
	/// 最近一次「按复制快捷键」的时刻。
	///
	/// **必须留出等待时间**：广播是服务端主动发的，从按键到数据落地
	/// 快则几十毫秒、慢则数百毫秒。原来的判定是「按下即检查」，
	/// 于是每次按 Cmd+C 都会立刻弹一条红色「服务端未广播剪贴板变化」
	/// ——功能正常也像坏了，而且 `warned_no_remote_clip` 只提示一次，
	/// 真出问题时反而没提示了。
	copy_attempt_at: Option<std::time::Instant>,
	/// 无人值守验证的输入脚本（见 [`SMOKE_INPUT_ENV`]）。解析一次，之后按下标推进。
	smoke_steps: Option<Vec<Step>>,
	/// 脚本执行到第几步。
	smoke_cursor: usize,
	/// 下一步最早什么时候可以发；`None` 表示立即可发。
	smoke_next_at: Option<std::time::Instant>,
	/// 无人值守验证的分辨率钩子是否已经触发（见 [`SMOKE_RESOLUTION_ENV`]）。
	smoke_resolution_sent: bool,
	/// 首帧到达的时刻，用来给无人值守注入计时（见 [`SMOKE_INPUT_DELAY`]）。
	first_frame_at: Option<std::time::Instant>,
	/// 无人值守验证的文件剪贴板钩子（见 [`SMOKE_CLIP_FILES_ENV`]）。
	///
	/// `(要挂上剪贴板的文件, 触发时刻)`；触发时刻消费后置 `None`。
	smoke_clip_files: Option<(Vec<std::path::PathBuf>, Option<std::time::Instant>)>,
	/// 无人值守验证的「按 Cmd+V」钩子（见 [`SMOKE_PASTE_ENV`]）。
	smoke_paste_at: Option<std::time::Instant>,
	/// 远端分辨率控制（S8-64）。
	resolution: Resolution,
	/// 已经上报给应用层（从而已落库）的分辨率选择。
	///
	/// 用它做「有没有新变化」的判据：和上一帧比会在 `pending` 被占用时丢改动，
	/// 和这个值比则会在下一次有空槽时补报。
	reported_resolution: daisy_core::model::Resolution,
	/// 内容放大倍数（百分比）。默认 100% = 原样。
	///
	/// 在**本机**兑现：远端按 1/倍数 渲染，绘制层放大铺满画布
	/// （远端 Windows Server 不兑现 disp/GCC 的 DPI 字段，见
	/// `scaled_size` 的说明）。
	dpi: u32,
	/// 上次已落库的 DPI 值——变化检测的基准（与 `reported_resolution` 同理）。
	reported_dpi: u32,
	/// 上一帧画面可用区域的逻辑尺寸——「跟随窗口」按它算目标分辨率。
	///
	/// 存上一帧是因为工具栏画在画面之前，那一帧还量不到这次的高度。
	/// 差一帧对防抖窗口（300ms）没有影响。
	avail: egui::Vec2,
	/// 当前窗口的像素密度（Retina 上 2.0）。
	///
	/// 用来算下发给远端的 **DPI 缩放**（`DesktopScaleFactor`）：
	/// 分辨率按设备像素给，缩放按这个值给——只给分辨率不给缩放，
	/// 远端就会按 100% 渲染一个超大桌面，字小到看不清。
	///
	/// 每帧在 [`Self::poll`] 开头刷新：`flush_resolution` 手里没有 `ctx`。
	pixels_per_point: f32,
}

/// 解析 `"x,y"`，任一不是 0..=65535 的整数就返回 `None`。
fn parse_point(spec: &str) -> Option<(u16, u16)> {
	let (x, y) = spec.split_once(',')?;
	Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// 无人值守输入脚本的一步。写法见 [`SMOKE_INPUT_ENV`]。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
	/// 把指针移到桌面坐标。
	Move(u16, u16),
	/// 在桌面坐标上点一下左键。
	Click(u16, u16),
	/// 按住左键从 (x1,y1) 拖到 (x2,y2)，中间插 `steps` 次移动。
	///
	/// 拖动是**刷新最密**的操作：服务端会连发多个 UPDATE PDU，
	/// 快照落在批次中间就会看到「窗口已到新位置、旧位置还有残影」。
	/// 有了这一步，撕裂能无人值守复现，不必靠人眼描述。
	Drag(u16, u16, u16, u16, u16),
	/// 在桌面坐标上滚若干格（正数向上/向右）。
	Wheel(u16, u16, i16),
	/// 按 US 布局敲一串可打印 ASCII。
	Type(String),
	/// 停一会儿再继续。
	Wait(Duration),
	/// 组合键：除最后一个以外的都按住不放，最后一个敲一下。
	Chord(Vec<RdpKey>),
}

/// 把脚本串解析成步骤。`;` 分隔，空步忽略。
fn parse_script(spec: &str) -> Result<Vec<Step>, String> {
	let mut steps = Vec::new();
	for raw in spec.split(';') {
		let step = raw.trim();
		if step.is_empty() {
			continue;
		}
		if let Some(rest) = step.strip_prefix("move:") {
			let point = parse_point(rest).ok_or_else(|| format!("坐标「{rest}」不是 x,y"))?;
			steps.push(Step::Move(point.0, point.1));
		} else if let Some(rest) = step.strip_prefix("wheel:") {
			// x,y,格数 —— 三个数字，分开解析才报得清是哪一个不对
			let parts: Vec<&str> = rest.split(',').collect();
			let bad = || format!("滚轮参数「{rest}」不是 x,y,格数");
			if parts.len() != 3 {
				return Err(bad());
			}
			let x: u16 = parts[0].trim().parse().map_err(|_| bad())?;
			let y: u16 = parts[1].trim().parse().map_err(|_| bad())?;
			let notches: i16 = parts[2].trim().parse().map_err(|_| bad())?;
			steps.push(Step::Wheel(x, y, notches));
		} else if let Some(rest) = step.strip_prefix("click:") {
			let point = parse_point(rest).ok_or_else(|| format!("坐标「{rest}」不是 x,y"))?;
			steps.push(Step::Click(point.0, point.1));
		} else if let Some(rest) = step.strip_prefix("drag:") {
			// x1,y1,x2,y2[,步数]
			let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
			let bad = || format!("拖动参数「{rest}」应为 x1,y1,x2,y2[,步数]");
			if parts.len() != 4 && parts.len() != 5 {
				return Err(bad());
			}
			let n = |i: usize| -> Result<u16, String> {
				parts[i].parse::<u16>().map_err(|_| bad())
			};
			let steps_count = if parts.len() == 5 {
				parts[4].parse::<u16>().map_err(|_| bad())?
			} else {
				12
			};
			steps.push(Step::Drag(
				n(0)?,
				n(1)?,
				n(2)?,
				n(3)?,
				steps_count.max(1),
			));
		} else if let Some(text) = step.strip_prefix("type:") {
			// 只保证 US 布局的 ASCII：非 ASCII 要么发不出去，要么在远端变成别的字，
			// 与其静默出错不如在解析阶段就拦下
			if let Some(bad) = text.chars().find(|c| char_key(*c).is_none()) {
				return Err(format!("字符「{bad}」不在可敲范围内（仅 US 布局 ASCII）"));
			}
			steps.push(Step::Type(text.to_owned()));
		} else if let Some(ms) = step.strip_prefix("wait:") {
			let ms: u64 = ms
				.trim()
				.parse()
				.map_err(|_| format!("毫秒「{ms}」不是整数"))?;
			steps.push(Step::Wait(Duration::from_millis(ms)));
		} else {
			let mut keys = Vec::new();
			for name in step.split('+') {
				let key = key_by_name(name).ok_or_else(|| format!("不认识的键名「{name}」"))?;
				keys.push(key);
			}
			steps.push(Step::Chord(keys));
		}
	}
	if steps.is_empty() {
		return Err("脚本是空的".to_owned());
	}
	Ok(steps)
}

/// 键名（或单个字符）→ 按键。名字大小写不敏感。
fn key_by_name(name: &str) -> Option<RdpKey> {
	let name = name.trim().to_ascii_lowercase();
	Some(match name.as_str() {
		"win" | "super" => RdpKey::SuperLeft,
		"ctrl" | "control" => RdpKey::ControlLeft,
		"shift" => RdpKey::ShiftLeft,
		"alt" => RdpKey::AltLeft,
		"enter" | "return" => RdpKey::Enter,
		"tab" => RdpKey::Tab,
		"esc" | "escape" => RdpKey::Escape,
		"space" => RdpKey::Space,
		"backspace" => RdpKey::Backspace,
		"delete" | "del" => RdpKey::Delete,
		"up" => RdpKey::ArrowUp,
		"down" => RdpKey::ArrowDown,
		"left" => RdpKey::ArrowLeft,
		"right" => RdpKey::ArrowRight,
		"home" => RdpKey::Home,
		"end" => RdpKey::End,
		"pageup" => RdpKey::PageUp,
		"pagedown" => RdpKey::PageDown,
		// 单个字符走字符表（`a`、`7`、`/`）
		other => {
			let mut chars = other.chars();
			let first = chars.next()?;
			if chars.next().is_some() {
				return None;
			}
			char_key(first)?.0
		},
	})
}

/// 字母 → 按键。用表而不是 26 个分支：加变体时编译器会提醒补齐。
const LETTER_KEYS: [RdpKey; 26] = [
	RdpKey::A,
	RdpKey::B,
	RdpKey::C,
	RdpKey::D,
	RdpKey::E,
	RdpKey::F,
	RdpKey::G,
	RdpKey::H,
	RdpKey::I,
	RdpKey::J,
	RdpKey::K,
	RdpKey::L,
	RdpKey::M,
	RdpKey::N,
	RdpKey::O,
	RdpKey::P,
	RdpKey::Q,
	RdpKey::R,
	RdpKey::S,
	RdpKey::T,
	RdpKey::U,
	RdpKey::V,
	RdpKey::W,
	RdpKey::X,
	RdpKey::Y,
	RdpKey::Z,
];

/// `'1'..'9'` 的按键（`'0'` 在键盘上排在 9 后面，单独处理）。
const DIGIT_KEYS: [RdpKey; 9] = [
	RdpKey::Digit1,
	RdpKey::Digit2,
	RdpKey::Digit3,
	RdpKey::Digit4,
	RdpKey::Digit5,
	RdpKey::Digit6,
	RdpKey::Digit7,
	RdpKey::Digit8,
	RdpKey::Digit9,
];

/// 一个可打印 ASCII 字符 → (按键, 要不要按住 Shift)。
///
/// 按 **US 布局**算：远端 Windows 若用别的布局，符号会错位，字母数字不受影响。
fn char_key(c: char) -> Option<(RdpKey, bool)> {
	let plain = match c {
		'a'..='z' => LETTER_KEYS[(c as u8 - b'a') as usize],
		'A'..='Z' => return Some((LETTER_KEYS[(c as u8 - b'A') as usize], true)),
		'1'..='9' => DIGIT_KEYS[(c as u8 - b'1') as usize],
		'0' => RdpKey::Digit0,
		' ' => RdpKey::Space,
		'-' => RdpKey::Minus,
		'=' => RdpKey::Equals,
		'[' => RdpKey::OpenBracket,
		']' => RdpKey::CloseBracket,
		'\\' => RdpKey::Backslash,
		';' => RdpKey::Semicolon,
		'\'' => RdpKey::Quote,
		',' => RdpKey::Comma,
		'.' => RdpKey::Period,
		'/' => RdpKey::Slash,
		'`' => RdpKey::Backtick,
		// 以下是同一颗键 + Shift 的上档字符
		'!' => return Some((RdpKey::Digit1, true)),
		'@' => return Some((RdpKey::Digit2, true)),
		'#' => return Some((RdpKey::Digit3, true)),
		'$' => return Some((RdpKey::Digit4, true)),
		'%' => return Some((RdpKey::Digit5, true)),
		'^' => return Some((RdpKey::Digit6, true)),
		'&' => return Some((RdpKey::Digit7, true)),
		'*' => return Some((RdpKey::Digit8, true)),
		'(' => return Some((RdpKey::Digit9, true)),
		')' => return Some((RdpKey::Digit0, true)),
		'_' => return Some((RdpKey::Minus, true)),
		'+' => return Some((RdpKey::Equals, true)),
		'{' => return Some((RdpKey::OpenBracket, true)),
		'}' => return Some((RdpKey::CloseBracket, true)),
		'|' => return Some((RdpKey::Backslash, true)),
		':' => return Some((RdpKey::Semicolon, true)),
		'"' => return Some((RdpKey::Quote, true)),
		'<' => return Some((RdpKey::Comma, true)),
		'>' => return Some((RdpKey::Period, true)),
		'?' => return Some((RdpKey::Slash, true)),
		'~' => return Some((RdpKey::Backtick, true)),
		_ => return None,
	};
	Some((plain, false))
}

/// 一条按键事件。
fn key_event(key: RdpKey, down: bool) -> InputEvent {
	InputEvent::Key {
		key,
		down,
		repeat: false,
	}
}

/// 敲一串文本的事件序列。
fn type_events(text: &str) -> Vec<InputEvent> {
	let mut events = Vec::new();
	for c in text.chars() {
		let Some((key, shift)) = char_key(c) else {
			continue;
		};
		if shift {
			events.push(key_event(RdpKey::ShiftLeft, true));
		}
		events.push(key_event(key, true));
		events.push(key_event(key, false));
		if shift {
			events.push(key_event(RdpKey::ShiftLeft, false));
		}
	}
	events
}

/// 组合键的事件序列：修饰键按下 → 主键按下抬起 → 修饰键**逆序**抬起。
fn chord_events(keys: &[RdpKey]) -> Vec<InputEvent> {
	let Some((&last, modifiers)) = keys.split_last() else {
		return Vec::new();
	};
	let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
	for &key in modifiers {
		events.push(key_event(key, true));
	}
	events.push(key_event(last, true));
	events.push(key_event(last, false));
	for &key in modifiers.iter().rev() {
		events.push(key_event(key, false));
	}
	events
}

impl RdpView {
	/// 起一条会话并返回视图。连接在后台进行，界面先显示「连接中」。
	///
	/// `host_id` 与 `scale` 来自主机记录：缩放是**每台主机各自的偏好**，
	/// 换一台机器就该按那台的设置显示，而不是沿用上一条会话的。
	///
	/// `settings` 同理——它决定工具栏初始显示「跟随窗口」还是某个预设，
	/// 也是「上次用的分辨率」得以延续的地方。
	pub fn new(
		title: String,
		params: ConnectionParams,
		host_id: Option<i64>,
		display: daisy_core::model::DisplaySettings,
		expected_cert: Option<String>,
	) -> Self {
		// 先把尺寸取出来——`params` 马上要交给会话线程（留一份给重连）
		let scale = smoke_scale().unwrap_or(display.scale);
		let dpi = smoke_dpi().unwrap_or(display.dpi);
		let resolution = display.resolution;
		// 建连尺寸同样按倍数缩小：否则首帧是「100% 大小」的桌面，
		// disp 收口后才变成本意尺寸——用户会看到画面先小后大跳一次。
		let start = scaled_size((params.width, params.height), dpi);
		let mut params = params;
		params.width = start.0;
		params.height = start.1;
		let frame_size = [params.width, params.height];
		Self {
			title,
			session: RdpSession::spawn(params.clone(), expected_cert.clone()),
			params,
			expected_cert,
			state: ViewState::Connecting,
			texture: None,
			uploaded_seq: 0,
			cursor_texture: None,
			cursor_seq: 0,
			cursor_px: [0, 0],
			cursor_hotspot: [0, 0],
			frame_size,
			last_pointer: None,
			wheel_accum: egui::Vec2::ZERO,
			remote_buttons: [false; BUTTON_SLOTS],
			scale,
			host_id,
			pending: None,
			smoke_steps: None,
			smoke_cursor: 0,
			smoke_next_at: None,
			smoke_resolution_sent: false,
			last_synced: String::new(),
			last_files_change_count: None,
			last_files_announced_at: None,
			pushed_file_change_count: None,
			last_clip_poll: std::time::Instant::now(),
			warned_no_remote_clip: false,
			pixels_per_point: 1.0,
			hide_local_cursor: false,
			user_tried_remote_copy: false,
			copy_attempt_at: None,
			first_frame_at: None,
			smoke_clip_files: std::env::var(SMOKE_CLIP_FILES_ENV).ok().map(|spec| {
				let paths: Vec<std::path::PathBuf> = spec
					.split(',')
					.map(str::trim)
					.filter(|part| !part.is_empty())
					.map(std::path::PathBuf::from)
					.collect();
				(paths, Some(std::time::Instant::now() + SMOKE_CLIP_FILES_DELAY))
			}),
			smoke_paste_at: std::env::var(SMOKE_PASTE_ENV)
				.ok()
				.map(|_| std::time::Instant::now() + SMOKE_PASTE_DELAY),
			resolution: Resolution::from_settings(resolution, (frame_size[0], frame_size[1])),
			reported_resolution: resolution,
			dpi,
			reported_dpi: dpi,
			avail: egui::Vec2::ZERO,
		}
	}

	/// 标签标题。
	pub fn title(&self) -> &str {
		&self.title
	}

	/// 断开会话。返回错误文案（正常关闭时为 `None`）。
	pub fn close(&mut self) -> Option<String> {
		self.session.disconnect();
		None
	}

	/// 绘制画面并处理新帧。返回本帧产生的动作（若有）。
	pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<RdpAction> {
		self.poll(ui.ctx());
		// 每帧重置：只有这一帧真的画了远端光标，才藏本机光标
		self.hide_local_cursor = false;
		// 放在这里而不是 `poll` 的新帧分支里：桌面静止时不再有新帧到达，
		// 挂在帧上就等于永不触发
		self.smoke_input(ui.ctx());
		self.smoke_resolution(ui.ctx());
		self.smoke_clip_files();
		self.smoke_paste();

		// 先把断开状态抄出来：下面的分支要 &mut self，借不到 `self.state`
		let ended = match &self.state {
			ViewState::Ended {
				reason,
				acknowledged,
				ever_live,
			} => Some((reason.clone(), *acknowledged, *ever_live)),
			_ => None,
		};

		if let Some((reason, acknowledged, ever_live)) = ended {
			if acknowledged {
				if self.paint_ended(ui, &reason, ever_live) {
					self.reconnect();
				}
			} else {
				// 冻结最后一帧垫在底下：用户能看清断开前是什么样子，
				// 而不是一断就黑屏，让人以为是自己点坏了什么
				self.paint_frame(ui, false);
				if self.end_dialog(ui.ctx(), &reason, ever_live) {
					self.acknowledge_end(reason, ever_live);
				}
				// 告知框等着人点，必须持续重绘
				ui.ctx().request_repaint_after(REPAINT_INTERVAL);
			}
		} else if matches!(self.state, ViewState::Connecting) {
			ui.vertical_centered(|ui| {
				ui.add_space(60.0);
				ui.spinner();
				ui.add_space(8.0);
				ui.label("正在连接远程桌面…");
			});
			// 连接结果由会话线程送来，不续帧就只能等用户碰鼠标
			// （Spinner 自己也会要帧，但不该指望控件内部实现）
			ui.ctx().request_repaint_after(REPAINT_INTERVAL);
		} else {
			self.scale_toolbar(ui);
			self.resolution_toolbar(ui);
			// 「跟随窗口」每帧按最新可用区域重算；防抖把拖动期间的
			// 连续变化吸收成一次请求
			if self.resolution.follow {
				self.queue_follow_resize();
			}
			self.flush_resolution();
			self.paint_frame(ui, true);
			// 帧在别的线程到达，不续帧就看不到更新
			ui.ctx().request_repaint_after(REPAINT_INTERVAL);
		}

		// **本机光标的最后一句话由这里说**：egui 的 `set_cursor_icon`
		// 是「本帧最后写入者赢」，而工具栏、面板边缘、窗口边框都会在
		// 画完之后再写一次。画面上必须只留远端光标，否则就是两个箭头。
		if self.hide_local_cursor {
			ui.ctx().set_cursor_icon(egui::CursorIcon::None);
		}

		self.pending.take()
	}

	/// 断开告知框。返回 `true` 表示用户确认了。
	///
	/// 用 `Modal` 而不是普通 `Window`：断线后底下那张冻结的画面还能点，
	/// 点下去就是往一条已经死掉的会话发输入。
	fn end_dialog(&self, ctx: &egui::Context, reason: &str, ever_live: bool) -> bool {
		// 键盘也算确认：断线时用户手常常还在键盘上
		if ctx.input(|input| {
			input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape)
		}) {
			return true;
		}

		let tokens = design::tokens(ctx);
		let mut acknowledged = false;
		egui::Modal::new(egui::Id::new(("rdp-ended", &self.title))).show(ctx, |ui| {
			ui.set_width(360.0);
			ui.label(
				egui::RichText::new(Self::ended_title(ever_live))
					.size(design::FONT_LG)
					.color(tokens.text),
			);
			ui.add_space(design::GAP_SM);
			ui.label(
				egui::RichText::new(Self::reason_body(reason))
					.size(design::FONT_BASE)
					.color(tokens.text_dim),
			);
			ui.add_space(design::GAP_MD + design::GAP_SM);
			ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
				acknowledged = design::filled_button(ui, "确定").clicked();
			});
		});
		acknowledged
	}

	/// 记下「用户已经看过断开提示」，并让画面真的变空白。
	fn acknowledge_end(&mut self, reason: String, ever_live: bool) {
		self.state = ViewState::Ended {
			reason,
			acknowledged: true,
			ever_live,
		};
		self.drop_frames();
	}

	/// 丢掉画面与光标纹理。
	///
	/// 断开后必须丢：留着就会在重连成功之前一直露着上一条会话的桌面，
	/// 用户会以为「还连着，只是不能操作」。
	fn drop_frames(&mut self) {
		self.texture = None;
		self.cursor_texture = None;
		self.uploaded_seq = 0;
		self.cursor_seq = 0;
	}

	/// 空白页 + 「重新连接」。
	///
	/// **不做倒计时自动重连**：断线多半是主机或网络侧的问题，自动重连只会在
	/// 界面已经用标题说明了「失败 / 断开」，正文里再重复一句「RDP 连接失败：」
	/// 只会把真正有用的原因往下挤。
	fn reason_body(reason: &str) -> &str {
		reason.strip_prefix("RDP 连接失败：").unwrap_or(reason)
	}

	/// 结束画面的标题：**「连不上」和「连上后断了」是两件事**。
	///
	/// 会话从没出过画面时，说「远程桌面已断开」会让人以为刚才那条会话被踢了；
	/// 实际是地址 / 端口 / 凭据 / 网络的问题，提示里也已经写明了原因。
	fn ended_title(ever_live: bool) -> &'static str {
		if ever_live {
			"远程桌面已断开"
		} else {
			"无法连接远程桌面"
		}
	}

	/// 对方还没恢复时反复撞墙，把日志刷满，也让人以为界面在自己乱动。
	fn paint_ended(&mut self, ui: &mut egui::Ui, reason: &str, ever_live: bool) -> bool {
		let tokens = design::tokens(ui.ctx());
		// 画面清空：整块内容区涂底部色，不残留上一条会话的任何一帧
		let rect = ui.available_rect_before_wrap();
		ui.painter().rect_filled(rect, 0.0, tokens.bg_base);

		let mut reconnect = false;
		ui.allocate_ui_with_layout(
			ui.available_size(),
			egui::Layout::top_down(egui::Align::Center),
			|ui| {
				ui.add_space((ui.available_height() * 0.32).max(24.0));
				ui.label(
					egui::RichText::new(Self::ended_title(ever_live))
						.size(design::FONT_LG)
						.color(tokens.text),
				);
				ui.add_space(design::GAP_SM);
				design::caption(ui, Self::reason_body(reason), tokens.text_muted);
				ui.add_space(design::GAP_MD + design::GAP_SM);
				reconnect = design::filled_button(ui, "重新连接").clicked();
			},
		);
		reconnect
	}

	/// 拿原参数重起一条会话。
	///
	/// 「原参数」要按**当前**分辨率先校正一次：用户可能在上一条会话里
	/// 改过尺寸，而 `self.params` 还是建连时那一份——照原样重连等于
	/// 把刚调好的分辨率丢掉。
	fn reconnect(&mut self) {
		tracing::info!("按用户请求重新连接远程桌面「{}」", self.title);
		let settings = self.resolution.settings();
		match settings {
			daisy_core::model::Resolution::Fixed { width, height }
			| daisy_core::model::Resolution::FitWindowAt { width, height } => {
				// 记录里存的是「界面目标」，建连要按当前倍数缩
				let start = scaled_size((width, height), self.dpi);
				self.params.width = start.0;
				self.params.height = start.1;
			},
			daisy_core::model::Resolution::FitWindow => {},
		}
		// 旧会话已经结束，`Drop` 里的 join 会立刻返回
		self.session = RdpSession::spawn(self.params.clone(), self.expected_cert.clone());
		self.state = ViewState::Connecting;

		// 上一条会话的残留一概作废——尤其是纹理，否则新会话首帧到达前
		// 会先闪一下旧桌面
		self.drop_frames();
		self.last_pointer = None;
		self.wheel_accum = egui::Vec2::ZERO;
		self.remote_buttons = [false; BUTTON_SLOTS];
		self.first_frame_at = None;
		self.frame_size = [self.params.width, self.params.height];
		self.resolution = Resolution::from_settings(settings, self.frame_size.into());
		self.reported_resolution = settings;
		self.warned_no_remote_clip = false;
		self.user_tried_remote_copy = false;
		self.copy_attempt_at = None;
	}

	/// 缩放工具栏。改动先落到本视图（立刻有视觉反馈），再上报给应用层落库。
	fn scale_toolbar(&mut self, ui: &mut egui::Ui) {
		let mut changed = false;
		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = 2.0;
			ui.weak("缩放");
			ui.add_space(6.0);
			for (mode, label) in SCALE_MODES {
				let selected = self.scale.mode == mode;
				let response = design::chip(ui, label, selected);
				// 点了当前档不算改动——否则每帧都会去写库
				if response.clicked() && !selected {
					self.scale.mode = mode;
					changed = true;
				}
			}

			if self.scale.mode == ScaleMode::Custom {
				ui.add_space(6.0);
				let mut factor = self.scale.factor;
				// egui 0.36 的 Slider 没有 `width`，宽度由 `spacing.slider_width` 决定
				let response = ui.add(
					egui::Slider::new(&mut factor, MIN_SCALE_FACTOR..=MAX_SCALE_FACTOR)
						.suffix(" %"),
				);
				if response.changed() {
					self.scale.factor = factor;
					// 拖动过程每一帧都是 changed；只在松手时落库，
					// 否则加密库写入会被每秒几十次
					changed = response.drag_stopped();
				}
			}
		});

		if changed {
			tracing::info!(
				"缩放改为 {:?}（{:.0}%）",
				self.scale.mode,
				self.scale.factor * 100.0
			);
			self.pending = Some(RdpAction::ScaleChanged {
				host_id: self.host_id,
				scale: self.scale,
			});
		}
	}

	/// 分辨率工具栏（S8-64）：预设下拉 + 自定义宽高 + 跟随窗口。
	///
	/// **通道没就绪时整行置灰**，并在右侧写明原因——否则用户点下去
	/// 只会得到「没反应」，而真正的原因（服务端不支持 disp）藏在日志里。
	fn resolution_toolbar(&mut self, ui: &mut egui::Ui) {
		let ready = self.resolution.ready;
		ui.horizontal(|ui| {
			ui.weak("分辨率");
			ui.add_enabled_ui(ready, |ui| {
				// 跟随窗口优先：选中它时下拉显示「跟随窗口」，预设保持不动，
				// 用户切回来还是原来那档
				let label = if self.resolution.follow {
					"跟随窗口".to_string()
				} else {
					match self.resolution.preset {
						Some((width, height)) => format!("{width}×{height}"),
						None => {
							format!("{}×{}", self.resolution.custom.0, self.resolution.custom.1)
						},
					}
				};
				egui::ComboBox::from_id_salt("rdp-resolution")
					.selected_text(label)
					// 8 行一屏放全：末行被裁一半时最难选中
					.height(RESOLUTION_MENU_HEIGHT)
					.show_ui(ui, |ui| {
						ui.spacing_mut().item_spacing.y = 2.0;
						let follow = ui.selectable_label(self.resolution.follow, "跟随窗口");
						if follow.clicked() && !self.resolution.follow {
							self.resolution.follow = true;
							self.resolution.preset = None;
							self.resolution.error = None;
							// 立刻算一次，不用等下一帧的可用区域
							self.queue_follow_resize();
						}
						for (size, name) in RESOLUTION_PRESETS {
							let selected =
								!self.resolution.follow && self.resolution.preset == Some(size);
							if ui.selectable_label(selected, name).clicked() {
								self.resolution.follow = false;
								self.resolution.preset = Some(size);
								self.resolution.error = None;
								self.resolution.want(size);
							}
						}
						ui.separator();
						let custom_selected =
							!self.resolution.follow && self.resolution.preset.is_none();
						if ui.selectable_label(custom_selected, "自定义").clicked() {
							self.resolution.follow = false;
							self.resolution.preset = None;
							self.resolution.error = None;
							self.resolution.want(self.resolution.custom);
						}
					});

				ui.weak("缩放").on_hover_text(
					"内容放大倍数：远端按 1/倍数 渲染，本机放大显示（100% 为原样）",
				);
				let dpi_label = format!("{}%", self.dpi);
				egui::ComboBox::from_id_salt("rdp-dpi")
					.selected_text(dpi_label)
					// 12 档一屏放不全就让它滚，避免把工具栏顶高
					.height(RESOLUTION_MENU_HEIGHT)
					.show_ui(ui, |ui| {
						ui.spacing_mut().item_spacing.y = 2.0;
						for value in DPI_OPTIONS {
							if ui
								.selectable_label(self.dpi == value, format!("{value}%"))
								.clicked() && self.dpi != value
							{
								self.dpi = value;
								// 倍数一变就得重算请求尺寸：清掉 last_sent，
								// 否则 `want` 会把它当成「同一个尺寸的重复请求」。
								self.resolution.last_sent = None;
								if self.resolution.follow {
									self.queue_follow_resize();
								} else {
									let target =
										self.resolution.preset.unwrap_or(self.resolution.custom);
									self.resolution.want(target);
								}
								self.notify(
									format!("内容放大已切到 {value}%（远端按此比例缩小渲染，本机放大显示）"),
									false,
								);
							}
						}
					});

				// 自定义档才露数字框，省得工具栏永远占着一块
				if !self.resolution.follow && self.resolution.preset.is_none() {
					let mut width = self.resolution.custom.0;
					let mut height = self.resolution.custom.1;
					let range = RESOLUTION_MIN..=RESOLUTION_MAX;
					let width_box = ui.add(
						egui::DragValue::new(&mut width)
							.range(range.clone())
							.speed(8.0),
					);
					ui.label("×");
					let height_box =
						ui.add(egui::DragValue::new(&mut height).range(range).speed(8.0));
					if width_box.changed() || height_box.changed() {
						self.resolution.custom = (width, height);
						// 拖数字框时每帧都变，只在松手时入队，避免一次拖动
						// 排出几十个分辨率请求
						if ui.ctx().input(|input| input.pointer.any_released()) {
							self.resolution.want((width, height));
						}
					}
				}
			});

			match (&self.resolution.error, ready) {
				(Some(reason), _) => {
					ui.colored_label(egui::Color32::from_rgb(220, 90, 90), reason);
				},
				(None, false) => {
					ui.weak("（等待服务端建立显示通道）");
				},
				(None, true) => {},
			}
		});

		// 只要「选择」和**上次上报过的**不一样就落库——跟随窗口、预设、
		// 自定义三条路都汇到这里，逐个分支记 `changed` 迟早会漏一个。
		//
		// 和「上一帧的值」比不行：`pending` 是单槽，同一帧里若已有别的动作
		// （比如证书待入库），这次选择就再也对不上、永远丢掉了。
		let current = self.resolution.settings();
		if current != self.reported_resolution && self.pending.is_none() {
			self.reported_resolution = current;
			tracing::info!("[分辨率] 记住这次选择：{current}");
			self.pending = Some(RdpAction::ResolutionChanged {
				host_id: self.host_id,
				resolution: current,
			});
		}

		// DPI 同理：选了就记住，下次连接直接生效
		if self.dpi != self.reported_dpi && self.pending.is_none() {
			self.reported_dpi = self.dpi;
			tracing::info!("[分辨率] 记住 DPI 缩放：{}%", self.dpi);
			self.pending = Some(RdpAction::DpiChanged {
				host_id: self.host_id,
				dpi: self.dpi,
			});
		}
	}

	/// 把「跟随窗口」的目标尺寸算出来并入队。
	fn queue_follow_resize(&mut self) {
		if self.avail.x < 1.0 || self.avail.y < 1.0 {
			return;
		}
		let target = follow_target(self.avail);
		tracing::debug!(
			"[分辨率] 跟随目标：avail={}×{}（ppp={}）→ {}×{}",
			self.avail.x,
			self.avail.y,
			self.pixels_per_point,
			target.0,
			target.1
		);
		self.resolution.want(target);
	}

	/// 防抖到点就真的发出去。
	fn flush_resolution(&mut self) {
		let Some((size, since)) = self.resolution.pending else {
			return;
		};
		if since.elapsed() < RESIZE_DEBOUNCE {
			return;
		}
		self.resolution.pending = None;
		if self.resolution.last_sent == Some(size) {
			return;
		}
		self.resolution.last_sent = Some(size);
		self.resolution.error = None;
		// 「缩放」= 内容放大倍数，在**本机**兑现：请求给远端的桌面按倍数
		// 缩小，再由绘制层放大铺满画布。远端（Windows Server）不兑现
		// disp/GCC 里的 DPI 字段，这是唯一能让字号真的变大的通道
		// （2026-11-15 四条路实测，见 docs/PENDING-TESTS.md）。
		// 100% 时不做除法，完全向后兼容。
		let scaled = scaled_size(size, self.dpi);
		tracing::info!(
			"[分辨率] 请求远端 {}×{}（内容放大 {}%）",
			scaled.0,
			scaled.1,
			self.dpi
		);
		self.session.resize(scaled.0, scaled.1);
	}

	fn poll(&mut self, ctx: &egui::Context) {
		// 缩放比例要跟着窗口所在显示器走：拖到外接非 Retina 屏应回到 100%
		self.pixels_per_point = ctx.pixels_per_point();
		for status in self.session.drain_statuses() {
			match status {
				RdpStatus::Connected { width, height } => {
					self.set_frame_size([width, height]);
					// 只把状态从「连接中」推进到「有画面」——首帧可能还没到，
					// `paint` 里纹理为空时会自己退化成提示文案
					if matches!(self.state, ViewState::Connecting) {
						self.state = ViewState::Live;
					}
				},
				RdpStatus::Ended(reason) => {
					// 只弹浮层的话事后查不到原因，这里留一条
					tracing::warn!("RDP 会话「{}」结束：{reason}", self.title);
					let acknowledged = std::env::var(SMOKE_ACK_ENV).is_ok();
					if acknowledged {
						tracing::info!("无人值守断开钩子：直接进入「已确认」画面");
					}
					let ever_live = self.first_frame_at.is_some();
					self.state = ViewState::Ended {
						reason,
						acknowledged,
						ever_live,
					};
					if acknowledged {
						self.drop_frames();
					}
				},
				RdpStatus::DisplayReady => {
					tracing::info!("[分辨率] disp 通道就绪，分辨率控件已启用");
					self.resolution.ready = true;
					self.resolution.error = None;
					// 「跟随窗口」的第一次请求几乎必然早于通道就绪（界面一出来
					// 就排队了），失败之后 `last_sent` 仍记着那个尺寸，
					// 之后每帧的 `want` 都被当成「重复请求」丢掉——桌面于是
					// 永远停在连接时的尺寸，工具栏还挂着一条报错。
					// 清掉它，让下一帧按真实可用区域重算重发。
					if self.resolution.follow {
						self.resolution.last_sent = None;
					}
				},
				RdpStatus::ResizeFailed(reason) => {
					tracing::warn!("[分辨率] 调整失败：{reason}");
					// 不回退 `last_sent`：失败可能是临时的（通道刚断），
					// 保留它才能让用户再点同一个尺寸时触发重试
					self.resolution.error = Some(reason);
				},
				RdpStatus::CertificateFirstSeen {
					host,
					port,
					fingerprint,
					subject,
					issuer,
				} => {
					// 记在视图里：这个标签页内重连直接算「一致」，不再重复提示
					tracing::info!(
						"[证书] 首个指纹待入库：{}:{} {}",
						host,
						port,
						daisy_rdp::format_fingerprint(&fingerprint)
					);
					self.expected_cert = Some(fingerprint.clone());
					self.pending = Some(RdpAction::TrustCertificate {
						host,
						port,
						fingerprint,
						subject,
						issuer,
					});
				},
			}
		}

		// 取走最新一帧并立刻上传，不保留像素副本（1080p 一帧 8 MB）
		if let Some((seq, frame)) = self.session.take_frame() {
			self.upload(ctx, seq, &frame);
			if matches!(self.state, ViewState::Connecting) {
				self.state = ViewState::Live;
			}
			self.first_frame_at
				.get_or_insert_with(std::time::Instant::now);
		}

		// 光标：形状变了才来，来了就上传成纹理
		if let Some((seq, cursor)) = self.session.take_cursor() {
			self.upload_cursor(ctx, seq, &cursor);
		}

		// ---- 剪贴板自动双向同步 ----
		//
		// 真实远程桌面客户端的行为是「一端复制，另一端立刻能用」，
		// 不需要用户按任何快捷键。所以这里无条件接下远端内容，
		// 并按固定间隔把本地剪贴板的变化推给远端。
		//
		// `last_synced` 是防回环的关键：远端推来的写进本地剪贴板后，
		// 本地轮询会又读到同一段——不记住就会推回去，来回乒乓。
		if let Some(text) = self.session.take_remote_clipboard()
			&& text != self.last_synced
		{
			// 空壳/控制字符回波不写本机剪贴板（见 `worth_syncing`）。
			// 这不是丢数据：那种「内容」没有任何可粘贴的意义，写进去只会
			// 悄悄覆盖用户本来的剪贴板。
			if !worth_syncing(&text) {
				tracing::info!(
					"[剪贴板] 远端回波是空壳（{} 字符，{:?}），不写本机剪贴板",
					text.chars().count(),
					text
				);
				self.last_synced = text;
			} else {
				tracing::info!(
					"[剪贴板] 收到远端内容 {} 字，写进本地",
					text.chars().count()
				);
				self.last_synced = text.clone();
				match arboard::Clipboard::new()
					.and_then(|mut clipboard| clipboard.set_text(text.clone()))
				{
					Ok(()) => self.notify(
						format!("远端剪贴板已同步（{} 字）", text.chars().count()),
						false,
					),
					Err(error) => tracing::info!("[剪贴板] 写系统剪贴板失败：{error}"),
				}
			}
		}

		// ---- 远端文件的剪贴板（S10）----
		//
		// 下载完成的文件落盘到「下载」目录并挂上本地剪贴板：
		// Finder 里 Cmd+V 就能粘出来，和 mstsc 的体感一致。
		// 失败（超限/远端拒绝）也要说出来，不说话就变成「复制了没反应」。
		if let Some(error) = self.session.take_download_error() {
			self.notify(format!("远端文件剪贴板失败：{error}"), true);
		}
		if let Some(files) = self.session.take_remote_files() {
			let mut saved = Vec::new();
			let mut failed = 0;
			for file in &files {
				match save_for_clipboard(&file.name, &file.data) {
					Ok(path) => saved.push(path),
					Err(error) => {
						failed += 1;
						tracing::warn!(
							"[剪贴板] 远端文件落盘失败「{}」：{error}",
							file.name
						);
					},
				}
			}
			if saved.is_empty() {
				self.notify("远端文件全部落盘失败，详见日志".to_string(), true);
			} else {
				// 挂上剪贴板：Finder 粘贴的就是这几个文件。
				// 记下 changeCount——轮询会看到这批文件，用它认出是自己写的。
				// 写失败（拿不到 changeCount）时**也不补路径判重**：
				// 那时本地剪贴板根本没变，轮询读到的仍是用户原来那份，
				// 不该被当成「已同步」而丢掉。
				let clipped =
					crate::pasteboard_files::put_files(&saved).is_some_and(|change_count| {
						self.pushed_file_change_count = Some(change_count);
						true
					});
				let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
				// 落盘成功但挂不上剪贴板时必须说清楚：用户会去 Finder 里
				// 按 Cmd+V，只听到一声 beep，完全不知道文件其实在「下载」里。
				let message = if clipped {
					format!(
						"已从远端复制 {} 个文件（{}），可在 Finder 里直接粘贴",
						saved.len(),
						names.join("、")
					)
				} else {
					format!(
						"{} 个文件已存到「下载」（{}），但没能挂上系统剪贴板，请在 Finder 里手动取用",
						saved.len(),
						names.join("、")
					)
				};
				self.notify(message, !clipped);
			}
			if failed > 0 {
				self.notify(format!("{failed} 个远端文件落盘失败，详见日志"), true);
			}
		}

		// 「远端 → 本地」不可用时必须说出来。
		//
		// 这个方向完全由服务端主导：只有远端剪贴板变了，服务端才会主动
		// 广播 `CB_FORMAT_LIST`。客户端没法主动拉——它连远端有哪些格式
		// 都不知道。实测服务端不广播时，**两个方向的表现都是「没反应」**，
		// 从外面看一模一样，用户只会以为软件坏了。
		//
		// 但**必须等用户真的复制过才提示**：服务端在远端没动过剪贴板时
		// 本来就不广播，无条件提示就是第 5 轮那种必然误报的坑。
		//
		// 按**根因**给不同的说法，笼统说「没反应」对用户毫无帮助：
		// 本会话从未开嗓 → 补报后再请用户复制一次；
		// 已开嗓但仍没广播 / 广播了但没数据 → 说清是哪一环。
		let copy_timed_out = self.user_tried_remote_copy
			&& !self.warned_no_remote_clip
			&& self
				.copy_attempt_at
				.is_some_and(|at| at.elapsed() >= COPY_WARN_DELAY);

		// 情况一：**本会话一条格式列表都没上报过** → 远端 rdpclip 不会开始
		// 监听，复制注定收不到。这正是 2026-11-13 当晚用户报的那一种：
		// 本机剪贴板是空的 / 只有一张截图，轮询「没有东西可报」，链路从头哑到尾。
		//
		// 不去让用户重连，而是**就地补报一条空文本**（开嗓），再请用户
		// 复制一次。补报之后 `announced` 会变真，下一轮判定的就是真实原因。
		if copy_timed_out && !self.session.clipboard_announced() {
			self.session.arm_clipboard();
			// 清掉这次尝试的计时：等用户真的再复制一次才重新判定，
			// 否则下一帧就会掉进下面的分支，立刻再弹一条自相矛盾的提示。
			self.copy_attempt_at = None;
			let message = "[剪贴板] 远端复制没传过来：本会话还没有上报过剪贴板（远端因此不会开始监听）。已自动补报一次，请回远端再复制一次".to_string();
			tracing::warn!("{}", message);
			self.notify(message, true);
		} else if copy_timed_out && !self.session.clipboard_remote_data_received() {
			self.warned_no_remote_clip = true;
			// 带诊断信息：用户报「又失效了」时，日志里必须能直接看出
			// 是「服务端一条报文都没发」还是「发了但没数据」，
			// 否则下一轮又只能靠猜（这次就是这么耗掉一整轮）。
			let detail = match self.session.clipboard_last_server_message() {
				Some((kind, ago)) => {
					format!("最近一条远端报文是「{kind}」，{:.1} 秒前", ago.as_secs_f32())
				},
				None => "本会话没收到过任何远端报文".to_string(),
			};
			let message = if self.session.clipboard_server_broadcast() {
				format!("[剪贴板] 远端已广播内容但数据未到（协议层卡住，{detail}），详见日志")
			} else {
				format!(
					"[剪贴板] 远端没有把剪贴板变化发过来（{detail}）：确认在远端真的选中并复制了内容；远端 rdpclip 卡住时需要重启它或重新连接"
				)
			};
			tracing::warn!("{}", message);
			self.notify(message, true);
		}

		// 本地 → 远端：按间隔轮询（系统没有「剪贴板变更」的推送事件）
		if self.last_clip_poll.elapsed() >= CLIPBOARD_POLL_INTERVAL {
			self.last_clip_poll = std::time::Instant::now();

			// **文件优先**：Finder 复制文件时剪贴板上通常没有文本，
			// 文本轮询什么都读不到；反过来复制文本时文件列表为空，
			// 两条路径天然互斥，不用协调。
			//
			// ⚠️ 上面那句「通常没有文本」是错的：**Finder 复制文件时
			// 会把文件名/路径同时放进 `public.utf8-plain-text`**
			// （实测 arboard 在这份剪贴板上读得到 "probe-ok.png"）。
			// 两条分支都跑的话，文件格式列表刚上报就被紧跟着的文本
			// 格式列表覆盖——远端剪贴板变回纯文本，桌面 Ctrl+V 毫无反应
			// （这正是「Finder Cmd+C → 远端 Ctrl+V 无反应」的根因：
			// 无人值守钩子只写 `public.file-url`，读不到文本，所以测不出）。
			// 因此**剪贴板里只要有文件对象，就只报文件，文本分支一律跳过**。
			let files = crate::pasteboard_files::clipboard_files();
			let text_allowed = text_branch_allowed(&files);
			// 这一轮有没有真的把什么东西报给远端。用来判断后面要不要补
			// 那条「开嗓」的空文本——**不能只看 session 的标志位**：
			// 上报是交给会话线程发的，本帧它还没发出去，标志位仍是 false。
			let mut announced_now = false;
			// 这批路径里**真正能带过去**的（存在、读得到元数据）。
			//
			// 不能只看 `paths.is_empty()`：剪贴板上会挂着已经被删除/移走的
			// 文件 URL（复制完再删文件就会这样，**测试往系统剪贴板写临时文件
			// 又删掉也这样**）。那时 `paths` 非空却一个都传不过去，
			// 于是「没有东西可报」这条判断为假、开嗓被一个死链接挡住，
			// 整条链路从连上起就哑着——真机 16:59:29 那次就是这么挂的。
			let transferable = files
				.as_ref()
				.map(|f| collect_local_files(&f.paths))
				.unwrap_or_default();
			let has_transferable = !transferable.is_empty();
			match &files {
				Some(files) if !files.paths.is_empty() => {
					// 自己刚写进去的（远端文件落盘时挂的）——不能回推远端；
					// 同一个 changeCount 已经报过也不再重复。用序号而不是
					// 路径比对：用户对同一批文件再按一次 Cmd+C 会拿到新的
					// 序号，理应重新上报（上次可能远端没收到）。
					if self.pushed_file_change_count != Some(files.change_count)
						&& self.last_files_change_count != Some(files.change_count)
					{
						if has_transferable {
							tracing::info!(
								"[剪贴板] 已把本地 {} 个文件挂上远端剪贴板，等远端粘贴",
								transferable.len()
							);
							self.last_files_change_count = Some(files.change_count);
							self.last_files_announced_at = Some(std::time::Instant::now());
							self.session.send_clipboard_files(transferable);
							announced_now = true;
						} else {
							// 死链接：说清楚是它挡住了，不然又是一轮「为什么没反应」
							tracing::info!(
								"[剪贴板] 剪贴板里的 {} 个文件路径一个都读不到（可能已被删除/移走），本轮当没有内容处理",
								files.paths.len()
							);
						}
					}
				},
				// 有文件对象但一个都带不过去（复制的是文件夹）：明确说一声。
				// 静默失败最难查——用户只会以为 Cmd+C 坏了。
				Some(files) if files.skipped > 0 => {
					if self.last_files_change_count != Some(files.change_count) {
						self.last_files_change_count = Some(files.change_count);
						tracing::warn!(
							"[剪贴板] 本地复制了 {} 项，全部是文件夹（暂不支持）",
							files.skipped
						);
						self.notify("文件夹暂不支持跨机粘贴，请复制文件".to_string(), true);
					}
				},
				Some(_) => {},
				// 读不到在 macOS 上是常态（剪贴板没有文件对象），压到 DEBUG
				None => tracing::debug!("[剪贴板] 本地剪贴板没有文件对象"),
			}

			// 剪贴板里是文件时不碰文本分支：两条格式列表会互相覆盖
			if text_allowed {
				match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
					Ok(text) => {
						if !text.is_empty() && text != self.last_synced {
							self.last_synced = text.clone();
							self.session.send_clipboard(text);
							announced_now = true;
							tracing::info!(
								"[剪贴板] 已把本地内容（{} 字）推给远端",
								self.last_synced.chars().count()
							);
						}
					},
					// 读不到剪贴板在 macOS 上是**常态**——剪贴板为空时 arboard
					// 就返回这个错，而我们每 500ms 轮询一次。INFO 会把日志刷满
					// （实测每分钟一百多条），压到 DEBUG。
					Err(error) => tracing::debug!("[剪贴板] 读本地剪贴板失败：{error}"),
				}
			}

			// ---- 开嗓 ----
			//
			// 本机没有可传内容（空剪贴板 / 只有图片 / 文件已被删）时也要报一条
			// 空文本：Windows 的 rdpclip 在客户端上报第一条格式列表之前不监听
			// 会话剪贴板，远端复制因此永远收不到（详见 `Clipboard::arm`、`should_arm`）。
			if should_arm(has_transferable, announced_now) && !self.session.clipboard_announced() {
				self.session.arm_clipboard();
			}
		}
	}

	/// 把本地剪贴板内容推给远端，并注入 Ctrl+V。
	///
	/// **文件优先，且不能退化成文本。**
	///
	/// Finder 复制文件时剪贴板上**同时**有 `public.file-url` 和
	/// `public.utf8-plain-text`（文本是文件名）。如果在 Daisy 窗口里按
	/// Cmd+V 时只把那段文本报给远端，服务端会把刚才的文件格式列表
	/// 覆盖成文本——用户切到远端粘贴，桌面上什么都不会出现。
	///
	/// 真机日志（2026-11-13）：
	/// ```text
	/// [输入] 兜底取到剪贴板 6 字
	/// [剪贴板] 上报格式列表 3 种：0xd / 0x10 / 0x1   ← 文件列表被覆盖
	/// ```
	fn paste_clipboard_to_remote(&mut self) {
		if let Some(files) = crate::pasteboard_files::clipboard_files()
			&& !files.paths.is_empty()
		{
			let local_files = collect_local_files(&files.paths);
			if !local_files.is_empty() {
				// **刚上报过就别再报一遍**：真机对照实测，粘贴时重复
				// 上报同一批文件，rdpclip 会变成「反复索要描述表、
				// 一次不问大小」，文件永远不落盘；只注入 Ctrl+V 则正常。
				let already_announced = self.last_files_change_count
					== Some(files.change_count)
					&& self
						.last_files_announced_at
						.is_some_and(|at| at.elapsed() < PASTE_REANNOUNCE_WINDOW);
				tracing::info!(
					"[剪贴板] 粘贴：本地剪贴板是 {} 个文件（{}）",
					local_files.len(),
					if already_announced {
						"刚上报过，直接注入 Ctrl+V"
					} else {
						"需要上报文件格式列表"
					}
				);
				// 记住这轮的序号：紧接着的轮询不该再上报一遍。
				self.pushed_file_change_count = Some(files.change_count);
				self.session.paste_files(local_files, !already_announced);
				return;
			}
		}
		match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
			Ok(text) if !text.is_empty() => {
				self.last_synced = text.clone();
				self.session.paste(text);
			},
			Ok(_) => tracing::info!("[剪贴板] 粘贴：剪贴板里既没有文件也没有文本"),
			Err(error) => tracing::warn!("[剪贴板] 粘贴：读本地剪贴板失败：{error}"),
		}
	}

	/// 补发 Ctrl+C / Ctrl+X 到远端（S8-63）。
	fn send_copy_to_remote(&mut self, cut: bool) {		let letter = if cut { RdpKey::X } else { RdpKey::C };
		// Ctrl 必须**按住到底**，且按键之间不能连发——
		// `send_input` 是同步写通道的，四条连着发会被远端当成同一批，
		// Ctrl 被丢掉、远端只看到字母本身（粘贴路径已修，这里同理）。
		let key = |key: RdpKey, down: bool| InputEvent::Key {
			key,
			down,
			repeat: false,
		};
		self.session.send_input(key(RdpKey::ControlLeft, true));
		self.session.send_input(key(letter, true));
		self.session.send_input(key(letter, false));
		self.session.send_input(key(RdpKey::ControlLeft, false));
		tracing::debug!("剪贴板：已补发 Ctrl+{} 到远端", if cut { "X" } else { "C" });
	}

	/// 记一条要显示给用户的提示（S8-63）。
	///
	/// 剪贴板**静默失败最难查**：按了 Cmd+C 什么都没发生，
	/// 用户只会以为键盘坏了。所以成功与失败都要说一声。
	fn notify(&mut self, text: String, is_error: bool) {
		self.pending = Some(RdpAction::Notice { text, is_error });
	}

	/// 上传光标位图。
	fn upload_cursor(&mut self, ctx: &egui::Context, seq: u64, cursor: &daisy_rdp::CursorSprite) {
		if seq == self.cursor_seq && self.cursor_texture.is_some() {
			return;
		}
		let image = egui::ColorImage::from_rgba_unmultiplied(
			[cursor.width as usize, cursor.height as usize],
			&cursor.pixels,
		);
		// 光标要保持像素锐利：放大时用线性过滤会糊成一小团
		let options = egui::TextureOptions::NEAREST;
		match &mut self.cursor_texture {
			Some(handle) => handle.set(image, options),
			None => {
				self.cursor_texture = Some(ctx.load_texture("daisy-rdp-cursor", image, options));
			},
		}
		self.cursor_seq = seq;
		self.cursor_px = [cursor.width, cursor.height];
		self.cursor_hotspot = [cursor.hot_x, cursor.hot_y];
		tracing::debug!(
			"已更新光标纹理：{}x{} 热点({}, {})",
			cursor.width,
			cursor.height,
			cursor.hot_x,
			cursor.hot_y
		);
	}

	/// 无人值守验证：按 [`SMOKE_CLIP_FILES_ENV`] 把文件挂上系统剪贴板。
	///
	/// 必须等首帧（连接稳定）之后触发；写入走 [`crate::pasteboard_files::put_files`]
	/// 与 Finder 相同的路径，轮询会自动读到并上报远端。
	fn smoke_clip_files(&mut self) {
		let Some((paths, at)) = &mut self.smoke_clip_files else {
			return;
		};
		if self.first_frame_at.is_none() {
			return;
		}
		let Some(deadline) = *at else {
			return;
		};
		if std::time::Instant::now() < deadline {
			return;
		}
		*at = None;
		tracing::info!(
			"[冒烟] 把 {} 个文件挂上系统剪贴板：{:?}",
			paths.len(),
			paths
		);
		match crate::pasteboard_files::put_files(paths) {
			Some(change_count) => {
				// 轮询读到这批文件时不能跳过：清掉「自己写的」标记，
				// 让它真的上报远端（这正是本次要验的链路）
				self.pushed_file_change_count = None;
				tracing::info!("[冒烟] 已写入剪贴板 changeCount={change_count}");
			},
			None => tracing::warn!("[冒烟] 写剪贴板失败"),
		}
	}

	/// 无人值守验证：模拟「用户在 Daisy 窗口里按 Cmd+V」（见 [`SMOKE_PASTE_ENV`]）。
	fn smoke_paste(&mut self) {
		let Some(deadline) = self.smoke_paste_at else {
			return;
		};
		if self.first_frame_at.is_none() || std::time::Instant::now() < deadline {
			return;
		}
		self.smoke_paste_at = None;
		tracing::info!("[冒烟] 模拟用户在 Daisy 窗口按 Cmd+V");
		self.paste_clipboard_to_remote();
	}

	/// 无人值守验证：按 [`SMOKE_RESOLUTION_ENV`] 自动改一次分辨率。
	///
	/// 走的是与用户点下拉**完全相同**的路径（`want` → 防抖 → `resize`），
	/// 所以它验的是整条链路，不是某个内部函数。
	fn smoke_resolution(&mut self, ctx: &egui::Context) {
		if self.smoke_resolution_sent {
			return;
		}
		// 通道没就绪就入队也没用——等 `DisplayReady` 之后再说
		if !self.resolution.ready {
			return;
		}
		let Ok(spec) = std::env::var(SMOKE_RESOLUTION_ENV) else {
			return;
		};
		// 给首帧和通道握手留出时间，与输入钩子用同一个延迟
		if self
			.first_frame_at
			.is_none_or(|at| at.elapsed() < SMOKE_INPUT_DELAY)
		{
			return;
		}
		self.smoke_resolution_sent = true;

		if spec.trim() == "follow" {
			tracing::info!("无人值守分辨率钩子：切到跟随窗口");
			self.resolution.follow = true;
			self.resolution.preset = None;
			return;
		}
		let Some((width, height)) = spec.trim().split_once('x') else {
			tracing::warn!("{SMOKE_RESOLUTION_ENV} 的取值「{spec}」不是 WxH，已忽略");
			return;
		};
		let (Ok(width), Ok(height)) = (width.trim().parse(), height.trim().parse()) else {
			tracing::warn!("{SMOKE_RESOLUTION_ENV} 的取值「{spec}」不是数字，已忽略");
			return;
		};
		tracing::info!("无人值守分辨率钩子：请求 {width}×{height}");
		self.resolution.follow = false;
		self.resolution.preset = RESOLUTION_PRESETS
			.iter()
			.map(|(size, _)| *size)
			.find(|size| *size == (width, height));
		if self.resolution.preset.is_none() {
			self.resolution.custom = (width, height);
		}
		self.resolution.want((width, height));
		// 桌面静止时 egui 在空闲、没人驱动 `flush_resolution`——
		// 不主动叫醒一次，钩子的请求会永远停在 pending（真机实测）。
		ctx.request_repaint_after(RESIZE_DEBOUNCE + Duration::from_millis(100));
	}

	/// 无人值守验证：按 [`SMOKE_INPUT_ENV`] 逐步注入输入脚本。
	///
	/// 一步一发、步间留 [`SMOKE_STEP_GAP`]：远端把上一步的结果画出来
	/// （弹出对话框、移动焦点）需要时间，一次性灌进去会打到空处。
	fn smoke_input(&mut self, ctx: &egui::Context) {
		// 只解析一次；环境变量没设就彻底不参与
		if self.smoke_steps.is_none() {
			let Ok(spec) = std::env::var(SMOKE_INPUT_ENV) else {
				return;
			};
			match parse_script(&spec) {
				Ok(steps) => {
					tracing::info!("无人值守输入脚本：{} 步（{spec}）", steps.len());
					self.smoke_steps = Some(steps);
				},
				// 解析失败也记成空脚本：否则每帧都重新解析、日志刷屏
				Err(error) => {
					tracing::warn!("{SMOKE_INPUT_ENV} 解析失败：{error}（取值「{spec}」）");
					self.smoke_steps = Some(Vec::new());
				},
			}
		}
		let Some(steps) = self.smoke_steps.as_ref() else {
			return;
		};
		if self.smoke_cursor >= steps.len() {
			return;
		}
		// 等首帧满 [`SMOKE_INPUT_DELAY`] 再发——见该常量的说明
		let Some(first_frame) = self.first_frame_at else {
			return;
		};
		if first_frame.elapsed() < SMOKE_INPUT_DELAY {
			return;
		}

		// 到点才发下一步。桌面静止时没有输入事件，egui 不会自己再来一帧，
		// 所以必须显式申请重绘，否则脚本会卡在中间不再推进。
		let now = std::time::Instant::now();
		if let Some(at) = self.smoke_next_at
			&& now < at
		{
			ctx.request_repaint_after(at - now);
			return;
		}

		let step = steps[self.smoke_cursor].clone();
		self.smoke_cursor += 1;
		let mut gap = SMOKE_STEP_GAP;
		match step {
			Step::Wait(duration) => gap = duration,
			// 指针位置只能靠 PointerMove 更新，而脚本化移动本机鼠标做不到
			Step::Move(x, y) => {
				tracing::info!("无人值守输入脚本：把指针移到 ({x}, {y})");
				self.last_pointer = Some((x, y));
				self.session.send_input(InputEvent::PointerMove { x, y });
			},
			Step::Click(x, y) => {
				tracing::info!("无人值守输入脚本：在 ({x}, {y}) 点一下左键");
				self.last_pointer = Some((x, y));
				self.session.send_input(InputEvent::PointerMove { x, y });
				for down in [true, false] {
					self.session.send_input(InputEvent::PointerButton {
						button: RdpPointerButton::Left,
						down,
						x,
						y,
					});
				}
			},
			Step::Drag(x1, y1, x2, y2, steps_count) => {
				tracing::info!(
					"无人值守输入脚本：从 ({x1}, {y1}) 拖到 ({x2}, {y2})，共 {steps_count} 段"
				);
				self.session.send_input(InputEvent::PointerMove { x: x1, y: y1 });
				self.session.send_input(InputEvent::PointerButton {
					button: RdpPointerButton::Left,
					down: true,
					x: x1,
					y: y1,
				});
				// 每段之间让出一点时间：拖动要像人手，服务端才会按「连续更新」
				// 处理（一次性灌完等于瞬移，服务端只发一次窗口重绘）
				for i in 1..=steps_count {
					let t = f32::from(i) / f32::from(steps_count);
					let x = f32::from(x1) + (f32::from(x2) - f32::from(x1)) * t;
					let y = f32::from(y1) + (f32::from(y2) - f32::from(y1)) * t;
					let (x, y) = (x.round() as u16, y.round() as u16);
					self.session.send_input(InputEvent::PointerMove { x, y });
					std::thread::sleep(Duration::from_millis(12));
				}
				self.session.send_input(InputEvent::PointerButton {
					button: RdpPointerButton::Left,
					down: false,
					x: x2,
					y: y2,
				});
				self.last_pointer = Some((x2, y2));
			},
			Step::Wheel(x, y, notches) => {
				tracing::info!("无人值守输入脚本：在 ({x}, {y}) 滚 {notches} 格");
				self.last_pointer = Some((x, y));
				self.session.send_input(InputEvent::PointerMove { x, y });
				// 一次发一格：远端按 `WHEEL_DELTA` 的倍数累加，整段一次发也行，
				// 但逐格更接近真实滚轮，且每格都能被服务端单独处理
				let step = if notches.is_negative() { -1 } else { 1 };
				for _ in 0..notches.abs() {
					self.session.send_input(InputEvent::Wheel {
						axis: WheelAxis::Vertical,
						delta: step * WHEEL_DELTA,
						x,
						y,
					});
				}
			},
			Step::Type(text) => {
				let events = type_events(&text);
				tracing::info!(
					"无人值守输入脚本：敲入「{text}」（{} 条事件）",
					events.len()
				);
				for event in events {
					self.session.send_input(event);
				}
			},
			Step::Chord(keys) => {
				let events = chord_events(&keys);
				tracing::info!("无人值守输入脚本：组合键 {keys:?}");
				for event in events {
					self.session.send_input(event);
				}
			},
		}
		self.smoke_next_at = Some(now + gap);
		ctx.request_repaint_after(gap);
	}

	/// 记下桌面尺寸；真的变了就作废与坐标相关的缓存。
	///
	/// 分辨率一变，旧坐标对应的已是另一块区域，直接沿用会点偏。
	/// **不动 `remote_buttons`**：那里面记的是"远端还按着哪个键"，
	/// 丢掉它就再也没机会补一条抬起了。
	fn set_frame_size(&mut self, size: [u32; 2]) {
		if self.frame_size != size {
			// 服务端主动改尺寸（应用 disp 请求、或回退）必须留痕——
			// 「分辨率自己跳回去」这类问题没有这条日志就只能靠截图猜。
			tracing::info!("[分辨率] 远端画面尺寸变为 {}×{}", size[0], size[1]);
		}
		if self.frame_size == size {
			return;
		}
		self.frame_size = size;
		self.last_pointer = None;
		self.wheel_accum = egui::Vec2::ZERO;
		// **工具栏要显示远端的真实尺寸，不是我们请求的尺寸。**
		//
		// 服务端会把请求夹进自己的能力范围（也可能整个忽略），
		// 那时界面若还写着「1280×800」而画面是 1024×768，
		// 用户只会觉得这个下拉在骗人。
		//
		// 只在**没发过任何请求**时同步一次：用户主动改过之后，
		// 尺寸变化本来就是他这次请求的结果，不该被再改回去。
		if self.resolution.last_sent == Some(self.resolution.connection_size) {
			// 远端给的是缩小后的尺寸，乘回倍数才是用户看到的目标值
			self.resolution
				.sync_to(unscaled_size((size[0], size[1]), self.dpi));
		}
	}

	/// 上传一帧到纹理。
	fn upload(&mut self, ctx: &egui::Context, seq: u64, frame: &RgbaFrame) {
		// 同一帧不重复上传（会话层已过滤，这里是防御）
		if seq == self.uploaded_seq && self.texture.is_some() {
			return;
		}

		let image = egui::ColorImage::from_rgba_unmultiplied(
			[frame.width as usize, frame.height as usize],
			&frame.pixels,
		);
		self.set_frame_size([frame.width, frame.height]);

		match &mut self.texture {
			Some(handle) => handle.set(image, egui::TextureOptions::LINEAR),
			None => {
				self.texture =
					Some(ctx.load_texture("daisy-rdp-frame", image, egui::TextureOptions::LINEAR));
			},
		}
		self.uploaded_seq = seq;
	}

	/// 按缩放设置绘制画面；`interactive` 为真时还接管键鼠输入。
	///
	/// 会话已断开时传 `false`：那条会话已经没有输入的去处，把事件发过去
	/// 只会得到一串「线程已退出，输入被丢弃」的日志。
	///
	/// **远端光标也必须一起停画**：它是贴着 `last_pointer` 画的，断开之后
	/// 那个坐标再也不会更新——本机光标又被 `CursorIcon::None` 藏了，
	/// 用户看到的就是「鼠标僵在断开那一刻的位置，怎么动都不走」。
	fn paint_frame(&mut self, ui: &mut egui::Ui, interactive: bool) {
		let Some(texture) = self.texture.as_ref().map(|handle| handle.id()) else {
			if interactive {
				ui.vertical_centered(|ui| {
					ui.add_space(60.0);
					ui.spinner();
					ui.add_space(8.0);
					ui.label("已连接，等待首帧画面…");
				});
			}
			return;
		};

		let frame = egui::vec2(self.frame_size[0] as f32, self.frame_size[1] as f32);
		let avail = ui.available_size();
		// 「跟随窗口」要用下一帧的可用区域，这里记下来
		self.avail = avail;
		if frame.x <= 0.0 || frame.y <= 0.0 || avail.x <= 0.0 || avail.y <= 0.0 {
			return;
		}

		let geometry = scale_geometry(
			self.scale.mode,
			self.scale.factor,
			frame,
			avail,
			ui.ctx().pixels_per_point(),
		);
		// 尺寸下限 1 点：0 尺寸的控件拿不到交互事件
		let size = geometry.size.max(egui::vec2(1.0, 1.0));

		match geometry.overflow {
			// 画得比窗口大：给滚动条，否则只能看到左上角一块
			Overflow::Scroll => {
				egui::ScrollArea::both()
					.auto_shrink([false, false])
					.show(ui, |ui| {
						let (rect, response) =
							ui.allocate_exact_size(size, egui::Sense::click_and_drag());
						ui.painter()
							.image(texture, rect, full_uv(), egui::Color32::WHITE);
						if interactive {
							self.draw_cursor(ui, rect);
							self.input(ui, &response, rect);
						}
					});
			},
			// 适应窗口 / 铺满：占满可用区，把画面摆在中间。
			// 铺满时画面比 avail 还大，四周一圈由面板自己裁掉。
			Overflow::Fits | Overflow::Crop => {
				let (rect, response) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
				let image_rect = egui::Rect::from_center_size(rect.center(), size);
				ui.painter()
					.image(texture, image_rect, full_uv(), egui::Color32::WHITE);
				if interactive {
					self.draw_cursor(ui, image_rect);
					self.input(ui, &response, image_rect);
				}
			},
		}
	}

	/// 把 Windows 光标画到画面上，并在画出来之后隐藏本机光标。
	///
	/// **位置只用本地指针的**：`gdi.cursor_x/y` 看着像服务端坐标，实测发完
	/// 移动事件它也不更新（一直是 (0,0)），拿来兜底只会把光标画到左上角。
	/// 本地坐标（[`Self::last_pointer`]）正是我们发给远端的那一个，
	/// 既精确又没有延迟。
	///
	/// **隐藏本机光标是有条件的**：拿不到远端光标位图前不能藏，否则用户
	/// 会在画面上看不到任何指针。拿到之后必须藏——两个箭头叠在一起既难看
	/// 又会让人以为点不准。
	///
	/// **指针移出画面就不画**，本机光标也会自动回来（`set_cursor_icon` 是
	/// 每帧生效的，不设就是恢复默认）。不这样做的话，远端光标会僵在画面
	/// 边缘不动，用户把鼠标移到标签栏/侧边栏时还留着个假指针在那儿。
	fn draw_cursor(&mut self, ui: &mut egui::Ui, image_rect: egui::Rect) {
		// **窗口不是活动窗口时不画远端光标**。
		//
		// 这时 macOS 会强制显示本机的箭头光标（隐藏会被系统重置），
		// 而我们若照旧把远端光标叠上去，用户就同时看到两个指针——
		// 真机反馈的「两个不同形状指针并存」正是这种情形：
		// 他切去看 Finder，Daisy 还在后面，指针恰好落在画面上。
		if !ui.ctx().input(|input| input.focused) {
			return;
		}
		// 指针不在画面上（甚至不在窗口上）：不画、也不藏本机光标
		if !pointer_over_desktop(image_rect, ui.ctx().pointer_latest_pos()) {
			return;
		}
		// 指针被浮在画面上方的层接住了（分辨率/缩放下拉、右键菜单、tooltip）：
		// 远端光标会画在弹层**底下**，本机光标又被藏起——两头都看不见，
		// 用户看到的就是「菜单里没有光标」。此时光标交还本机。
		// 注意 `layer_id_at` 只查登记过的 Area 层，指针在画布上时返回
		// `None`（Background 不在册），不能把它当成「指针不在画布」。
		if let Some(pos) = ui.ctx().pointer_latest_pos() {
			match ui.ctx().layer_id_at(pos) {
				Some(top) if top != ui.layer_id() => return,
				_ => {},
			}
		}
		let Some(texture) = self.cursor_texture.as_ref().map(|handle| handle.id()) else {
			return;
		};
		// 还没发过任何移动事件时不知道指针在哪儿，此时不画（而不是画在 0,0）
		let Some((x, y)) = self.last_pointer else {
			return;
		};
		let Some(rect) = cursor_rect(
			image_rect,
			self.frame_size,
			(x, y),
			(self.cursor_px[0], self.cursor_px[1]),
			(self.cursor_hotspot[0], self.cursor_hotspot[1]),
		) else {
			return;
		};

		ui.painter()
			.image(texture, rect, full_uv(), egui::Color32::WHITE);
		// 本机光标必须藏掉（两个箭头叠着既难看又让人以为点不准）。
		// 这里先设一次，`ui()` 末尾还会再设一次——期间工具栏/面板/窗口
		// 边框都可能再调 `set_cursor_icon`（egui 是「最后写入者赢」），
		// 只设一次的话，落在这些控件上时本机箭头又会冒出来。
		ui.ctx().set_cursor_icon(egui::CursorIcon::None);
		self.hide_local_cursor = true;
	}

	/// 把 egui 的键鼠事件翻译成 [`InputEvent`] 发给会话线程。
	fn input(&mut self, ui: &egui::Ui, response: &egui::Response, image_rect: egui::Rect) {
		// 点一下就把键盘焦点拿过来，之后按键才归远端
		if response.clicked() || response.drag_started() {
			response.request_focus();
		}

		// ---- 指针移动（不需要焦点：鼠标划过就该动）----
		if (response.hovered() || response.is_pointer_button_down_on())
			&& let Some(pos) = ui.ctx().pointer_latest_pos()
			&& let Some(coord) = desktop_pos(image_rect, self.frame_size, pos)
			&& self.last_pointer != Some(coord)
		{
			self.last_pointer = Some(coord);
			self.session.send_input(InputEvent::PointerMove {
				x: coord.0,
				y: coord.1,
			});
		}

		// **剪贴板快捷键不受焦点门控约束。**
		//
		// `Event::Paste` / `Copy` / `Cut` 是 **egui-winit 在窗口层面**
		// 产出的（`on_keyboard_input` 里直接 `return`，连 `Event::Key`
		// 都不发），它们表达的是「用户按了 Cmd+V」这个**应用级**动作，
		// 与远端画布有没有焦点无关。
		//
		// 而下面的焦点门控是给 `Event::Key` / `PointerButton` 用的——
		// 那些确实要求焦点，否则焦点在主机列表上敲 A 会把字母发给远端。
		//
		// 之前剪贴板和按键一起被挡在门控后面，结果是：
		// **只要没点过画面，Cmd+V 就完全没反应**，现象只是「粘贴没反应」，
		// 毫无线索指向焦点。现在分两趟：先处理剪贴板，再看焦点。
		// ---- 按键级兜底：直接认 Cmd/Ctrl+V，不依赖 `Event::Paste` ----
		//
		// 为什么需要兜底：`Event::Paste` 由 egui-winit 产出，
		// 而它要先 `self.clipboard.get()` 拿到内容才会 push 事件。
		// 万一剪贴板后端没就绪（arboard 初始化失败会退回到应用内的
		// 假剪贴板、`get()` 返回 None），`Event::Paste` 就**永远不产出**——
		// 而现象只是「Cmd+V 没反应」，毫无线索。
		//
		// 这里直接读原始按键：`V` 按下且带 command/ctrl 修饰键，
		// 就自己从系统剪贴板取文本、主动触发粘贴。
		// 这样即便 `Event::Paste` 这条路整个失效，Cmd+V 依然能用。
		{
			let events = ui.ctx().input(|input| input.events.clone());
			// **同帧去重**：Cmd+V 由 egui-winit 直接产出 `Event::Paste`，
			// 而 Ctrl+V 不产出 Paste、只产出 `Event::Key{V}`（mac 上
			// `is_paste_command` 只看 `modifiers.command`）。
			//
			// 两条路各自都能完成粘贴，但**一次按键只能粘一次**：
			// 某帧已经拿到 Paste 就不再兜底。两种修饰键都会按的用户
			// （Windows 习惯 Ctrl、mac 习惯 Cmd）不会因此粘出两份。
			let paste_from_event = events
				.iter()
				.any(|event| matches!(event, egui::Event::Paste(_)));
			for event in events {
				let egui::Event::Key {
					key,
					physical_key,
					pressed: true,
					modifiers,
					..
				} = event
				else {
					continue;
				};
				// 用**事件自带的** modifiers，而不是本帧的 input.modifiers：
				// 用户松手很快时，那一帧的全局修饰键状态可能已经归零，
				// 判据就失效了。
				if !is_paste_shortcut(modifiers, key, physical_key) {
					continue;
				}
				if paste_from_event {
					tracing::info!("[输入] 本帧已有 Event::Paste，跳过兜底（避免粘两次）");
					continue;
				}
				tracing::info!("[输入] 兜底捕获 Cmd/Ctrl+V");
				// 同样走「文件优先」：这里原来只读文本，
				// 直接把远端的文件格式列表覆盖成文本了。
				self.paste_clipboard_to_remote();
			}
		}

		for event in ui.ctx().input(|input| input.events.clone()) {
			// **任何键盘事件都记一条**——判定「按键有没有到 egui」。
			// 前面十三轮我一直在查下游，却从没确认过最上游这一步。
			//
			// 但这是**每键两条**的量级，打字时会把真正有用的行淹掉，
			// 所以压到 DEBUG：排查时 `DAISY_LOG=debug` 就能全部拿回来。
			if let egui::Event::Key { key, pressed, .. } = &event {
				tracing::debug!("[输入] 收到按键 {key:?} pressed={pressed}");
			}
			match event {
				egui::Event::Paste(text) => {
					tracing::info!("[输入] 收到 Event::Paste（{} 字）", text.chars().count());
					// **不直接用事件里的文本**：剪贴板里是文件时 winit 只给
					// 文件名那一小段文本，照它上报会把远端的文件格式列表
					// 覆盖掉。统一走「文件优先」的判定。
					self.paste_clipboard_to_remote();
				},
				egui::Event::Copy => {
					tracing::info!("[输入] 收到 Event::Copy");
					self.user_tried_remote_copy = true;
					self.copy_attempt_at = Some(std::time::Instant::now());
					self.send_copy_to_remote(false);
				},
				egui::Event::Cut => {
					tracing::info!("[输入] 收到 Event::Cut");
					self.user_tried_remote_copy = true;
					self.copy_attempt_at = Some(std::time::Instant::now());
					self.send_copy_to_remote(true);
				},
				// 其余事件要焦点，交给下面那趟
				_ => {},
			}
		}

		// **焦点门控不能「一刀切」**。
		//
		// 原来这里是无条件 `if !has_focus { return }`：只要画布不是焦点控件，
		// 后面所有按键（包括 Ctrl+C / Ctrl+X / 打字 / 方向键）**全部就地丢弃**，
		// 日志里一条都没有。真机日志证实了这个形态：全天没有一条
		// `Event::Copy`（Cmd+C 才产出它），用户按的是 **Ctrl+C**——
		// 那是普通按键事件，画布没焦点就被静默吃掉，远端自然一次广播都不来，
		// 用户看到的就是「远端 Ctrl+C 毫无反应」。
		//
		// 现在的判据：画布有焦点，或**指针正停在远端画面上**。
		// 指针在画面上是「我要操作远端」最强的意图信号，这时按键一律转发。
		// 反过来的情形（光标在左侧主机列表、在搜索框里打字）指针不在画面上，
		// 照旧不转发，不会把字母漏给远端。
		let hovering = response.hovered() || response.is_pointer_button_down_on();
		if !response.has_focus() && !hovering {
			tracing::debug!("[输入] 按键被焦点门控丢弃（画布未聚焦且指针不在画面上）");
			return;
		}

		// 焦点锁：Tab / 方向键 / Esc 不再被 egui 拿去遍历控件或取消焦点。
		// 不锁的话远端收不到这几个键——对 Windows 桌面来说 Tab 是刚需。
		ui.ctx().memory_mut(|memory| {
			memory.set_focus_lock_filter(
				response.id,
				egui::EventFilter {
					tab: true,
					horizontal_arrows: true,
					vertical_arrows: true,
					escape: true,
				},
			);
		});

		let events = ui.ctx().input(|input| input.events.clone());
		// 带 Cmd/Ctrl 的组合键**不转发裸键**给远端。
		//
		// macOS 上 Cmd 在 egui 里就是一个 `Event::Key`，
		// 按住 Cmd+V 会依次产出 `Cmd↓`、`V↓`、`V↑`、`Cmd↑`——
		// 那个 `V` 会从这里原样发给远端，在 Windows 里粘出**字面的字母 v**。
		// 用户实测现象正是「按 Ctrl+V 结果显示 v」。
		//
		// Cmd+C / Cmd+V / Ctrl+V 这类组合由上面那段专门处理（同步剪贴板 + 注入按键），
		// 不该再走「转发裸键」这条路。其他组合键（远端自己的 Ctrl+Alt+Del、
		// 终端里的 Ctrl+C 等）仍照常转发——判定细节见 `should_forward_key`。
		// **焦点门控诊断**：Cmd+V 走不通时，先分清是「事件没来」还是
		// 「事件来了被吃掉」。这一行能直接告出答案。
		// 每帧都会调用，用 debug 免得刷屏；
		// 真正有情况的是下面 Paste / Copy 分支里的 info。
		tracing::debug!("[输入] 本帧 {} 个 egui 事件", events.len());
		for event in events {
			// **任何键盘事件都记一条**——用来判定「按键有没有到 egui」。
			// 同上一趟，压在 DEBUG（每键一条，INFO 会刷屏）。
			if let egui::Event::Key { key, pressed, .. } = &event {
				tracing::debug!("[输入] 有焦点时收到按键 {key:?} pressed={pressed}");
			}
			match event {
				// Cmd+V：**egui-winit 在这里直接 return 了**——它只产出
				// `Event::Paste`，不会再发对应的 `Event::Key`。
				// 也就是说「V」这个按键**根本到不了远端**，光同步剪贴板内容
				// 是不够的：远端收到的是「剪贴板里有新内容」，但没人告诉它
				// 「现在粘贴」。所以这里必须补发一个 Ctrl+V。
				//
				// 这与 Windows 自带 mstsc 的行为一致：它也在连接前问一次
				// 要不要映射剪贴板，勾了之后 Cmd/Ctrl+V 才是"直连两端"的。
				// 剪贴板三者已在上面处理过（不受焦点门控约束）
				egui::Event::Paste(_) | egui::Event::Copy | egui::Event::Cut => {},
				egui::Event::Key {
					key,
					physical_key,
					pressed,
					repeat,
					modifiers,
					..
				} => {
					// Cmd 按着（或 Ctrl+V 粘贴）时不要把裸键发过去，
					// 否则远端会多收一次按键（见 `should_forward_key`）
					if !should_forward_key(ui.ctx(), key, physical_key) {
						continue;
					}
					// **Ctrl+C / Ctrl+X 也算「复制意图」**：Windows 习惯的用户
					// 按的是 Ctrl 而不是 Cmd，这条路径不走 `Event::Copy`，
					// 于是「远端没广播」的诊断永远不会触发——用户只看到
					// 「什么都没发生」。这里补记意图，让它和 Cmd+C 一样能被提示。
					let physical = physical_key.unwrap_or(key);
					if pressed
						&& !repeat
						&& (modifiers.ctrl || modifiers.command)
						&& matches!(physical, egui::Key::C | egui::Key::X)
					{
						self.user_tried_remote_copy = true;
						self.copy_attempt_at = Some(std::time::Instant::now());
					}
					// 优先物理键：它按位置给键、忽略键盘布局，正是远端要的
					if let Some(key) = rdp_key(physical_key.unwrap_or(key)) {
						self.session.send_input(InputEvent::Key {
							key,
							down: pressed,
							repeat,
						});
					}
				},
				egui::Event::PointerButton {
					pos,
					button,
					pressed,
					..
				} => {
					self.pointer_button(image_rect, pos, button, pressed);
				},
				egui::Event::MouseWheel { unit, delta, .. } => {
					self.mouse_wheel(unit, delta);
				},
				_ => {},
			}
		}
	}

	/// 转发鼠标按键，并维护「远端按着哪些键」。
	///
	/// 按下的前提是指针在画面内；**抬起无条件转发**（只要这一键是我们按下的），
	/// 否则拖到画面外松手，远端会一直保持按住。
	fn pointer_button(
		&mut self,
		image_rect: egui::Rect,
		pos: egui::Pos2,
		button: egui::PointerButton,
		pressed: bool,
	) {
		let Some(button) = rdp_button(button) else {
			return;
		};
		let slot = button_slot(button);

		if pressed {
			if !image_rect.contains(pos) {
				return;
			}
		} else if !self.remote_buttons[slot] {
			// 不是我们按下的（比如用户在侧边栏点了右键），不要替它抬起
			return;
		}

		let Some((x, y)) = desktop_pos(image_rect, self.frame_size, pos) else {
			return;
		};
		self.remote_buttons[slot] = pressed;
		self.last_pointer = Some((x, y));
		self.session.send_input(InputEvent::PointerButton {
			button,
			down: pressed,
			x,
			y,
		});
	}

	/// 转发滚轮。
	///
	/// 触控板的位移是连续的，一帧往往不到一格，所以先攒再发（见 [`drain_notches`]）。
	/// 坐标用最后一次发过的位置——滚轮事件本身不带位置。
	fn mouse_wheel(&mut self, unit: egui::MouseWheelUnit, delta: egui::Vec2) {
		self.wheel_accum += wheel_delta_in_notches(unit, delta);
		let (notches, remainder) = drain_notches(self.wheel_accum);
		self.wheel_accum = remainder;

		let (x, y) = self.last_pointer.unwrap_or((0, 0));
		for (axis, notches) in [
			(WheelAxis::Horizontal, notches.0),
			(WheelAxis::Vertical, notches.1),
		] {
			if notches == 0 {
				continue;
			}
			self.session.send_input(InputEvent::Wheel {
				axis,
				// 格数换成 Windows 的 120 单位；一次滚轮最多几十格，不会溢出
				delta: (notches * i32::from(WHEEL_DELTA)) as i16,
				x,
				y,
			});
		}
	}
}

/// 本机指针是否还落在远端画面上。
///
/// 指针移出画面时远端光标就该跟着消失、本机光标该回来。`pos` 为 `None`
/// 表示指针根本不在窗口上（比如用户切到了别的应用），同样按「不在画面上」处理。
///
/// **用画面矩形而不是响应对象的 `hovered()`**：铺满缩放下画面比面板还大、
/// 被裁掉的部分仍在 `image_rect` 里，而拖拽时 `hovered()` 又可能为假——
/// 只有拿指针实际位置和绘制矩形求交才准。
fn pointer_over_desktop(image_rect: egui::Rect, pos: Option<egui::Pos2>) -> bool {
	pos.is_some_and(|pos| image_rect.contains(pos))
}

/// 光标位图该画在哪儿。
///
/// 光标位图是**远端桌面像素**，画面却被缩放/居中绘制，所以要按画面尺寸与
/// 桌面尺寸的比例换算。**热点必须参与计算**——它标出位图里哪个像素才是
/// 指针位置；少了这一步，箭头尖端会整体偏离真实点击点。
///
/// 退化几何（画面尺寸为 0、桌面尺寸为 0）返回 `None`。
fn cursor_rect(
	image_rect: egui::Rect,
	frame_size: [u32; 2],
	desktop: (u16, u16),
	sprite_px: (u16, u16),
	hotspot: (u16, u16),
) -> Option<egui::Rect> {
	let size = image_rect.size();
	let (frame_w, frame_h) = (frame_size[0], frame_size[1]);
	if frame_w == 0 || frame_h == 0 || size.x <= 0.0 || size.y <= 0.0 {
		return None;
	}
	let (sprite_w, sprite_h) = (sprite_px.0 as f32, sprite_px.1 as f32);
	if sprite_w <= 0.0 || sprite_h <= 0.0 {
		return None;
	}

	// 每「桌面像素」对应多少逻辑点
	let scale = egui::vec2(size.x / frame_w as f32, size.y / frame_h as f32);
	let tip = egui::pos2(
		image_rect.min.x + f32::from(desktop.0) * scale.x,
		image_rect.min.y + f32::from(desktop.1) * scale.y,
	);
	let top_left = egui::pos2(
		tip.x - f32::from(hotspot.0) * scale.x,
		tip.y - f32::from(hotspot.1) * scale.y,
	);
	Some(egui::Rect::from_min_size(
		top_left,
		egui::vec2(sprite_w * scale.x, sprite_h * scale.y),
	))
}

/// 把界面坐标换算成远端桌面坐标。
///
/// 画面是等比缩放之后**居中**绘制的（铺满时是放大后居中、被面板裁掉四周），
/// 所以必须按 `image_rect` 与桌面尺寸的**比例**反算；直接用缩放比会漏掉
/// 居中偏移，画面越偏离中心点击偏得越多。
///
/// 两侧长度都是「逻辑点」，Retina 的倍率天然抵消，不必手动除——这一点对
/// 四档缩放都成立：不管画面是被缩小、放大还是裁切，换算只看 `image_rect`
/// 与桌面尺寸的对应关系。
///
/// 超出画面的位置**夹到边界**而不是丢弃——拖选到边缘时不该中断输入。
fn desktop_pos(
	image_rect: egui::Rect,
	frame_size: [u32; 2],
	pos: egui::Pos2,
) -> Option<(u16, u16)> {
	let [width, height] = frame_size;
	let size = image_rect.size();
	if width == 0 || height == 0 || size.x <= 0.0 || size.y <= 0.0 {
		return None;
	}

	// 桌面尺寸理论上可能超过 u16（RDP 坐标是 16 位），夹住以防溢出
	let max_x = (width - 1).min(u32::from(u16::MAX)) as f32;
	let max_y = (height - 1).min(u32::from(u16::MAX)) as f32;

	let relative_x = (pos.x - image_rect.min.x) / size.x;
	let relative_y = (pos.y - image_rect.min.y) / size.y;

	let x = (relative_x * width as f32).clamp(0.0, max_x);
	let y = (relative_y * height as f32).clamp(0.0, max_y);
	Some((x.round() as u16, y.round() as u16))
}

/// 把 egui 的滚轮位移换算成**格**（可以是小数）。
///
/// 轴向与正负都沿用 egui 的约定：`+y` 表示内容下移，也就是向上滚。
/// 这恰好与 Windows `WHEEL_DELTA` 的正方向一致，不需要翻转——
/// egui-winit 也没有翻转（`LineDelta` 原样透传）。
fn wheel_delta_in_notches(unit: egui::MouseWheelUnit, delta: egui::Vec2) -> egui::Vec2 {
	match unit {
		egui::MouseWheelUnit::Line => delta,
		egui::MouseWheelUnit::Point => delta / POINTS_PER_WHEEL_NOTCH,
		egui::MouseWheelUnit::Page => delta * NOTCHES_PER_WHEEL_PAGE,
	}
}

/// 取出累积位移里的整格数，返回 `(整格, 余量)`。
///
/// 触控板一帧的位移常常不足一格，直接四舍五入会把慢速滚动整个吞掉。
/// 把不足一格的部分攒到下一次，慢速滚动也能稳定产生事件。
fn drain_notches(accumulated: egui::Vec2) -> ((i32, i32), egui::Vec2) {
	// `trunc` 向零取整：-0.8 留下 -0.8 的欠账，不会变成 +0.2
	let whole = egui::vec2(accumulated.x.trunc(), accumulated.y.trunc());
	((whole.x as i32, whole.y as i32), accumulated - whole)
}

/// 本地剪贴板轮询间隔。
///
/// 系统没有「剪贴板变了」的推送事件，只能定时读。0.5 秒是权衡：
/// 更短会不停读剪贴板（macOS 上还要过一层 NSPasteboard），
/// 更长则「这边复制完切过去」会觉得慢。
const CLIPBOARD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// 「按了复制但远端没动静」的等待时间。
///
/// 广播是服务端主动发的：正常时从按键到数据落地几十到几百毫秒。
/// 少于 2 秒就下结论会**必然误报**（真机上 Cmd+C 后 200ms 才收到
/// `CB_FORMAT_LIST`），而这条提示只弹一次——误报一次等于把这个
/// 诊断能力彻底废掉。
const COPY_WARN_DELAY: std::time::Duration = std::time::Duration::from_millis(2500);

/// 粘贴时「这批文件刚上报过」的时间窗。
///
/// 窗口内只注入 Ctrl+V、不再重报格式列表——重复上报会把 rdpclip 的
/// 粘贴流程搞坏（见 `last_files_announced_at`）。窗口外则重新上报，
/// 因为远端可能在这期间换了自己的剪贴板内容。
const PASTE_REANNOUNCE_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// 路径 → 要交给远端剪贴板的文件条目（量不到元数据的直接丢掉）。
fn collect_local_files(paths: &[std::path::PathBuf]) -> Vec<daisy_rdp::LocalFile> {
	paths
		.iter()
		.filter_map(|path| {
			let name = path.file_name().map(|n| n.to_string_lossy().into_owned())?;
			let size = std::fs::metadata(path).ok()?.len();
			Some(daisy_rdp::LocalFile {
				name,
				size,
				path: path.clone(),
			})
		})
		.collect()
}

/// 这一轮轮询里，**文本分支还能不能跑**。
///
/// Finder 复制文件时，剪贴板上**同时**有 `public.file-url` 与
/// `public.utf8-plain-text`（实测 arboard 读得到文件名 "probe-ok.png"）。
/// 若文件和文本两条分支都上报，后发的文本格式列表会覆盖先发的文件
/// 格式列表——远端剪贴板变回纯文本，桌面 Ctrl+V 毫无反应。
/// 所以：**剪贴板里有文件对象（哪怕全是被跳过的文件夹）就只报文件**。
///
/// 只有「根本没读到任何文件对象」时才允许报文本，这样复制纯文本
/// 仍然照旧（`readObjectsForClasses:@[NSURL]` 返回空数组的情形）。
fn text_branch_allowed(
	files: &Option<crate::pasteboard_files::ClipboardFiles>,
) -> bool {
	match files {
		Some(files) => files.paths.is_empty() && files.skipped == 0,
		None => true,
	}
}

/// 远端回来的文本值不值得写进系统剪贴板。
///
/// 全是控制字符/空白的「内容」是 rdpclip 的残渣：为了让远端开始监听，
/// 我们会主动上报一条**空文本**格式列表（见 `Clipboard::arm`），
/// 服务端把它装进剪贴板后会再广播回来。真机日志里那条回波是
/// 1 个控制字符（`"\u{3}"`，CF_LOCALE 的残渣），原样写进系统剪贴板
/// 等于把用户刚截的图、刚复制的文件悄悄换掉——用户会以为「剪贴板被清了」。
fn worth_syncing(text: &str) -> bool {
	text.chars().any(|c| !c.is_control() && !c.is_whitespace())
}

/// 要不要补一条「开嗓」的空文本格式列表（见 `Clipboard::arm`）。
///
/// 两条排除：本轮已经上报过别的内容；本地**确实有文件能带过去**
/// （那时补一条空文本会把远端剪贴板顶掉）。
///
/// 这里收的是「真的能带过去的文件」而不是「剪贴板里有没有 URL」，
/// 因为两个坑都出在后者身上：一是 `readObjectsForClasses:@[NSURL]` 在
/// 剪贴板里没有 URL 对象时返回**空数组而不是 nil**（`files` 恒为 `Some`）；
/// 二是剪贴板会挂着**已经被删掉的文件 URL**（复制完再删、或者测试往系统
/// 剪贴板写临时文件又删），`paths` 非空但一个都读不出来。
/// 两版都拿「有没有 URL」判，开嗓一次都没触发过（真机 16:56、16:59 两次）。
fn should_arm(has_transferable_files: bool, announced_now: bool) -> bool {
	!has_transferable_files && !announced_now
}

/// 为「挂剪贴板」落盘：**文件名必须和远端一模一样**。
///
/// 不能沿用 [`save_into`] 的「重名加序号」：那会返回
/// `b (3).txt`，用户粘出来就是 `b (3).txt`，再粘一次变 `b (4).txt`——
/// 真机反馈的「粘贴后文件名带（1）（2）」就是这么来的（序号是我们加的，
/// 不是 Finder 加的）。
///
/// 三种情况：
/// 1. 下载目录没有同名文件 → 存「下载」（用户能在 Finder 里看到）
/// 2. 有同名且内容一致 → 复用（反复复制同一批文件不产生副本）
/// 3. 有同名但内容不同 → 存进缓存目录（**同名**），既不覆盖也不改名
fn save_for_clipboard(name: &str, data: &[u8]) -> std::io::Result<std::path::PathBuf> {
	let home = std::env::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
	let downloads = home.join("Downloads");
	let cache = home
		.join("Library")
		.join("Caches")
		.join("Daisy")
		.join("clipboard");
	save_for_clipboard_in(&downloads, &cache, name, data)
}

/// [`save_for_clipboard`] 的实际实现（目录可注入，便于单测）。
fn save_for_clipboard_in(
	downloads: &std::path::Path,
	cache: &std::path::Path,
	name: &str,
	data: &[u8],
) -> std::io::Result<std::path::PathBuf> {
	let safe_name = name
		.rsplit(['\\', '/'])
		.next()
		.filter(|s| !s.is_empty() && *s != "." && *s != "..")
		.unwrap_or("未命名");

	let preferred = downloads.join(safe_name);
	let same_content = preferred.exists()
		&& std::fs::read(&preferred)
			.map(|existing| existing == data)
			.unwrap_or(false);
	if !preferred.exists() || same_content {
		// 情况 1 / 2：直接落在「下载」，名字保持原样
		return save_into(downloads, safe_name, data);
	}

	// 情况 3：下载目录里已有同名但内容不同的文件。存进缓存目录（同名），
	// 粘出来的名字才是对的；缓存目录里的同名冲突极罕见（上次也可能存过），
	// 真有就让 `save_into` 加序号——那份用户看不见，不影响粘贴体验。
	std::fs::create_dir_all(cache)?;
	save_into(cache, safe_name, data)
}

/// 往目录里写一个文件，**重名加序号**（`a.txt` → `a (1).txt`）。
///
/// 名字是**净化过**的：远端来的文件名可能有路径分隔符（恶意或笔误），
/// 直接拼路径会写到目录之外——按文件名处理。
///
/// 注意：这个「加序号」只能用在**用户看不见的位置**（缓存目录）或
/// 确实需要保留两份的场合。挂剪贴板走的是 [`save_for_clipboard`]——
/// 序号一旦进到文件名里，用户粘出来就是带序号的。
fn save_into(
	downloads: &std::path::Path,
	name: &str,
	data: &[u8],
) -> std::io::Result<std::path::PathBuf> {
	std::fs::create_dir_all(downloads)?;

	// 取出纯文件名（去掉远端可能带的任何目录成分）
	let safe_name = name
		.rsplit(['\\', '/'])
		.next()
		.filter(|s| !s.is_empty() && *s != "." && *s != "..")
		.unwrap_or("未命名");

	let mut target = downloads.join(safe_name);
	// 同名且内容完全一致 → 复用已有文件，**不**产生 ` (1)`。
	//
	// 真机反馈：反复复制同一批文件后，下载目录里堆出
	// `hosts (1) (2) (3)`，再把这些文件粘回远端，名字就带着序号了。
	// 用户眼里「粘贴把文件名改了」，实际上是我们在落盘时改了名。
	// 内容一样就没必要留第二份；内容不同仍然加序号（绝不覆盖用户的文件）。
	if target.exists()
		&& std::fs::read(&target)
			.map(|existing| existing == data)
			.unwrap_or(false)
	{
		return Ok(target);
	}

	let mut index = 1;
	// 重名不覆盖：加 " (1)"、" (2)" 序号
	while target.exists() {
		let stem = std::path::Path::new(safe_name)
			.file_stem()
			.map(|s| s.to_string_lossy().into_owned())
			.unwrap_or_else(|| safe_name.to_owned());
		let ext = std::path::Path::new(safe_name)
			.extension()
			.map(|s| format!(".{}", s.to_string_lossy()));
		target = match ext {
			Some(ext) => downloads.join(format!("{stem} ({index}){ext}")),
			None => downloads.join(format!("{stem} ({index})")),
		};
		index += 1;
	}

	std::fs::write(&target, data)?;
	Ok(target)
}

/// Cmd/Ctrl 按着时，**不要**把这个按键当裸键转发给远端。
///
/// macOS 上 Cmd 是一个真实的 `Event::Key`，所以 Cmd+V 会依次产出
/// `Cmd↓`、`V↓`、`V↑`、`Cmd↑`——其中那个 `V` 若照常转发，
/// 远端 Windows 里就会**粘出字面的字母 v**。
///
/// 实测现象与日志（第二十四轮）：
/// ```
/// 09:57:56.104  [输入] 兜底捕获 Cmd/Ctrl+V      ← 走粘贴通道
/// 09:57:56.104  [输入] 收到按键 V pressed=true   ← 同一个 V 又被转发
/// 09:57:56.135  [剪贴板] 注入第 1 步 ControlLeft↓  ← 我们补发的 Ctrl+V
/// ```
/// 一次 Cmd+V 变成了「远端收到裸 v」+「远端收到 Ctrl+V」两件事。
///
/// Cmd+C / Cmd+V / Cmd+X 这类由上面那段专门处理
/// （同步剪贴板 + 注入按键），不该再走「转发裸键」这条路。
/// 其他组合键（远端自己的 Ctrl+Alt+Del 等）仍照常转发——
/// 这里只挡 macOS 的 command 修饰，不动 ctrl/shift/alt。
fn should_forward_bare_key(ctx: &egui::Context) -> bool {
	!ctx.input(|input| input.modifiers.command)
}

/// 这一下按键是不是「粘贴」组合（Cmd+V 或 Ctrl+V）。
///
/// 用户两个习惯都有：Windows 下按 Ctrl+V，mac 下按 Cmd（外接键盘上
/// 印着 win 键的那个位置）。两条路都得能粘，所以两种修饰键都认。
///
/// **逻辑键与物理键同时认**：中文输入法、非拉丁布局下 `key` 可能
/// 不是 `V`，但 `physical_key` 一定按位置给的是 V——egui-winit 自己
/// 做粘贴判定时也是 `logical_key.or(physical_key)`。
fn is_paste_shortcut(
	modifiers: egui::Modifiers,
	key: egui::Key,
	physical_key: Option<egui::Key>,
) -> bool {
	if !(modifiers.command || modifiers.ctrl) {
		return false;
	}
	key == egui::Key::V || physical_key == Some(egui::Key::V)
}

/// 具体到某个键：这一下要不要作为裸键转发。
///
/// 在 [`should_forward_bare_key`] 之外多挡一条：**Ctrl+V**。
///
/// 用户是从 Windows 过来的，习惯按 Ctrl+V。macOS 上 egui 同样把
/// Ctrl+V 认成粘贴（我们的兜底逻辑就是按 `command || ctrl` 判的），
/// 于是同一个动作会走两条路：
/// 1. 兜底「同步内容 + 注入 Ctrl+V」——正确的粘贴；
/// 2. 裸键转发，把物理 V 原样发给远端——远端再粘一次。
///
/// 结果是**粘两遍**。这里把 2 掐掉。
///
/// 只挡 V：Ctrl+C / Ctrl+X 必须照常转发——远端终端里 Ctrl+C 是中断，
/// 不能因为我们想粘贴就吞掉。
fn should_forward_key(
	ctx: &egui::Context,
	key: egui::Key,
	physical_key: Option<egui::Key>,
) -> bool {
	if !should_forward_bare_key(ctx) {
		return false;
	}
	let modifiers = ctx.input(|input| input.modifiers);
	!is_paste_shortcut(modifiers, key, physical_key)
}

/// `egui::Key` → 本层的物理键。
///
/// 调用方优先传 `Event::Key::physical_key`（按**位置**给键、忽略键盘布局），
/// 只有它缺失时才退回逻辑键。退回时要把 Shift 派生出来的字符
/// （`Colon` / `Pipe` / `Questionmark` 这些）映射回它所在的物理键，
/// 否则这些键会整个丢掉。
///
/// egui 没有 CapsLock / NumLock / PrintScreen / Pause / 菜单键的变体，
/// 所以它们发不出去；小键盘也与主键盘数字共用变体，无法区分。
fn rdp_key(key: egui::Key) -> Option<RdpKey> {
	use RdpKey as R;
	Some(match key {
		// ---- 字母 ----
		egui::Key::A => R::A,
		egui::Key::B => R::B,
		egui::Key::C => R::C,
		egui::Key::D => R::D,
		egui::Key::E => R::E,
		egui::Key::F => R::F,
		egui::Key::G => R::G,
		egui::Key::H => R::H,
		egui::Key::I => R::I,
		egui::Key::J => R::J,
		egui::Key::K => R::K,
		egui::Key::L => R::L,
		egui::Key::M => R::M,
		egui::Key::N => R::N,
		egui::Key::O => R::O,
		egui::Key::P => R::P,
		egui::Key::Q => R::Q,
		egui::Key::R => R::R,
		egui::Key::S => R::S,
		egui::Key::T => R::T,
		egui::Key::U => R::U,
		egui::Key::V => R::V,
		egui::Key::W => R::W,
		egui::Key::X => R::X,
		egui::Key::Y => R::Y,
		egui::Key::Z => R::Z,

		// ---- 主键盘数字（egui 不区分小键盘）----
		egui::Key::Num0 => R::Digit0,
		egui::Key::Num1 => R::Digit1,
		egui::Key::Num2 => R::Digit2,
		egui::Key::Num3 => R::Digit3,
		egui::Key::Num4 => R::Digit4,
		egui::Key::Num5 => R::Digit5,
		egui::Key::Num6 => R::Digit6,
		egui::Key::Num7 => R::Digit7,
		egui::Key::Num8 => R::Digit8,
		egui::Key::Num9 => R::Digit9,

		// ---- 符号。物理键优先，下面几个 Shift 派生的字符是退回逻辑键时的兜底 ----
		egui::Key::Backtick => R::Backtick,
		egui::Key::Minus => R::Minus,
		egui::Key::Equals | egui::Key::Plus => R::Equals,
		egui::Key::OpenBracket | egui::Key::OpenCurlyBracket => R::OpenBracket,
		egui::Key::CloseBracket | egui::Key::CloseCurlyBracket => R::CloseBracket,
		egui::Key::Backslash | egui::Key::Pipe => R::Backslash,
		egui::Key::Semicolon | egui::Key::Colon => R::Semicolon,
		egui::Key::Quote => R::Quote,
		egui::Key::Comma => R::Comma,
		egui::Key::Period => R::Period,
		egui::Key::Slash | egui::Key::Questionmark => R::Slash,
		egui::Key::IntlBackslash => R::IntlBackslash,

		// ---- 编辑键 ----
		egui::Key::Enter => R::Enter,
		egui::Key::Escape => R::Escape,
		egui::Key::Tab => R::Tab,
		egui::Key::Backspace => R::Backspace,
		egui::Key::Space => R::Space,

		// ---- 导航 ----
		egui::Key::Insert => R::Insert,
		egui::Key::Delete => R::Delete,
		egui::Key::Home => R::Home,
		egui::Key::End => R::End,
		egui::Key::PageUp => R::PageUp,
		egui::Key::PageDown => R::PageDown,
		egui::Key::ArrowUp => R::ArrowUp,
		egui::Key::ArrowDown => R::ArrowDown,
		egui::Key::ArrowLeft => R::ArrowLeft,
		egui::Key::ArrowRight => R::ArrowRight,

		// ---- 功能键（egui 到 F35，我们只到 F24）----
		egui::Key::F1 => R::F1,
		egui::Key::F2 => R::F2,
		egui::Key::F3 => R::F3,
		egui::Key::F4 => R::F4,
		egui::Key::F5 => R::F5,
		egui::Key::F6 => R::F6,
		egui::Key::F7 => R::F7,
		egui::Key::F8 => R::F8,
		egui::Key::F9 => R::F9,
		egui::Key::F10 => R::F10,
		egui::Key::F11 => R::F11,
		egui::Key::F12 => R::F12,
		egui::Key::F13 => R::F13,
		egui::Key::F14 => R::F14,
		egui::Key::F15 => R::F15,
		egui::Key::F16 => R::F16,
		egui::Key::F17 => R::F17,
		egui::Key::F18 => R::F18,
		egui::Key::F19 => R::F19,
		egui::Key::F20 => R::F20,
		egui::Key::F21 => R::F21,
		egui::Key::F22 => R::F22,
		egui::Key::F23 => R::F23,
		egui::Key::F24 => R::F24,

		// ---- 修饰键 ----
		egui::Key::ShiftLeft => R::ShiftLeft,
		egui::Key::ShiftRight => R::ShiftRight,
		egui::Key::ControlLeft => R::ControlLeft,
		egui::Key::ControlRight => R::ControlRight,
		egui::Key::AltLeft => R::AltLeft,
		egui::Key::AltRight => R::AltRight,
		egui::Key::SuperLeft => R::SuperLeft,
		egui::Key::SuperRight => R::SuperRight,

		// Command / 命令类事件与剩余的键不转发：
		// `Copy` / `Cut` / `Paste` 是 egui 从组合键派生的，真实按键会另发；
		// `BrowserBack` 没有对应的 RDP 扫描码。
		_ => return None,
	})
}

/// `egui::PointerButton` → 本层的鼠标按键。
///
/// 侧键（`Extra1` / `Extra2`）RDP 要另外的扩展事件，本步先不发。
fn rdp_button(button: egui::PointerButton) -> Option<RdpPointerButton> {
	match button {
		egui::PointerButton::Primary => Some(RdpPointerButton::Left),
		egui::PointerButton::Secondary => Some(RdpPointerButton::Right),
		egui::PointerButton::Middle => Some(RdpPointerButton::Middle),
		_ => None,
	}
}

/// 鼠标按键在 [`RdpView::remote_buttons`] 里的槽位。
fn button_slot(button: RdpPointerButton) -> usize {
	match button {
		RdpPointerButton::Left => 0,
		RdpPointerButton::Middle => 1,
		RdpPointerButton::Right => 2,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// ---- 无人值守输入脚本（冒烟钩子） ----

	/// `;` 分隔、空步忽略、四种步骤都认得。
	#[test]
	fn parses_every_step_kind() {
		let steps =
			parse_script("win+r; ; wait:800; type:cmd; move:10,20; click:12,780; wheel:5,5,-3")
				.unwrap();
		assert_eq!(
			steps,
			vec![
				Step::Chord(vec![RdpKey::SuperLeft, RdpKey::R]),
				Step::Wait(Duration::from_millis(800)),
				Step::Type("cmd".to_owned()),
				Step::Move(10, 20),
				Step::Click(12, 780),
				Step::Wheel(5, 5, -3),
			]
		);
	}

	/// 认不出的键名、坏坐标、非 ASCII 字符都要在解析阶段就报错。
	///
	/// 静默跳过最糟：脚本少一步，验收时只会看到「远端没反应」，
	/// 而真正的原因藏在几十行日志之后。
	#[test]
	fn rejects_bad_scripts() {
		assert!(parse_script("win+nope").is_err());
		assert!(parse_script("move:10").is_err());
		assert!(parse_script("type:中文").is_err());
		assert!(parse_script("wait:soon").is_err());
		assert!(parse_script("   ").is_err());
	}

	/// 组合键的顺序：修饰键先按下、最后**逆序**抬起。
	///
	/// 抬起顺序反了（比如 `ctrl+shift+a` 先松 ctrl）会让远端把中间那一刻
	/// 读成一次单独的按键，偶尔触发意料之外的快捷键。
	#[test]
	fn chord_presses_modifiers_then_releases_in_reverse() {
		let events = chord_events(&[RdpKey::ControlLeft, RdpKey::ShiftLeft, RdpKey::A]);
		let keys: Vec<_> = events
			.iter()
			.map(|event| match event {
				InputEvent::Key { key, down, .. } => (*key, *down),
				_ => panic!("组合键只该产生按键事件"),
			})
			.collect();
		assert_eq!(
			keys,
			vec![
				(RdpKey::ControlLeft, true),
				(RdpKey::ShiftLeft, true),
				(RdpKey::A, true),
				(RdpKey::A, false),
				(RdpKey::ShiftLeft, false),
				(RdpKey::ControlLeft, false),
			]
		);
	}

	/// 敲文本：小写直接按，大写与上档符号自动加 Shift，Shift 用完立刻松开。
	#[test]
	fn typing_inserts_shift_only_where_needed() {
		// "aA!" → a / A / !
		let events = type_events("aA!");
		let keys: Vec<_> = events
			.iter()
			.map(|event| match event {
				InputEvent::Key { key, down, .. } => (*key, *down),
				_ => panic!("敲文本只该产生按键事件"),
			})
			.collect();
		assert_eq!(
			keys,
			vec![
				(RdpKey::A, true),
				(RdpKey::A, false),
				(RdpKey::ShiftLeft, true),
				(RdpKey::A, true),
				(RdpKey::A, false),
				(RdpKey::ShiftLeft, false),
				(RdpKey::ShiftLeft, true),
				(RdpKey::Digit1, true),
				(RdpKey::Digit1, false),
				(RdpKey::ShiftLeft, false),
			]
		);
	}

	/// 认得出验收要用的那些键与字符（`shutdown /l` 全靠这套表）。
	#[test]
	fn key_names_cover_the_acceptance_script() {
		assert_eq!(key_by_name("win"), Some(RdpKey::SuperLeft));
		assert_eq!(key_by_name("Enter"), Some(RdpKey::Enter));
		assert_eq!(key_by_name("s"), Some(RdpKey::S));
		assert_eq!(key_by_name("0"), Some(RdpKey::Digit0));
		assert_eq!(key_by_name("/"), Some(RdpKey::Slash));
		assert_eq!(key_by_name("space"), Some(RdpKey::Space));
		assert_eq!(key_by_name("nosuchkey"), None);
	}

	// ---- 远端分辨率（S8-64） ----

	/// 「跟随窗口」按**逻辑点**取目标，夹进 disp 允许范围并取偶数。
	///
	/// 取偶数是必须的：奇数宽高在部分服务端上会算出半像素偏移，
	/// 整幅画面错半列（表现为右侧一条彩色竖线）。
	///
	/// 不乘 `pixels_per_point`：Retina 上那会请求两倍大的桌面，远端按
	/// 100% 渲染后字只有十几物理像素——用户报「字太小」就是这么来的。
	/// 放大交给本机绘制（逻辑点 → 设备像素），视觉大小才和本地一致。
	#[test]
	fn follow_target_uses_logical_points_and_stays_even() {
		// 2 倍屏上的 1410×897 逻辑点 → 仍是 1410×896（不翻倍，只取偶）
		assert_eq!(follow_target(egui::vec2(1410.0, 897.0)), (1410, 896));
		assert_eq!(follow_target(egui::vec2(1280.0, 800.0)), (1280, 800));
		// 奇数向下取偶
		assert_eq!(follow_target(egui::vec2(801.0, 601.0)), (800, 600));
		// 太小被夹到下限（仍是偶数）
		assert_eq!(follow_target(egui::vec2(10.0, 10.0)), (200, 200));
		// 太大被夹到上限
		assert_eq!(follow_target(egui::vec2(9000.0, 9000.0)), (8192, 8192));
	}

	/// 「缩放」是内容放大倍数：100% 原样，150% 请求缩到 2/3，且往返可逆。
	#[test]
	fn scale_shrinks_the_requested_desktop() {
		// 100% 完全不动（向后兼容）
		assert_eq!(super::scaled_size((1100, 796), 100), (1100, 796));
		// 150% → 缩到 2/3
		assert_eq!(super::scaled_size((1200, 900), 150), (800, 600));
		// 200% → 缩到一半
		assert_eq!(super::scaled_size((1100, 796), 200), (550, 398));
		// 结果仍是偶数、且不低于下限
		assert_eq!(super::scaled_size((300, 300), 500), (200, 200));
		// 逆运算能还原（偶数取整的误差不超过一档）
		assert_eq!(super::unscaled_size((550, 398), 200), (1100, 796));
		assert_eq!(super::unscaled_size((800, 600), 150), (1200, 900));
	}

	/// 同一个尺寸不重复入队，已发过的也不入队。
	///
	/// 「跟随窗口」每帧都会重算一次目标；没有这条判断就会每帧重置防抖起点，
	/// 300ms 永远攒不满——**一次请求都发不出去**，界面看着像没反应。
	#[test]
	fn want_ignores_duplicates_and_already_sent_sizes() {
		let mut resolution = Resolution::new((1024, 768));
		assert_eq!(resolution.last_sent, Some((1024, 768)), "初始尺寸即已发出");

		// 与 last_sent 相同：不入队
		resolution.want((1024, 768));
		assert!(resolution.pending.is_none());

		// 新尺寸：入队
		resolution.want((1280, 800));
		assert!(resolution.pending.is_some());

		// 同一尺寸再来一次：保留原计时，不重置
		let first = resolution.pending.expect("刚入队").1;
		std::thread::sleep(std::time::Duration::from_millis(5));
		resolution.want((1280, 800));
		assert_eq!(
			resolution.pending.expect("仍在队列").1,
			first,
			"不能重置防抖起点"
		);

		// 换了尺寸：换成新的目标
		resolution.want((1600, 900));
		assert_eq!(resolution.pending.expect("换了目标").0, (1600, 900));
	}

	/// 主机记录里的分辨率 ↔ 工具栏状态，两头都要对得上。
	/// 两头对不上会各错一半：
	/// - 只存不读：重开还是默认尺寸（用户说的「记不住」）；
	/// - 读了不反查预设：设成 1600×900 的机器重开后下拉显示成「自定义」，
	///   跟上次点的那个预设名对不上，用户以为设置丢了。
	#[test]
	fn resolution_settings_round_trip_through_the_toolbar() {
		use daisy_core::model::Resolution as Stored;

		// 跟随窗口：settings() 带上最近一次目标（FitWindowAt），
		// 落库后下次**建连**直接用它——首帧即正确分辨率，不再跳变
		let state = Resolution::from_settings(Stored::FitWindow, (1280, 800));
		assert!(state.follow, "记录是跟随窗口，工具栏就该显示跟随窗口");
		assert_eq!(
			state.settings(),
			Stored::FitWindowAt {
				width: 1280,
				height: 800
			}
		);

		// 记住了目标的跟随窗口：往返要原样保真
		let remembered = Stored::fit_window_at(2820, 1794).expect("合法尺寸");
		let state = Resolution::from_settings(remembered, (2820, 1794));
		assert!(state.follow);
		assert_eq!(state.settings(), remembered, "跟随目标必须原样往返");

		// 预设：要能反查到预设名，而不是掉进「自定义」
		let fixed = Stored::fixed(1600, 900).expect("合法尺寸");
		let state = Resolution::from_settings(fixed, (1280, 800));
		assert!(!state.follow);
		assert_eq!(state.preset, Some((1600, 900)), "应显示成预设 1600×900");
		assert_eq!(state.settings(), fixed);

		// 非预设的自定义尺寸：保留数值，回头路一致
		let custom = Stored::fixed(1500, 800).expect("合法尺寸");
		let state = Resolution::from_settings(custom, (1280, 800));
		assert!(!state.follow);
		assert_eq!(state.preset, None);
		assert_eq!(state.custom, (1500, 800));
		assert_eq!(state.settings(), custom);
	}

	/// **回归**：通道就绪后必须能重发「跟随窗口」的首次请求。
	///
	/// 真实链路踩到的：连接时 follow 立刻入队，但那会儿 disp 还没连上，
	/// 请求失败；`flush_resolution` 已经记下 `last_sent`，之后每帧的
	/// `want` 都被当成重复请求丢掉——桌面永远停在连接尺寸 1280×800，
	/// 工具栏挂着一条「disp 通道还没连上」的报错。
	#[test]
	fn display_ready_allows_follow_to_retry_after_early_failure() {
		let mut resolution = Resolution::new((1280, 800));
		resolution.follow = true;

		// 模拟 flush：先记 last_sent，再发，结果通道没就绪、失败
		resolution.want((1800, 1314));
		assert_eq!(resolution.pending.expect("应入队").0, (1800, 1314));
		resolution.pending = None;
		resolution.last_sent = Some((1800, 1314));

		// 不清 `last_sent`：这一帧和之后每一帧的请求都会被丢掉
		resolution.want((1800, 1314));
		assert!(resolution.pending.is_none(), "这正是那个 bug 的形态");

		// 收到 DisplayReady 后清掉 → 下一帧能重发
		resolution.last_sent = None;
		resolution.want((1800, 1314));
		assert_eq!(
			resolution.pending.expect("应重新入队").0,
			(1800, 1314),
			"通道就绪后要能重发首次请求"
		);
	}

	// ---- Cmd+V 的裸键转发 ----

	/// **回归**：Cmd 按着时不能把裸键转发给远端。
	///
	/// macOS 的 Cmd 是真实的 `Event::Key`，Cmd+V 会产出
	/// `Cmd↓ V↓ V↑ Cmd↑`。那个 `V` 若照常转发，
	/// 远端就会**粘出字面的字母 v**（用户实测现象）。
	/// 实测日志里一次 Cmd+V 同时出现了
	/// 「兜底捕获 Cmd/Ctrl+V」与「收到按键 V pressed=true」——
	/// 同一个 V 走了两条路。
	#[test]
	fn command_modifier_suppresses_bare_key_forwarding() {
		let ctx = egui::Context::default();
		// 无修饰键：正常转发
		ctx.input_mut(|input| input.modifiers = egui::Modifiers::default());
		assert!(
			should_forward_bare_key(&ctx),
			"没有修饰键时按键必须转发，否则远端完全打不了字"
		);
		// Cmd 按着：不转发裸键
		ctx.input_mut(|input| {
			input.modifiers.command = true;
			input.modifiers.alt = true;
		});
		assert!(
			!should_forward_bare_key(&ctx),
			"Cmd 按着时不能转发裸键，否则远端粘出字面的 v"
		);
	}

	/// **回归**：只挡 command，不牵连 ctrl / shift / alt。
	///
	/// 远端自己的组合键（Ctrl+Alt+Del 等）必须照常转发——
	/// 在 Windows 那边，ctrl 才是主修饰键，挡掉它会让远端键盘失效。
	#[test]
	fn only_command_modifier_is_suppressed() {
		let ctx = egui::Context::default();
		let cases = [
			("ctrl", egui::Modifiers::default(), true),
			("shift", egui::Modifiers::default(), false),
			("alt", egui::Modifiers::default(), false),
		];
		for (name, base, which) in cases {
			let mut modifiers = base;
			match which {
				true => modifiers.ctrl = true,
				false => {
					modifiers.shift = true;
					modifiers.alt = true;
				},
			}
			ctx.input_mut(|input| input.modifiers = modifiers);
			assert!(
				should_forward_bare_key(&ctx),
				"{name} 修饰不应影响转发——远端组合键靠它"
			);
		}
	}

	// ---- 缩放几何 ----

	/// **回归（第二十六轮）**：Ctrl+V 只走「同步 + 注入」，不再转发裸 V。
	///
	/// 用户从 Windows 过来，习惯按 Ctrl+V。egui 在 macOS 上同样把它
	/// 认成粘贴（兜底逻辑按 `command || ctrl` 判），于是同一个动作走了
	/// 两条路：兜底注入的 Ctrl+V 一次、裸键转发的 V 再一次 → **粘两遍**。
	/// 这里只掐掉 V，Ctrl+C / Ctrl+X 必须照常转发（远端终端里
	/// Ctrl+C 是中断，不能吞）。
	#[test]
	fn ctrl_v_is_not_forwarded_but_ctrl_c_is() {
		let ctx = egui::Context::default();
		ctx.input_mut(|input| {
			input.modifiers = egui::Modifiers::default();
			input.modifiers.ctrl = true;
		});
		assert!(
			!should_forward_key(&ctx, egui::Key::V, Some(egui::Key::V)),
			"Ctrl+V 由我们自己注入，再转发裸 V 会粘两次"
		);
		assert!(
			should_forward_key(&ctx, egui::Key::C, Some(egui::Key::C)),
			"Ctrl+C 必须照常转发——远端终端靠它中断"
		);
		assert!(
			should_forward_key(&ctx, egui::Key::X, Some(egui::Key::X)),
			"Ctrl+X 也必须照常转发"
		);
		// 没有修饰键时 V 正常转发
		ctx.input_mut(|input| input.modifiers = egui::Modifiers::default());
		assert!(
			should_forward_key(&ctx, egui::Key::V, Some(egui::Key::V)),
			"裸 V 必须转发"
		);
	}

	/// 用户的两种习惯：Windows 下 Ctrl+V，mac 下 Cmd+V（外接键盘的 win 键）。
	/// 两条都必须被认成粘贴，否则其中一种会退化成「远端收到字面的 v」。
	#[test]
	fn both_ctrl_v_and_cmd_v_are_paste_shortcuts() {
		let cmd = egui::Modifiers {
			command: true,
			mac_cmd: true,
			..Default::default()
		};
		let ctrl = egui::Modifiers {
			ctrl: true,
			..Default::default()
		};
		assert!(
			is_paste_shortcut(cmd, egui::Key::V, Some(egui::Key::V)),
			"Cmd+V（mac 习惯）必须认"
		);
		assert!(
			is_paste_shortcut(ctrl, egui::Key::V, Some(egui::Key::V)),
			"Ctrl+V（Windows 习惯）必须认"
		);
		// 非拉丁布局：逻辑键不是 V，但物理位置是 V
		assert!(
			is_paste_shortcut(ctrl, egui::Key::Y, Some(egui::Key::V)),
			"按位置认键——逻辑键不是 V 时也要能粘"
		);
		// 别误伤：单按 V、Cmd+C、Ctrl+X 都不是粘贴
		assert!(!is_paste_shortcut(
			egui::Modifiers::default(),
			egui::Key::V,
			Some(egui::Key::V)
		));
		assert!(!is_paste_shortcut(cmd, egui::Key::C, Some(egui::Key::C)));
		assert!(!is_paste_shortcut(ctrl, egui::Key::X, Some(egui::Key::X)));
	}

	/// 1024×768 的远端桌面放进 800×600 的可用区域。
	const FRAME: egui::Vec2 = egui::vec2(1024.0, 768.0);
	const AVAIL: egui::Vec2 = egui::vec2(800.0, 600.0);

	#[test]
	fn fit_keeps_the_whole_desktop_visible() {
		let geometry = scale_geometry(ScaleMode::Fit, 1.0, FRAME, AVAIL, 1.0);
		// 限制来自宽度：800/1024 比 600/768 小
		assert_eq!(geometry.size.x, 800.0);
		assert_eq!(geometry.size.y, 600.0);
		assert_eq!(geometry.overflow, Overflow::Fits);
	}

	/// 铺满必须**不留黑边**：取两个方向里较大的缩放比。
	///
	/// 可用区特意用长宽比不同的 900×400——若宽高比恰好与画面相同
	/// （比如 800×600 对 1024×768），两种模式会算出同一个比例，测不出差别。
	#[test]
	fn fill_covers_every_edge() {
		let wide = egui::vec2(900.0, 400.0);
		let geometry = scale_geometry(ScaleMode::Fill, 1.0, FRAME, wide, 1.0);
		// 宽度受限：900/1024 比 400/768 大，取它 → 宽度正好铺满
		assert_eq!(geometry.size.x, 900.0);
		// 高度按宽度等比放大到 675，超出可用高度 → 由面板裁掉上下
		assert!(
			geometry.size.y > wide.y,
			"铺满时高度应超出可用区域，实际 {} vs {}",
			geometry.size.y,
			wide.y
		);
		assert_eq!(geometry.overflow, Overflow::Crop);
	}

	/// 原始大小 = 远端 1 像素对屏幕 1 像素，所以要除以倍率。
	#[test]
	fn actual_is_one_to_one_device_pixels() {
		let retina = scale_geometry(ScaleMode::Actual, 1.0, FRAME, AVAIL, 2.0);
		assert_eq!(retina.size.x, 512.0);
		assert_eq!(retina.size.y, 384.0);

		// 普通屏上就是原始像素
		let normal = scale_geometry(ScaleMode::Actual, 1.0, FRAME, AVAIL, 1.0);
		assert_eq!(normal.size.x, 1024.0);
	}

	#[test]
	fn custom_factor_scales_relative_to_device_pixels() {
		let geometry = scale_geometry(ScaleMode::Custom, 0.5, FRAME, AVAIL, 2.0);
		// 0.5 倍 ÷ 2 倍率 = 画面按 1/4 原始像素显示
		assert_eq!(geometry.size.x, 256.0);
		assert_eq!(geometry.overflow, Overflow::Fits);
	}

	/// 自定义放大到超出窗口时给滚动条，而不是把画面裁掉——原始大小同理。
	#[test]
	fn overscrolled_actual_and_custom_scroll() {
		let big = scale_geometry(ScaleMode::Actual, 1.0, FRAME, AVAIL, 0.5);
		assert_eq!(big.overflow, Overflow::Scroll);
		let big_custom = scale_geometry(ScaleMode::Custom, 4.0, FRAME, AVAIL, 1.0);
		assert_eq!(big_custom.overflow, Overflow::Scroll);
	}

	/// 铺满画大了是**故意**的，不该给滚动条；适应窗口永远不滚动。
	#[test]
	fn fit_never_scrolls_and_fill_never_scrolls() {
		let tiny = egui::vec2(100.0, 80.0);
		assert_eq!(
			scale_geometry(ScaleMode::Fit, 1.0, FRAME, tiny, 1.0).overflow,
			Overflow::Fits
		);
		assert_eq!(
			scale_geometry(ScaleMode::Fill, 1.0, FRAME, tiny, 1.0).overflow,
			Overflow::Crop
		);
	}

	/// 两种基准在普通屏上应当一致——否则「原始大小」和「适应窗口」的差别
	/// 会在 Retina 与普通屏之间表现得不一致。
	#[test]
	fn custom_at_100_percent_equals_actual_on_normal_display() {
		let actual = scale_geometry(ScaleMode::Actual, 1.0, FRAME, AVAIL, 1.0);
		let custom = scale_geometry(ScaleMode::Custom, 1.0, FRAME, AVAIL, 1.0);
		assert_eq!(actual.size, custom.size);
		assert_eq!(actual.overflow, custom.overflow);
	}

	/// 退化输入要有定义：0 尺寸不能产生 inf / NaN（会画出一块看不见的东西）。
	#[test]
	fn degenerate_inputs_are_handled() {
		let zero_frame = scale_geometry(ScaleMode::Fit, 1.0, egui::Vec2::ZERO, AVAIL, 1.0);
		assert_eq!(zero_frame.size, egui::Vec2::ZERO);
		assert_eq!(zero_frame.overflow, Overflow::Fits);

		let zero_avail = scale_geometry(ScaleMode::Fit, 1.0, FRAME, egui::Vec2::ZERO, 1.0);
		assert_eq!(zero_avail.overflow, Overflow::Fits);

		// 倍率为 0 不能把尺寸算成无穷大
		let zero_ppp = scale_geometry(ScaleMode::Actual, 1.0, FRAME, AVAIL, 0.0);
		assert_eq!(zero_ppp.size, FRAME);
	}

	/// 缩放变了，`desktop_pos` 照样按新的 image_rect 反算——这是四档共用的
	/// 那条路径，测它保证换档不会把坐标映射弄歪。
	#[test]
	fn desktop_pos_follows_the_scaled_rect() {
		let desktop = [1024, 768];
		// 适应窗口：画面 800×600 摆在 (0,0)-(800,600) 的区域正中
		let rect = egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(900.0, 700.0));
		assert_eq!(
			desktop_pos(rect, desktop, egui::pos2(500.0, 400.0)),
			Some((512, 384))
		);
		// 原始大小下画面更小，rect 也更小，但中心点仍映射到桌面中心
		let small = egui::Rect::from_min_max(egui::pos2(300.0, 200.0), egui::pos2(812.0, 584.0));
		assert_eq!(
			desktop_pos(small, desktop, egui::pos2(556.0, 392.0)),
			Some((512, 384))
		);
	}

	// ---- 光标定位 ----

	/// 指针在画面内才画远端光标；移出去就交给本机光标。
	#[test]
	fn pointer_over_desktop_tracks_leaving_the_frame() {
		let image = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1024.0, 768.0));
		assert!(
			pointer_over_desktop(image, Some(egui::pos2(500.0, 400.0))),
			"画面正中应算在画面上"
		);
		assert!(
			!pointer_over_desktop(image, Some(egui::pos2(1100.0, 400.0))),
			"移到画面右侧之外应交还本机光标"
		);
		assert!(
			!pointer_over_desktop(image, Some(egui::pos2(500.0, -10.0))),
			"移到画面上方之外（标签栏）应交还本机光标"
		);
		assert!(
			!pointer_over_desktop(image, None),
			"指针不在窗口上（切到别的应用）时不该画远端光标"
		);
	}

	/// 铺满缩放下画面被面板裁掉一部分——裁掉的区域外仍要算移出。
	#[test]
	fn pointer_over_desktop_uses_the_image_rect_not_the_panel() {
		// 画面比可视区大，四周一圈被裁掉；画面外那圈不该画远端光标
		let image = egui::Rect::from_min_max(egui::pos2(-40.0, -30.0), egui::pos2(1080.0, 810.0));
		assert!(pointer_over_desktop(image, Some(egui::pos2(-20.0, -20.0))));
		assert!(!pointer_over_desktop(image, Some(egui::pos2(-60.0, 400.0))));
		assert!(!pointer_over_desktop(image, Some(egui::pos2(500.0, 820.0))));
	}

	/// 1:1（画面像素 = 桌面像素）时光标按原尺寸贴，热点对齐指针。
	#[test]
	fn cursor_rect_at_native_scale() {
		let image = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1024.0, 768.0));
		let rect =
			cursor_rect(image, [1024, 768], (100, 200), (32, 32), (0, 0)).expect("1:1 应算得出来");
		assert_eq!(rect.min, egui::pos2(100.0, 200.0));
		assert_eq!(rect.size(), egui::vec2(32.0, 32.0));
	}

	/// 缩小一半时，光标也要跟着缩到一半——否则会是个巨大的糊块。
	#[test]
	fn cursor_rect_follows_the_frame_scale() {
		let image = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(512.0, 384.0));
		let rect =
			cursor_rect(image, [1024, 768], (100, 200), (32, 32), (0, 0)).expect("应算得出来");
		assert_eq!(rect.size(), egui::vec2(16.0, 16.0));
		assert_eq!(rect.min, egui::pos2(50.0, 100.0));
	}

	/// 热点偏移：箭头尖端要落在指针上，所以左上角要**减去**热点。
	#[test]
	fn cursor_rect_offsets_by_hotspot() {
		let image = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1024.0, 768.0));
		let rect =
			cursor_rect(image, [1024, 768], (100, 200), (32, 32), (3, 5)).expect("应算得出来");
		assert_eq!(rect.min, egui::pos2(97.0, 195.0));
		assert_eq!(rect.size(), egui::vec2(32.0, 32.0));
	}

	/// 画面在窗口里有偏移时（居中留边），光标位置也要跟着偏。
	#[test]
	fn cursor_rect_respects_letterbox_offset() {
		let image = egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(612.0, 484.0));
		let rect = cursor_rect(image, [1024, 768], (0, 0), (32, 32), (0, 0)).expect("应算得出来");
		assert_eq!(rect.min, egui::pos2(100.0, 100.0));
	}

	#[test]
	fn cursor_rect_rejects_degenerate_geometry() {
		let image = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
		assert!(cursor_rect(image, [0, 768], (1, 1), (32, 32), (0, 0)).is_none());
		assert!(cursor_rect(image, [1024, 0], (1, 1), (32, 32), (0, 0)).is_none());
		// 位图尺寸未知时也不画（宁可不画，也不要画一个 0×0 的东西）
		assert!(cursor_rect(image, [1024, 768], (1, 1), (0, 0), (0, 0)).is_none());
		let empty = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.0, 0.0));
		assert!(cursor_rect(empty, [1024, 768], (1, 1), (32, 32), (0, 0)).is_none());
	}

	// ---- 键鼠事件 ----

	/// 居中且带留白的画面：400×300 点画 1600×1200 的桌面。
	fn sample_rect() -> egui::Rect {
		egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(500.0, 400.0))
	}

	#[test]
	fn desktop_pos_maps_corners() {
		let rect = sample_rect();
		let size = [1600, 1200];
		assert_eq!(
			desktop_pos(rect, size, egui::pos2(100.0, 100.0)),
			Some((0, 0))
		);
		// 右下角落在地一个像素之外，夹回最后一列 / 最后一行
		assert_eq!(
			desktop_pos(rect, size, egui::pos2(500.0, 400.0)),
			Some((1599, 1199))
		);
	}

	#[test]
	fn desktop_pos_maps_center() {
		let rect = sample_rect();
		assert_eq!(
			desktop_pos(rect, [1600, 1200], egui::pos2(300.0, 250.0)),
			Some((800, 600))
		);
	}

	/// 缩放比不能靠"可用区域"算——画面居中，左侧有留白，
	/// 用错基准会让整块坐标平移。
	#[test]
	fn desktop_pos_respects_letterbox_offset() {
		let rect = sample_rect();
		// 画面左沿再往左 100 点是留白，应夹到 0 而不是负数
		assert_eq!(
			desktop_pos(rect, [1600, 1200], egui::pos2(0.0, 250.0)),
			Some((0, 600))
		);
	}

	#[test]
	fn desktop_pos_clamps_outside() {
		let rect = sample_rect();
		let size = [1600, 1200];
		// 右下方向拖出画面：夹到右下角，输入不中断
		assert_eq!(
			desktop_pos(rect, size, egui::pos2(9999.0, 9999.0)),
			Some((1599, 1199))
		);
		assert_eq!(
			desktop_pos(rect, size, egui::pos2(-9999.0, -9999.0)),
			Some((0, 0))
		);
	}

	#[test]
	fn desktop_pos_rejects_degenerate_geometry() {
		let rect = sample_rect();
		assert_eq!(desktop_pos(rect, [0, 1200], egui::pos2(300.0, 250.0)), None);
		assert_eq!(desktop_pos(rect, [1600, 0], egui::pos2(300.0, 250.0)), None);
		let empty = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(0.0, 0.0));
		assert_eq!(desktop_pos(empty, [1600, 1200], egui::pos2(0.0, 0.0)), None);
	}

	/// 窗口比桌面像素还大时（放大显示），坐标同样要对。
	#[test]
	fn desktop_pos_handles_upscaled_display() {
		// 100×100 点画 50×50 的桌面 → 放大 2 倍
		let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
		assert_eq!(
			desktop_pos(rect, [50, 50], egui::pos2(50.0, 50.0)),
			Some((25, 25))
		);
	}

	#[test]
	fn wheel_line_unit_counts_as_notches() {
		let delta = egui::vec2(0.0, 2.0);
		assert_eq!(
			wheel_delta_in_notches(egui::MouseWheelUnit::Line, delta),
			egui::vec2(0.0, 2.0)
		);
	}

	#[test]
	fn wheel_point_unit_scales_by_step() {
		let delta = egui::vec2(0.0, POINTS_PER_WHEEL_NOTCH * 2.0);
		assert_eq!(
			wheel_delta_in_notches(egui::MouseWheelUnit::Point, delta),
			egui::vec2(0.0, 2.0)
		);
	}

	#[test]
	fn wheel_page_unit_is_multiplied() {
		let delta = egui::vec2(0.0, 1.0);
		assert_eq!(
			wheel_delta_in_notches(egui::MouseWheelUnit::Page, delta),
			egui::vec2(0.0, NOTCHES_PER_WHEEL_PAGE)
		);
	}

	#[test]
	fn drain_notches_keeps_remainder_with_sign() {
		let (whole, rest) = drain_notches(egui::vec2(2.7, -0.5));
		assert_eq!(whole, (2, 0));
		// 向上的欠账与向下的欠账都不能被抹平
		assert!((rest.x - 0.7).abs() < 1e-6);
		assert!((rest.y + 0.5).abs() < 1e-6);
	}

	/// 慢速滚动：每次不足一格，攒够才发一条——不能一路舍成 0。
	#[test]
	fn drain_notches_accumulates_small_steps() {
		let step = egui::vec2(0.0, 0.4);
		let mut accum = egui::Vec2::ZERO;
		let mut emitted = 0;
		for _ in 0..3 {
			accum += step;
			let (whole, rest) = drain_notches(accum);
			emitted += whole.1;
			accum = rest;
		}
		assert_eq!(emitted, 1, "三次 0.4 格应攒出一条");
		assert!((accum.y - 0.2).abs() < 1e-6, "余量应当留下");
	}

	#[test]
	fn drain_notches_passes_whole_values_through() {
		let (whole, rest) = drain_notches(egui::vec2(-3.0, 1.0));
		assert_eq!(whole, (-3, 1));
		assert_eq!(rest, egui::Vec2::ZERO);
	}

	#[test]
	fn maps_letters_digits_and_navigation() {
		assert_eq!(rdp_key(egui::Key::A), Some(RdpKey::A));
		assert_eq!(rdp_key(egui::Key::Num0), Some(RdpKey::Digit0));
		assert_eq!(rdp_key(egui::Key::Tab), Some(RdpKey::Tab));
		assert_eq!(rdp_key(egui::Key::ArrowLeft), Some(RdpKey::ArrowLeft));
	}

	/// 修饰键必须按左右分开——远端 Ctrl+Alt+Del 之类靠它区分。
	#[test]
	fn maps_both_sides_of_modifiers() {
		assert_eq!(rdp_key(egui::Key::ShiftLeft), Some(RdpKey::ShiftLeft));
		assert_eq!(rdp_key(egui::Key::ShiftRight), Some(RdpKey::ShiftRight));
		assert_eq!(rdp_key(egui::Key::ControlLeft), Some(RdpKey::ControlLeft));
		assert_eq!(rdp_key(egui::Key::ControlRight), Some(RdpKey::ControlRight));
		assert_eq!(rdp_key(egui::Key::AltRight), Some(RdpKey::AltRight));
	}

	/// 退回逻辑键时要认得 Shift 派生字符，否则 `?` `:` `|` 这些会丢。
	#[test]
	fn maps_shift_derived_characters_to_physical_keys() {
		assert_eq!(rdp_key(egui::Key::Questionmark), Some(RdpKey::Slash));
		assert_eq!(rdp_key(egui::Key::Colon), Some(RdpKey::Semicolon));
		assert_eq!(rdp_key(egui::Key::Pipe), Some(RdpKey::Backslash));
		assert_eq!(rdp_key(egui::Key::Plus), Some(RdpKey::Equals));
	}

	/// 派生事件不转发：真按键会另发一条，转发它就变成发两次。
	#[test]
	fn skips_derived_events() {
		assert_eq!(rdp_key(egui::Key::Copy), None);
		assert_eq!(rdp_key(egui::Key::Cut), None);
		assert_eq!(rdp_key(egui::Key::Paste), None);
	}

	#[test]
	fn maps_primary_buttons_only() {
		assert_eq!(
			rdp_button(egui::PointerButton::Primary),
			Some(RdpPointerButton::Left)
		);
		assert_eq!(
			rdp_button(egui::PointerButton::Secondary),
			Some(RdpPointerButton::Right)
		);
		assert_eq!(
			rdp_button(egui::PointerButton::Middle),
			Some(RdpPointerButton::Middle)
		);
		assert_eq!(rdp_button(egui::PointerButton::Extra1), None);
	}

	/// 三个按键各占一个槽——槽位撞车会让一个键的抬起清掉另一个键的状态。
	#[test]
	fn button_slots_are_distinct() {
		let slots = [
			button_slot(RdpPointerButton::Left),
			button_slot(RdpPointerButton::Middle),
			button_slot(RdpPointerButton::Right),
		];
		assert_eq!(
			slots.iter().collect::<std::collections::HashSet<_>>().len(),
			3
		);
		for slot in slots {
			assert!(slot < BUTTON_SLOTS);
		}
	}

	/// 断开后必须先问一句，确认之后画面才清空；重连从原参数重起，不自动倒计时。
	///
	/// 用 `127.0.0.1:1` 当目标：这个端口必然拒绝连接，会话线程会立刻回报
	/// `Ended`，正好走完整条「断开 → 告知 → 确认 → 重连」的路径。
	#[test]
	fn disconnect_asks_first_then_reconnects_on_demand() {
		let ctx = egui::Context::default();
		let mut params = ConnectionParams::new("127.0.0.1", "daisy", "x");
		params.port = 1;
		let mut view = RdpView::new(
			"测试".to_string(),
			params,
			None,
			daisy_core::model::DisplaySettings::default(),
			None,
		);

		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
		while !matches!(view.state, ViewState::Ended { .. }) && std::time::Instant::now() < deadline
		{
			view.poll(&ctx);
			std::thread::sleep(std::time::Duration::from_millis(20));
		}

		let ViewState::Ended { acknowledged, .. } = &view.state else {
			panic!("连不上的会话应进入 Ended，实际还没到");
		};
		assert!(
			!acknowledged,
			"断开后应先弹告知框——画面直接变空白等于什么都没说"
		);

		// 点「确定」
		view.acknowledge_end("测试断开".to_string(), false);
		assert!(view.texture.is_none(), "确认之后画面必须清空");
		assert!(
			matches!(
				view.state,
				ViewState::Ended {
					acknowledged: true,
					..
				}
			),
			"确认之后应停在「空白页 + 重新连接」"
		);

		// 点「重新连接」
		view.reconnect();
		assert!(
			matches!(view.state, ViewState::Connecting),
			"重连应回到连接中，而不是停在断开页"
		);
		assert!(view.texture.is_none(), "重连后不得残留上一条会话的画面");
	}

	/// 断开后那张冻结的画面**不能**继续画远端光标。
	///
	/// 真机现象：RDP 主动断开后，鼠标僵在黑屏左下角（断开那一刻的位置）动不了，
	/// 只有把指针移出整个窗口才恢复。原因是冻结画面仍走 `draw_cursor`：
	/// 远端光标被画成一个不会移动的假指针，本机光标又被 `CursorIcon::None` 藏了。
	#[test]
	fn frozen_frame_keeps_the_local_cursor_visible() {
		let ctx = egui::Context::default();
		ctx.run_ui(egui::RawInput::default(), |_ui| {})
			.textures_delta
			.clear();

		let mut params = ConnectionParams::new("127.0.0.1", "daisy", "x");
		params.port = 1;
		let mut view = RdpView::new(
			"冻结".to_string(),
			params,
			None,
			daisy_core::model::DisplaySettings::default(),
			None,
		);

		// 伪造断开前的那一刻：有画面、有远端光标、指针停在画面里
		let image = egui::ColorImage::example();
		view.texture = Some(ctx.load_texture(
			"daisy-test-frame",
			image.clone(),
			egui::TextureOptions::NEAREST,
		));
		view.cursor_texture =
			Some(ctx.load_texture("daisy-test-cursor", image, egui::TextureOptions::NEAREST));
		view.cursor_px = [16, 16];
		view.cursor_hotspot = [0, 0];
		view.last_pointer = Some((8, 8));
		view.state = ViewState::Ended {
			reason: "测试断开".to_string(),
			acknowledged: false,
			// 这条用例测的是「断开后不能再画远端光标」，前提是曾经有画面
			ever_live: true,
		};

		let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
		let mut input = egui::RawInput {
			screen_rect: Some(screen),
			..Default::default()
		};
		input
			.events
			.push(egui::Event::PointerMoved(screen.center()));

		let mut output = ctx.run_ui(input, |ui| {
			view.paint_frame(ui, false);
		});
		output.textures_delta.clear();

		assert_ne!(
			output.platform_output.cursor_icon,
			egui::CursorIcon::None,
			"冻结的画面不该藏起本机光标，否则用户会觉得鼠标卡住不动"
		);
	}

	/// 测试用的假弹层：更高图层（Middle）里一块 100×100 的悬停区，
	/// 正压在画面中心 (400,300) 上——模拟打开的分辨率下拉。
	fn popup(ctx: &egui::Context) {
		egui::Area::new(egui::Id::new("popup-under-test"))
			.fixed_pos(egui::pos2(350.0, 250.0))
			.show(ctx, |area| {
				area.allocate_exact_size(egui::vec2(100.0, 100.0), egui::Sense::hover());
				area.painter()
					.rect_filled(area.max_rect(), 0.0, egui::Color32::BLUE);
			});
	}

	/// 弹层浮在画面上时要把光标交还本机。
	///
	/// 分辨率下拉这类弹层画在画布上方的独立图层里：远端光标贴着指针画，
	/// 却被压在弹层底下；本机光标又按「指针在画面上」被藏起——
	/// 两头都看不见，用户看到的就是「菜单里没有指针」。
	#[test]
	fn floating_popup_layer_hands_the_cursor_back() {
		let ctx = egui::Context::default();
		let mut params = ConnectionParams::new("127.0.0.1", "daisy", "x");
		params.port = 1;
		let mut view = RdpView::new(
			"弹层".to_string(),
			params,
			None,
			daisy_core::model::DisplaySettings::default(),
			None,
		);

		// 有画面、有远端光标、指针停在画面正中
		let image = egui::ColorImage::new([64, 64], vec![egui::Color32::RED; 64 * 64]);
		view.texture = Some(ctx.load_texture(
			"daisy-test-frame",
			image.clone(),
			egui::TextureOptions::NEAREST,
		));
		view.cursor_texture =
			Some(ctx.load_texture("daisy-test-cursor", image, egui::TextureOptions::NEAREST));
		view.frame_size = [64, 64];
		view.cursor_px = [16, 16];
		view.cursor_hotspot = [0, 0];
		view.last_pointer = Some((32, 32));

		let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
		let pointer = screen.center();
		let mut input = egui::RawInput {
			screen_rect: Some(screen),
			..Default::default()
		};
		input.events.push(egui::Event::PointerMoved(pointer));

		// 第一帧只负责把弹层的 Area 状态登记进 memory（`layer_id_at` 要到
		// 第二帧才查得到）；真实场景里用户把鼠标移到弹层上时早已是第二帧。
		// 被丢弃的帧要清掉纹理增量，否则 epaint 的 drop 守卫会报错。
		let mut first = ctx.run_ui(input.clone(), |ui| {
			popup(ui.ctx());
			view.paint_frame(ui, true);
		});
		first.textures_delta.clear();

		let mut output = ctx.run_ui(input, |ui| {
			popup(ui.ctx());
			view.paint_frame(ui, true);
		});
		output.textures_delta.clear();

		assert_ne!(
			output.platform_output.cursor_icon,
			egui::CursorIcon::None,
			"指针在弹层上时本机光标必须可见——远端光标此刻画在弹层底下"
		);
	}

	/// 同样的布局，指针在弹层**外**的画面上时远端光标照常接管。
	///
	/// 守住「弹层让路」不变成「永远让路」：`layer_id_at` 只查登记过的
	/// Area 层，指针在画布上时返回 `None`，不能误判成「有东西挡着」。
	#[test]
	fn the_canvas_layer_still_takes_over_the_cursor() {
		let ctx = egui::Context::default();
		let mut params = ConnectionParams::new("127.0.0.1", "daisy", "x");
		params.port = 1;
		let mut view = RdpView::new(
			"画布".to_string(),
			params,
			None,
			daisy_core::model::DisplaySettings::default(),
			None,
		);

		let image = egui::ColorImage::new([64, 64], vec![egui::Color32::RED; 64 * 64]);
		view.texture = Some(ctx.load_texture(
			"daisy-test-frame",
			image.clone(),
			egui::TextureOptions::NEAREST,
		));
		view.cursor_texture =
			Some(ctx.load_texture("daisy-test-cursor", image, egui::TextureOptions::NEAREST));
		view.frame_size = [64, 64];
		view.cursor_px = [16, 16];
		view.cursor_hotspot = [0, 0];
		view.last_pointer = Some((32, 32));

		let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
		// 指针在画面正中；弹层摆在角落，不碰指针
		let mut input = egui::RawInput {
			screen_rect: Some(screen),
			..Default::default()
		};
		input
			.events
			.push(egui::Event::PointerMoved(screen.center()));

		let mut output = ctx.run_ui(input, |ui| {
			egui::Area::new(egui::Id::new("popup-under-test"))
				.fixed_pos(egui::pos2(10.0, 10.0))
				.show(ui.ctx(), |area| {
					area.allocate_exact_size(egui::vec2(40.0, 40.0), egui::Sense::hover());
					area.painter()
						.rect_filled(area.max_rect(), 0.0, egui::Color32::BLUE);
				});
			view.paint_frame(ui, true);
		});
		output.textures_delta.clear();

		assert_eq!(
			output.platform_output.cursor_icon,
			egui::CursorIcon::None,
			"指针在画面上（弹层之外）时远端光标照常接管"
		);
	}

	/// 「连不上」不能写成「已断开」。
	///
	/// 用户看到「远程桌面已断开」会以为刚才那条会话被踢掉了（然后去找管理员），
	/// 而实际多半是地址 / 端口 / 凭据写错——提示里的原因才是该看的东西。
	#[test]
	fn a_failed_connect_is_not_reported_as_a_disconnect() {
		assert_eq!(RdpView::ended_title(false), "无法连接远程桌面");
		assert_eq!(RdpView::ended_title(true), "远程桌面已断开");

		// 连不上的会话：状态里必须记住「从没有过画面」
		let ctx = egui::Context::default();
		let mut params = ConnectionParams::new("127.0.0.1", "daisy", "x");
		params.port = 1;
		let mut view = RdpView::new(
			"连不上".to_string(),
			params,
			None,
			daisy_core::model::DisplaySettings::default(),
			None,
		);

		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
		while !matches!(view.state, ViewState::Ended { .. }) && std::time::Instant::now() < deadline
		{
			view.poll(&ctx);
			std::thread::sleep(std::time::Duration::from_millis(20));
		}

		let ViewState::Ended { ever_live, .. } = &view.state else {
			panic!("连不上的会话应进入 Ended");
		};
		assert!(!ever_live, "一次都没出过画面，不该说「已断开」");
	}

	/// 标题已经说了失败，正文不该再来一句同名前缀。
	#[test]
	fn reason_body_drops_the_redundant_prefix() {
		assert_eq!(
			RdpView::reason_body(
				"RDP 连接失败：传输层失败（The connection transport layer failed.）"
			),
			"传输层失败（The connection transport layer failed.）"
		);
		assert_eq!(RdpView::reason_body("网络中断"), "网络中断");
		assert_eq!(RdpView::reason_body(""), "");
	}
}

#[cfg(test)]
mod file_clipboard_tests {
	use super::text_branch_allowed;

	/// 同名但**内容不同**时不覆盖：加序号，各归各的。
	#[test]
	fn downloaded_files_dedupe_names() {
		let dir = tempfile::tempdir().expect("临时目录");
		let base = super::save_into(dir.path(), "报告.txt", b"first").expect("首次落盘");
		assert_eq!(base, dir.path().join("报告.txt"));

		let second = super::save_into(dir.path(), "报告.txt", b"second").expect("二次落盘");
		assert_eq!(second, dir.path().join("报告 (1).txt"), "内容不同才加序号");
		assert_eq!(std::fs::read(&base).unwrap(), b"first", "原文件不能被覆盖");
		assert_eq!(std::fs::read(&second).unwrap(), b"second");
	}

	/// 同名且**内容相同**时复用原文件，不生成 ` (1)`。
	///
	/// 真机反馈：反复复制同一批文件后下载目录里堆出 `hosts (1) (2) (3)`，
	/// 再粘回远端时名字就带着序号——用户以为「粘贴改了文件名」。
	#[test]
	fn identical_content_reuses_the_original_name() {
		let dir = tempfile::tempdir().expect("临时目录");
		let first = super::save_into(dir.path(), "hosts", b"127.0.0.1 localhost\n")
			.expect("首次落盘");
		let again = super::save_into(dir.path(), "hosts", b"127.0.0.1 localhost\n")
			.expect("再次落盘");

		assert_eq!(again, first, "内容一样就该复用同一个文件");
		let count = std::fs::read_dir(dir.path()).unwrap().count();
		assert_eq!(count, 1, "不该多出「hosts (1)」");
	}

	/// 开嗓只在「本地确实没有可传内容」时触发。
	///
	/// 前两版分别用 `files.is_none()` 与 `paths.is_empty()` 判断，都失灵：
	/// 前者因为 `readObjectsForClasses:@[NSURL]` 返回空数组而非 nil，
	/// 后者因为剪贴板会挂着已删除文件的死链接。
	#[test]
	fn arm_only_when_nothing_transferable() {
		assert!(super::should_arm(false, false), "本地没有可传内容必须开嗓");
		assert!(!super::should_arm(true, false), "真有文件要传时不能补空文本");
		assert!(!super::should_arm(false, true), "本轮已上报过就别再补");
	}

	/// 剪贴板上的死链接（文件已被删/移走）要算作「没有内容」。
	///
	/// 真机 16:59:29 那次会话就挂在这种死链接上：`paths` 非空、
	/// 一个都读不出来，开嗓因此被挡住，远端复制从头到尾没反应。
	#[test]
	fn stale_file_links_count_as_nothing() {
		let paths = vec![std::path::PathBuf::from(
			"/nonexistent/daisy/definitely-missing.txt",
		)];
		assert!(
			super::collect_local_files(&paths).is_empty(),
			"不存在的路径必须被剔除，否则会被当成「有文件要传」"
		);
	}

	/// 全是控制字符/空白的「远端内容」不能写进系统剪贴板。
	///
	/// 为了让远端 rdpclip 开始监听，我们会主动上报一条空文本（见
	/// `Clipboard::arm`）；服务端会把它再广播回来，真机上的回波是
	/// 一个控制字符 `"\u{3}"`。原样写进剪贴板等于把用户刚截的图、
	/// 刚复制的文件悄悄清掉。
	#[test]
	fn control_only_remote_text_is_not_synced() {
		assert!(!super::worth_syncing("\u{3}"), "CF_LOCALE 残渣不能落进剪贴板");
		assert!(!super::worth_syncing(""));
		assert!(!super::worth_syncing(" \r\n\t"));
		assert!(super::worth_syncing("b.txt"), "正常文本照旧同步");
		assert!(super::worth_syncing(" 中 "), "带空白但有实义的内容要保留");
	}

	/// 挂剪贴板的落盘**绝不改名**：同名内容不同就存缓存目录，名字照旧。
	///
	/// 真机反馈：远端复制 `b.txt`、mac 上粘贴出来却是 `b (4).txt`。
	/// 根因是落盘走「重名加序号」，序号进了文件名 → 粘出来就带序号，
	/// 再粘一次 Finder 又加一层。
	#[test]
	fn clipboard_save_never_renames() {
		let dir = tempfile::tempdir().expect("临时目录");
		let downloads = dir.path().join("Downloads");
		let cache = dir.path().join("cache");
		std::fs::create_dir_all(&downloads).unwrap();

		// 下载目录里先有一份**内容不同**的同名文件
		std::fs::write(downloads.join("b.txt"), b"old").unwrap();

		let path = super::save_for_clipboard_in(&downloads, &cache, "b.txt", b"new")
			.expect("应能落盘");
		assert_eq!(
			path.file_name().unwrap().to_string_lossy(),
			"b.txt",
			"剪贴板里的文件名必须和远端一致"
		);
		assert!(path.starts_with(&cache), "同名冲突时要落到缓存目录，不覆盖下载里的原文件");
		assert_eq!(
			std::fs::read(downloads.join("b.txt")).unwrap(),
			b"old",
			"下载目录里的原文件不能被覆盖"
		);

		// 内容一致的第二次：复用下载目录里那份，且不再产生副本
		let another = super::save_for_clipboard_in(&downloads, &cache, "c.txt", b"same")
			.expect("应能落盘");
		let again = super::save_for_clipboard_in(&downloads, &cache, "c.txt", b"same")
			.expect("应能落盘");
		assert_eq!(another, again);
		assert_eq!(another.file_name().unwrap().to_string_lossy(), "c.txt");
	}

	/// 远端文件名的路径成分必须被剥掉——只落文件名，不越出目标目录。
	#[test]
	fn downloaded_name_strips_path_components() {
		for evil in ["..\\evil.exe", "/tmp/evil.sh", "..", ".", "a/b.txt"] {
			let name = evil
				.rsplit(['\\', '/'])
				.next()
				.filter(|s| !s.is_empty() && *s != "." && *s != "..")
				.unwrap_or("未命名");
			assert!(
				!name.contains('/') && !name.contains('\\') && name != ".." && name != ".",
				"{evil} 净化后仍带路径成分：{name}"
			);
		}
	}

	/// 剪贴板里是文件时，文本分支必须让路。
	///
	/// Finder 复制文件会**同时**写 `public.file-url` 与
	/// `public.utf8-plain-text`（arboard 读得到文件名）。两条都上报的话，
	/// 后发的文本格式列表把文件格式列表覆盖掉，远端桌面 Ctrl+V 就没反应了。
	#[test]
	fn text_branch_yields_to_files() {
		use crate::pasteboard_files::ClipboardFiles;

		let with_file = Some(ClipboardFiles {
			change_count: 1,
			paths: vec![std::path::PathBuf::from("/tmp/a.txt")],
			skipped: 0,
		});
		assert!(
			!text_branch_allowed(&with_file),
			"剪贴板里是文件时不许再报文本，否则文件列表被覆盖"
		);

		// 全是文件夹（带不过去）同样不报文本：报了远端只会粘出路径字符串
		let folder_only = Some(ClipboardFiles {
			change_count: 2,
			paths: Vec::new(),
			skipped: 1,
		});
		assert!(
			!text_branch_allowed(&folder_only),
			"复制文件夹时不许报文本"
		);

		// 纯文本剪贴板（没读到任何文件对象）照旧
		assert!(text_branch_allowed(&None), "纯文本剪贴板必须仍然上报");

		// readObjects 返回空数组也是纯文本的正常形态
		let empty = Some(ClipboardFiles {
			change_count: 3,
			paths: Vec::new(),
			skipped: 0,
		});
		assert!(text_branch_allowed(&empty), "空文件列表不该挡住文本");
	}
}
