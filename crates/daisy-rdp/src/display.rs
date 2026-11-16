//! S8-64 动态分辨率：`disp`（Display Control）通道。
//!
//! ## 为什么 disp 比 cliprdr 麻烦
//!
//! cliprdr 是**静态通道**：拿着入口函数指针 `freerdp_channels_client_load_ex`
//! 一装就完事。disp 是 **DVC 插件**（`Microsoft::Windows::RDS::DisplayControl`），
//! 必须挂在 `drdynvc` 之下：
//!
//! ```text
//! settings.DynamicChannelArray 里有 "disp"
//!        ↓ drdynvc 通道打开时遍历
//! freerdp_load_channel_addin_entry("disp", …, FREERDP_ADDIN_CHANNEL_DYNAMIC)
//!        ↓ 查 CLIENT_STATIC_ADDIN_TABLE（生成的表里确实有 { "disp", "DVCPluginEntry" }）
//! disp_DVCPluginEntry(&entryPoints)  → 注册 listener
//!        ↓ 服务端接受通道
//! OnChannelConnected(name="Microsoft::Windows::RDS::DisplayControl", pInterface=DispClientContext*)
//! ```
//!
//! 有两处库模式不会自动做，必须自己补：
//!
//! 1. **`freerdp_register_addin_provider`**：它只被 `freerdp_client_context_new`
//!    （`client/common/client.c:240`）调用过。我们不走高层 API，没有 provider 时
//!    `freerdp_load_channel_addin_entry` 会退化成 `dlopen` 找 `.so`——静态链接下必然失败，
//!    表现是「disp 通道静悄悄没注册」，没有任何报错。
//! 2. **加载 drdynvc**：`freerdp_client_load_addins` 的 step 5 会做，我们同样不走它。
//!
//! ## 回调为什么要链式覆盖
//!
//! FreeRDP core 在 `freerdp_channels_attach` 里把
//! `channels->drdynvc->OnChannelConnected` 设成它自己的
//! `freerdp_drdynvc_on_channel_connected`（内部转成 PubSub 事件）。
//! 我们直接覆盖会掐断那条分发；所以先存下原函数指针，在自己的回调里转发一次。
//!
//! `DrdynvcClientContext.custom` 已经被 core 占用（指向 `rdpChannels`），
//! 不能拿来放自己的状态，所以实例走模块级 `ACTIVE`——Daisy 同时只跑一个 RDP 会话。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::error::{RdpError, Result};
use crate::ffi;

/// disp 的 DVC 通道名（`DISP_DVC_CHANNEL_NAME`）。
const DISP_CHANNEL: &std::ffi::CStr = c"Microsoft::Windows::RDS::DisplayControl";

/// `DISPLAY_CONTROL_MONITOR_PRIMARY`：告诉服务端这块是主显示器。
const MONITOR_PRIMARY: u32 = 1;

/// 当前活跃实例。回调只能拿到 `DrdynvcClientContext`，回不到 Rust 侧，
/// 所以走模块级指针。Daisy 一次只有一个 RDP 会话。
static ACTIVE: AtomicPtr<DisplayControl> = AtomicPtr::new(std::ptr::null_mut());

/// 动态分辨率控制句柄。
pub struct DisplayControl {
	/// disp 通道连上后由回调填入。空 = 还没连上。
	disp: AtomicPtr<ffi::DispClientContext>,
	/// drdynvc 上原本的 `OnChannelConnected`（FreeRDP core 设的），链式转发用。
	prev_on_connected: AtomicPtr<c_void>,
	/// drdynvc 接口只取一次。
	bound: AtomicBool,
}

impl Default for DisplayControl {
	fn default() -> Self {
		Self::new()
	}
}

impl std::fmt::Debug for DisplayControl {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("DisplayControl")
			.field("connected", &self.is_connected())
			.finish_non_exhaustive()
	}
}

impl DisplayControl {
	pub fn new() -> Self {
		Self {
			disp: AtomicPtr::new(std::ptr::null_mut()),
			prev_on_connected: AtomicPtr::new(std::ptr::null_mut()),
			bound: AtomicBool::new(false),
		}
	}

	/// disp 通道是否已经连上（可以发分辨率了）。
	pub fn is_connected(&self) -> bool {
		!self.disp.load(Ordering::Acquire).is_null()
	}

