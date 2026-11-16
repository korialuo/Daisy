//! 终端调色板。
//!
//! [`crate::term::TermColor`] 只描述语义（默认前景、索引色、真彩色），
//! 具体显示成什么颜色由这里决定。换主题就是换一个 [`Palette`]。
//!
//! 索引色 16–255 走 xterm 的标准表，不需要逐条列举：
//!
//! - 16–231：6×6×6 色立方，分量映射为 `0` 或 `55 + 40 × v`
//! - 232–255：24 级灰阶，`8 + 10 × n`

use egui::Color32;

use crate::term::TermColor;

/// 标准 xterm 16 色的默认取值（`0`–`15`）。
///
/// 顺序与 ANSI 一致：黑、红、绿、黄、蓝、品红、青、白，然后是高亮版本。
const ANSI_FALLBACK: [Color32; 16] = [
	Color32::from_rgb(0x00, 0x00, 0x00),
	Color32::from_rgb(0xCD, 0x00, 0x00),
	Color32::from_rgb(0x00, 0xCD, 0x00),
	Color32::from_rgb(0xCD, 0xCD, 0x00),
	Color32::from_rgb(0x00, 0x00, 0xEE),
	Color32::from_rgb(0xCD, 0x00, 0xCD),
	Color32::from_rgb(0x00, 0xCD, 0xCD),
	Color32::from_rgb(0xE5, 0xE5, 0xE5),
	Color32::from_rgb(0x7F, 0x7F, 0x7F),
	Color32::from_rgb(0xFF, 0x00, 0x00),
	Color32::from_rgb(0x00, 0xFF, 0x00),
	Color32::from_rgb(0xFF, 0xFF, 0x00),
	Color32::from_rgb(0x5C, 0x5C, 0xFF),
	Color32::from_rgb(0xFF, 0x00, 0xFF),
	Color32::from_rgb(0x00, 0xFF, 0xFF),
	Color32::from_rgb(0xFF, 0xFF, 0xFF),
];

/// 终端配色。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
	/// 默认前景色。
	pub foreground: Color32,
	/// 默认背景色。
	pub background: Color32,
	/// 光标颜色。
	pub cursor: Color32,
	/// 光标覆盖字符时的文字颜色。
	pub cursor_text: Color32,
	/// 选区高亮色。
	pub selection: Color32,
	/// ANSI 0–15 命名色。
	pub ansi: [Color32; 16],
}

impl Default for Palette {
	fn default() -> Self {
		Self::daisy_dark()
	}
}

impl Palette {
	/// Daisy 默认深色配色。
	#[must_use]
	pub fn daisy_dark() -> Self {
		Self {
			foreground: Color32::from_rgb(0xD8, 0xD8, 0xD8),
			background: Color32::from_rgb(0x14, 0x16, 0x1A),
			cursor: Color32::from_rgb(0xE6, 0xB4, 0x50),
			cursor_text: Color32::from_rgb(0x14, 0x16, 0x1A),
			selection: Color32::from_rgb(0x2D, 0x4A, 0x6B),
			ansi: ANSI_FALLBACK,
		}
	}

	/// Daisy 浅色配色。
	///
	/// ANSI 0–15 仍走标准表——终端内容里显式指定颜色的部分不该随主题变，
	/// 否则同一个 `ls --color` 输出在深浅主题下颜色不同，反而难认。
	/// 只有默认前景/背景/光标/选区这四个「容器色」随主题翻转。
	#[must_use]
	pub fn daisy_light() -> Self {
		Self {
			foreground: Color32::from_rgb(0x1F, 0x23, 0x28),
			background: Color32::from_rgb(0xFA, 0xFA, 0xFA),
			cursor: Color32::from_rgb(0x0A, 0x66, 0xC2),
			cursor_text: Color32::from_rgb(0xFF, 0xFF, 0xFF),
			selection: Color32::from_rgb(0xCD, 0xE3, 0xF7),
			ansi: ANSI_FALLBACK,
		}
	}

	/// 把语义颜色解析成实际颜色。
	///
	/// `default` 用于兜底：当 `TermColor` 表示的是「默认」但方向相反时
	/// （例如反显之后的背景），由调用方决定拿前景还是背景顶上。
	#[must_use]
	pub fn resolve(&self, color: TermColor, default: Color32) -> Color32 {
		match color {
			TermColor::DefaultForeground | TermColor::DefaultBackground => default,
			TermColor::Cursor => self.cursor,
			TermColor::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
			TermColor::Indexed(index) => {
				if index < 16 {
					self.ansi[usize::from(index)]
				} else {
					xterm_256(index)
				}
			},
		}
	}

	/// 前景色。
	#[must_use]
	pub fn fg(&self, color: TermColor) -> Color32 {
		self.resolve(color, self.foreground)
	}

	/// 背景色。
	#[must_use]
	pub fn bg(&self, color: TermColor) -> Color32 {
		self.resolve(color, self.background)
	}
}

