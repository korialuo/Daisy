//! 主题与外观应用。
//!
//! 把 [`AppConfig`] 里的偏好落成三件事：
//!
//! 1. egui 的 `Visuals`（控件配色）
//! 2. 界面缩放（`pixels_per_point`，相对系统 DPI 的**倍数**）
//! 3. 终端渲染器的 [`Palette`] 与字号
//!
//! 「跟随系统」在每次应用时重新探测——用户切换 macOS 外观后，
//! 下次改动任何设置都会跟着变，不需要重启。

use daisy_core::config::Theme;
use daisy_term::Palette;
use egui::{Context, Theme as EguiTheme};

/// 解析后的深浅模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
	/// 浅色。
	Light,
	/// 深色。
	Dark,
}

impl From<ColorMode> for crate::design::Mode {
	fn from(mode: ColorMode) -> Self {
		match mode {
			ColorMode::Light => Self::Light,
			ColorMode::Dark => Self::Dark,
		}
	}
}

impl ColorMode {
	/// 按偏好与系统当前主题求解。系统主题不可知时按深色处理
	/// （Daisy 的默认观感就是深色）。
	pub fn resolve(preference: Theme, ctx: &Context) -> Self {
		match preference {
			Theme::Light => Self::Light,
			Theme::Dark => Self::Dark,
			Theme::System => match ctx.system_theme() {
				Some(EguiTheme::Light) => Self::Light,
				Some(EguiTheme::Dark) => Self::Dark,
				// 拿不到系统主题（极少数平台/远程桌面）：回退深色
				_ => Self::Dark,
			},
		}
	}

	/// 终端配色。ANSI 16 色两套共用——显式指定颜色的终端内容
	/// 不该随主题翻转，否则同一输出在两种主题下颜色不同。
	#[must_use]
	pub fn terminal_palette(self) -> Palette {
		match self {
			Self::Light => Palette::daisy_light(),
			Self::Dark => Palette::daisy_dark(),
		}
	}
}

/// 启动时记下的系统像素密度基准。
///
/// `pixels_per_point` 是**绝对值**（含系统 DPI），而配置里的 `ui_scale`
/// 是相对倍率。两者相乘才是目标值；不记基准的话每次设置都会在已缩放
/// 的值上继续乘，调两次 1.1 就变成 1.21。
#[derive(Debug, Clone, Copy)]
pub struct ScaleBase(f32);

impl ScaleBase {
	/// 用指定基准构造。占位值（等 ctx 就绪前）用 1.0。
	#[must_use]
	pub fn new(base: f32) -> Self {
		Self(if base.is_finite() && base > 0.0 {
			base
		} else {
			1.0
		})
	}

	/// 记录当前值作为基准。必须在任何 `set_pixels_per_point` 之前取。
	#[must_use]
	pub fn capture(ctx: &Context) -> Self {
		Self::new(ctx.pixels_per_point())
	}

	/// 按倍率算出目标像素密度。
	#[must_use]
	pub fn scaled(self, scale: f32) -> f32 {
		(self.0 * scale).max(0.1)
	}
}