	/// 在 `connect` **之前**准备好动态通道。
	///
	/// 和剪贴板同理：`freerdp_connect` 会重建整个 `context->channels`，
	/// 连接后装会拿到 `CHANNEL_RC_ALREADY_CONNECTED`。
	pub fn prepare(&self, instance: &mut ffi::freerdp) -> Result<()> {
		// SAFETY: 引用参数保证 instance 有效；context 由 FreeRDP 持有。
		let context = unsafe { instance.context.as_ref() }
			.ok_or_else(|| RdpError::Gdi("context 为空，无法装 disp 通道".to_string()))?;
		let channels = context.channels;
		let settings = context.settings;
		if channels.is_null() || settings.is_null() {
			return Err(RdpError::Gdi(
				"context->channels/settings 为空，无法装 disp 通道".to_string(),
			));
		}

		// ① 注册 addin provider——库模式下没人做过这件事。
		//
		// **不能直接用 FreeRDP 的 `freerdp_channels_load_static_addin_entry`**：
		// 它引用的 `CLIENT_STATIC_ADDIN_TABLE` 会把每个已编译通道的 .o
		// 一起拖进链接，其中 remdesk 引用了几个没人提供的辅助符号，
		// 结果是整个二进制链接失败。我们自己当 provider，只回答 disp。
		//
		// SAFETY: 符号由 FreeRDP 导出；重复注册只是覆盖函数指针，幂等。
		unsafe {
			ffi::freerdp_register_addin_provider(Some(disp_addin_provider), 0);
		}

		// ② 宣告能力：GCC 报文里带 Display Control，并让 drdynvc 知道要开 disp。
		// SAFETY: settings 非空。
		unsafe {
			ffi::freerdp_settings_set_bool(
				settings,
				ffi::FreeRDP_Settings_Keys_Bool::FreeRDP_SupportDisplayControl,
				1,
			);
			// 名字必须是 DVC 通道名（服务端按这个名字认），不是短名 "disp"。
			let names = [DISP_CHANNEL.as_ptr()];
			let ok = ffi::freerdp_client_add_dynamic_channel(settings, names.len(), names.as_ptr());
			if ok == 0 {
				return Err(RdpError::Gdi("把 disp 加进动态通道表失败".to_string()));
			}
		}

		// ③ 装 drdynvc：disp 的插件入口靠它在通道打开时按名字查出来。
		// SAFETY: channels/settings 非空；入口是库里导出的函数。
		let rc = unsafe {
			ffi::freerdp_channels_client_load_ex(
				channels,
				settings,
				Some(ffi::drdynvc_VirtualChannelEntryEx),
				std::ptr::null_mut(),
			)
		};
		if rc != 0 {
			return Err(RdpError::Gdi(format!("加载 drdynvc 通道失败（返回 {rc}）")));
		}

		// 回调要能回到本实例。
		ACTIVE.store(std::ptr::from_ref(self).cast_mut(), Ordering::Release);
		tracing::info!("[分辨率] drdynvc 已加载，disp 动态通道待服务端打开");
		Ok(())
	}

	/// 接管 drdynvc 的「通道已连接」回调。**每帧调一次**，取到就装。
	///
	/// 返回 `true` 表示已接管。
	pub fn try_bind(&self, instance: &mut ffi::freerdp) -> bool {
		if self.bound.load(Ordering::Acquire) {
			return true;
		}
		// SAFETY: 引用参数保证 instance 有效。
		let Some(context) = (unsafe { instance.context.as_ref() }) else {
			return false;
		};
		let channels = context.channels;
		if channels.is_null() {
			return false;
		}
		// SAFETY: channels 非空；"drdynvc" 是静态通道名。
		let interface = unsafe {
			ffi::freerdp_channels_get_static_channel_interface(channels, c"drdynvc".as_ptr())
		};
		if interface.is_null() {
			// 通道还没建好——正常，等下一帧
			return false;
		}
		// SAFETY: drdynvc 建的就是 DrdynvcClientContext
		// （FreeRDP 自己在 client.c:549 也是这么转的）。
		let drdynvc = unsafe { &mut *(interface as *mut ffi::DrdynvcClientContext) };
		// 链式：先存原回调（core 设为 freerdp_drdynvc_on_channel_connected），
		// 再换成我们的——直接覆盖会掐断 FreeRDP 自己的通道事件分发。
		self.prev_on_connected.store(
			drdynvc
				.OnChannelConnected
				.map_or(std::ptr::null_mut(), |f| f as *mut c_void),
			Ordering::Release,
		);
		drdynvc.OnChannelConnected = Some(on_channel_connected_trampoline);
		self.bound.store(true, Ordering::Release);
		tracing::info!("[分辨率] 已接管 drdynvc 的通道连接回调");
		true
	}