/// xterm 256 色表中 16 以上的部分。
///
/// 16–231 是 6×6×6 色立方，232–255 是灰阶。两者都是纯计算，无需查表。
#[must_use]
pub fn xterm_256(index: u8) -> Color32 {
	match index {
		0..=15 => ANSI_FALLBACK[usize::from(index)],
		16..=231 => {
			let offset = index - 16;
			let r = offset / 36;
			let g = (offset % 36) / 6;
			let b = offset % 6;
			Color32::from_rgb(cube_level(r), cube_level(g), cube_level(b))
		},
		_ => {
			let level = 8 + 10 * (index - 232);
			Color32::from_gray(level)
		},
	}
}

/// 色立方某一分量的取值：0 保持全黑，其余从 95 起每级 +40。
fn cube_level(value: u8) -> u8 {
	if value == 0 { 0 } else { 55 + 40 * value }
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_palette_is_dark() {
		let palette = Palette::daisy_dark();
		// 背景亮度应明显低于前景
		let bg = palette.background;
		let fg = palette.foreground;
		let bg_sum = u32::from(bg.r()) + u32::from(bg.g()) + u32::from(bg.b());
		let fg_sum = u32::from(fg.r()) + u32::from(fg.g()) + u32::from(fg.b());
		assert!(bg_sum < fg_sum, "深色主题的背景应比前景暗");
	}

	#[test]
	fn light_palette_is_light() {
		let palette = Palette::daisy_light();
		let bg_sum = u32::from(palette.background.r())
			+ u32::from(palette.background.g())
			+ u32::from(palette.background.b());
		let fg_sum = u32::from(palette.foreground.r())
			+ u32::from(palette.foreground.g())
			+ u32::from(palette.foreground.b());
		assert!(bg_sum > fg_sum, "浅色主题的背景应比前景亮");
	}

	#[test]
	fn light_and_dark_share_the_ansi_table() {
		// 内容显式指定的颜色不随主题翻转，否则同一输出在两种主题下难认
		assert_eq!(Palette::daisy_light().ansi, Palette::daisy_dark().ansi);
	}

	#[test]
	fn rgb_passes_through() {
		let palette = Palette::daisy_dark();
		assert_eq!(
			palette.fg(TermColor::Rgb(1, 2, 3)),
			Color32::from_rgb(1, 2, 3)
		);
		assert_eq!(
			palette.bg(TermColor::Rgb(9, 8, 7)),
			Color32::from_rgb(9, 8, 7)
		);
	}

	#[test]
	fn default_uses_fallback() {
		let palette = Palette::daisy_dark();
		assert_eq!(palette.fg(TermColor::DefaultForeground), palette.foreground);
		assert_eq!(palette.bg(TermColor::DefaultBackground), palette.background);
	}

	#[test]
	fn named_indexes_come_from_ansi_table() {
		let palette = Palette::daisy_dark();
		// 索引 1 是红
		assert_eq!(palette.fg(TermColor::Indexed(1)), palette.ansi[1]);
		// 索引 9 是亮红
		assert_eq!(palette.fg(TermColor::Indexed(9)), palette.ansi[9]);
	}

	#[test]
	fn cube_corners_match_xterm_spec() {
		// 16 = 全黑，231 = 全白，这是色立方的两个角
		assert_eq!(xterm_256(16), Color32::from_rgb(0, 0, 0));
		assert_eq!(xterm_256(231), Color32::from_rgb(255, 255, 255));
	}

	#[test]
	fn cube_max_component_level_is_255() {
		// 5 级映射到 55 + 40*5 = 255
		assert_eq!(cube_level(5), 255);
		assert_eq!(cube_level(1), 95);
		assert_eq!(cube_level(0), 0);
	}

	#[test]
	fn cube_pure_red() {
		// 196 = 16 + 36*5，即 r 分量拉满
		assert_eq!(xterm_256(196), Color32::from_rgb(255, 0, 0));
	}

	#[test]
	fn cube_pure_blue() {
		// 21 = 16 + 5，即 b 分量拉满
		assert_eq!(xterm_256(21), Color32::from_rgb(0, 0, 255));
	}

	#[test]
	fn gray_ramp_endpoints() {
		// 232 是最暗的灰，255 是最亮的灰
		assert_eq!(xterm_256(232), Color32::from_gray(8));
		assert_eq!(xterm_256(255), Color32::from_gray(8 + 10 * 23));
	}

	#[test]
	fn gray_ramp_is_monotonic() {
		for index in 232..255 {
			let current = xterm_256(index).r();
			let next = xterm_256(index + 1).r();
			assert!(
				next > current,
				"灰阶应单调递增：{index} -> {next} vs {current}"
			);
		}
	}

	#[test]
	fn low_indexes_fall_back_to_ansi() {
		for index in 0..16u8 {
			assert_eq!(xterm_256(index), ANSI_FALLBACK[usize::from(index)]);
		}
	}

	#[test]
	fn full_index_range_does_not_panic() {
		let palette = Palette::daisy_dark();
		for index in 0..=255u8 {
			let _ = palette.fg(TermColor::Indexed(index));
		}
	}

	#[test]
	fn cursor_color_is_distinct() {
		let palette = Palette::daisy_dark();
		assert_ne!(palette.cursor, palette.background);
		assert_eq!(palette.fg(TermColor::Cursor), palette.cursor);
	}
}