	/// 请求远端把桌面切成 `width × height`。
	///
	/// 布局里的 `DesktopScaleFactor` / `DeviceScaleFactor` **固定 100**：
	/// Windows Server 对这两个字段一律忽略（纯 DPI 变更、随分辨率变更
	/// 一起改、以及 GCC 里的同名字段，2026-11-14/15 全部实测无效），
	/// 而填非 100 还会让服务端把整个布局静默丢弃。会话内放大改由客户端
	/// 兑现（请求更小的桌面 + 本机放大，见 `rdp_view::scaled_size`）。
	///
	/// 通道还没连上时返回错误——UI 侧据此提示「远端还没准备好」，
	/// 而不是静默吞掉（那种"点了没反应"最难查）。
	pub fn set_size(&self, width: u32, height: u32) -> Result<()> {
		let disp = self.disp.load(Ordering::Acquire);
		if disp.is_null() {
			return Err(RdpError::Gdi(
				"disp 通道还没连上，无法调整分辨率".to_string(),
			));
		}
		// SAFETY: disp 是 disp 插件建的上下文，生命周期覆盖会话。
		let Some(send) = (unsafe { (*disp).SendMonitorLayout }) else {
			return Err(RdpError::Gdi("disp 通道没有 SendMonitorLayout".to_string()));
		};

		/// 布局里的缩放字段一律 100（见 `set_size` 的说明）。
		const SCALE: u32 = 100;
		let to_mm = physical_mm;
		let mut layout = ffi::DISPLAY_CONTROL_MONITOR_LAYOUT {
			Flags: MONITOR_PRIMARY,
			Left: 0,
			Top: 0,
			Width: width,
			Height: height,
			PhysicalWidth: to_mm(width),
			PhysicalHeight: to_mm(height),
			Orientation: 0, // 横向
			DesktopScaleFactor: SCALE,
			DeviceScaleFactor: SCALE,
		};
		// SAFETY: disp 非空；layout 在调用期间有效。
		let rc = unsafe { send(disp, 1, &mut layout) };
		if rc != 0 {
			return Err(RdpError::Gdi(format!("发送分辨率请求失败（返回 {rc}）")));
		}
		tracing::info!("[分辨率] 已请求远端切到 {width}×{height}");
		Ok(())
	}
}

impl Drop for DisplayControl {
	fn drop(&mut self) {
		// 别让全局指针悬空——会话结束后回调不该再进来。
		let _ = ACTIVE.compare_exchange(
			std::ptr::from_ref(self).cast_mut(),
			std::ptr::null_mut(),
			Ordering::AcqRel,
			Ordering::Relaxed,
		);
	}
}

/// 像素 → 毫米（按 Windows 的 96 DPI 基准折算）。
///
/// 不能照抄官方客户端的 75 DPI 约定（`xf_disp.c:216`）：真机
/// （Windows Server 2019）实测，物理尺寸必须与布局声称的缩放自洽
/// （缩放恒 100），否则服务端把整个布局静默丢弃。
pub(crate) fn physical_mm(px: u32) -> u32 {
	(f64::from(px) / 96.0 * 25.4).round() as u32
}

/// `FREERDP_ADDIN_CHANNEL_DYNAMIC`（`include/freerdp/addin.h:37`）。
const ADDIN_CHANNEL_DYNAMIC: ffi::DWORD = 0x0000_2000;

/// 我们自己当 addin provider：只回答 disp 这一个动态通道插件。
///
/// FreeRDP 只在自己没找到插件时才用这个 provider；返回空指针表示「不认识」。
///
/// SAFETY: 由 FreeRDP 在 `dvcman_load_addin` 里调用，`psz_name` 是 NUL 结尾串。
unsafe extern "C" fn disp_addin_provider(
	psz_name: ffi::LPCSTR,
	_psz_subsystem: ffi::LPCSTR,
	_psz_type: ffi::LPCSTR,
	flags: ffi::DWORD,
) -> ffi::PVIRTUALCHANNELENTRY {
	if psz_name.is_null() || (flags & ADDIN_CHANNEL_DYNAMIC) == 0 {
		return None;
	}
	// SAFETY: psz_name 由 FreeRDP 传入，NUL 结尾。
	if unsafe { std::ffi::CStr::from_ptr(psz_name) } != DISP_CHANNEL {
		return None;
	}
	// disp 的入口签名是 `UINT(*)(IDRDYNVC_ENTRY_POINTS*)`，与通道入口的
	// 通用函数指针类型不同，但 C 里都只是地址——按地址转（`PVIRTUALCHANNELENTRY`
	// 本身就是 `Option<fn>`，这里一次转到位）。
	let raw: *const c_void = ffi::disp_DVCPluginEntry as *const c_void;
	// SAFETY: 两者都是函数指针，宽度一致（8 字节）。
	unsafe { std::mem::transmute::<*const c_void, ffi::PVIRTUALCHANNELENTRY>(raw) }
}

/// drdynvc 的「通道已连接」回调。
///
/// SAFETY: 由 FreeRDP 在通道连接时调用；`name` 是 NUL 结尾的 C 串。
unsafe extern "C" fn on_channel_connected_trampoline(
	context: *mut ffi::DrdynvcClientContext,
	name: *const std::os::raw::c_char,
	p_interface: *mut c_void,
) -> ffi::UINT {
	let display = ACTIVE.load(Ordering::Acquire);
	let mut rc = 0;
	// SAFETY: name 由 FreeRDP 传入，NUL 结尾。
	if !display.is_null() && !name.is_null() {
		let current = unsafe { &*display };
		let is_disp = unsafe { std::ffi::CStr::from_ptr(name) } == DISP_CHANNEL;
		if is_disp {
			tracing::info!("[分辨率] disp 通道已连接，可以调整分辨率了");
			current.disp.store(
				p_interface.cast::<ffi::DispClientContext>(),
				Ordering::Release,
			);
		}
		// 链式转发给 FreeRDP core 原来的处理（它转成 PubSub 事件）。
		let prev = current.prev_on_connected.load(Ordering::Acquire);
		if !prev.is_null() {
			// SAFETY: prev 是 core 设的同签名函数指针。
			let prev: unsafe extern "C" fn(
				*mut ffi::DrdynvcClientContext,
				*const std::os::raw::c_char,
				*mut c_void,
			) -> ffi::UINT = unsafe { std::mem::transmute(prev) };
			rc = unsafe { prev(context, name, p_interface) };
		}
	}
	rc
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 物理尺寸按 96 DPI 基准折算（布局里的缩放恒 100，必须与之自洽）。
	#[test]
	fn physical_size_uses_96_dpi() {
		assert_eq!(physical_mm(1920), 508);
		assert_eq!(physical_mm(1280), 339);
		assert_eq!(physical_mm(732), 194);
	}

	/// provider 只回答 disp，别的名字一律「不认识」。
	///
	/// 返回非空会让 FreeRDP 拿错误的入口去初始化通道——
	/// 那种错误在运行期表现成莫名其妙的通道崩溃，所以要在这一层挡住。
	#[test]
	fn provider_only_answers_disp_dynamic_channel() {
		// SAFETY: 传进去的都是 static 的 NUL 结尾串。
		let call = |name: &std::ffi::CStr, flags: ffi::DWORD| unsafe {
			disp_addin_provider(name.as_ptr(), std::ptr::null(), std::ptr::null(), flags)
		};
		assert!(
			call(DISP_CHANNEL, ADDIN_CHANNEL_DYNAMIC).is_some(),
			"disp + DYNAMIC 必须给入口"
		);
		assert!(
			call(c"cliprdr", ADDIN_CHANNEL_DYNAMIC).is_none(),
			"别的通道不能碰"
		);
		assert!(
			call(DISP_CHANNEL, 0).is_none(),
			"没有 DYNAMIC 标志时不回答（静态通道走 load_ex，不经 provider）"
		);
		// SAFETY: 空指针是 provider 必须能承受的输入（FreeRDP 可能传 NULL name）
		assert!(
			unsafe {
				disp_addin_provider(
					std::ptr::null(),
					std::ptr::null(),
					std::ptr::null(),
					ADDIN_CHANNEL_DYNAMIC,
				)
			}
			.is_none()
		);
	}

	/// 通道没连上时 `set_size` 必须报错，不能静默吞掉。
	///
	/// 「点了没反应」是最难查的一类问题：界面以为发了，远端什么都没收到。
	#[test]
	fn set_size_before_channel_is_an_error() {
		let display = DisplayControl::new();
		assert!(!display.is_connected());
		assert!(
			display.set_size(1280, 800).is_err(),
			"通道没连上就该报错，让界面能提示"
		);
	}

}
