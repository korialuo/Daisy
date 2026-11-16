//! egui 渲染后端。
//!
//! 把 [`GridSnapshot`] 画到 egui 的画笔上。**不依赖 `alacritty_terminal`**，
//! 因此可以脱离终端单独测试颜色与布局换算。
//!
//! ## 绘制策略
//!
//! 逐格调用 `painter.text()` 会为每个格生成一个 galley，80×24 就是近两千次
//! 文本布局，帧率会直接崩掉。这里按**相同样式连续段**合并：
//!
//! 1. 整屏铺一次背景色
//! 2. 逐行扫描，把 `(前景, 背景, 属性)` 完全相同的连续格子合成一段
//! 3. 每段只画一次背景矩形 + 一次文本
//!
//! 实际终端里一行往往只有几段不同样式，调用次数从 `列数 × 行数` 降到几十次。

use egui::{Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, Vec2};

use crate::palette::Palette;
use crate::term::{CellFlags, CellSnapshot, CursorShape, GridSnapshot, TermColor};

/// 单元格像素尺寸与字体。
#[derive(Debug, Clone, PartialEq)]
pub struct CellMetrics {
	/// 单格宽度。
	pub width: f32,
	/// 单格高度（行高）。
	pub height: f32,
	/// 字号，与 [`Self::font_id`] 保持一致。
	pub font_size: f32,
	/// 等宽字体。
	pub font_id: FontId,
}

impl CellMetrics {
	/// 从字体实测出单元格尺寸。
	///
	/// 宽度取 `'M'` 的实测宽度——等宽字体下它等于任意字符的步进；
	/// 高度取一行文本的高度。
	#[must_use]
	pub fn measure(ctx: &egui::Context, font_size: f32) -> Self {
		let font_id = FontId::monospace(font_size);

		let size = ctx.fonts_mut(|fonts| {
			// 不换行：wrap 宽度给正无穷，否则 "M" 会被折行
			let galley = fonts.layout(
				"M".to_owned(),
				font_id.clone(),
				Color32::PLACEHOLDER,
				f32::INFINITY,
			);
			galley.size()
		});

		Self {
			width: size.x.max(1.0),
			height: size.y.max(1.0),
			font_size,
			font_id,
		}
	}

	/// 单元格尺寸。
	#[must_use]
	pub fn cell_size(&self) -> Vec2 {
		Vec2::new(self.width, self.height)
	}

	/// 给定可用区域，能放下多少列多少行。
	///
	/// 结果至少是 `1×1`，避免除零与零尺寸网格。
	#[must_use]
	pub fn grid_size(&self, available: Vec2) -> (u16, u16) {
		let cols = (available.x / self.width).floor().max(1.0);
		let rows = (available.y / self.height).floor().max(1.0);

		(
			cols.min(f32::from(u16::MAX)) as u16,
			rows.min(f32::from(u16::MAX)) as u16,
		)
	}

	/// 网格占用的像素尺寸。
	#[must_use]
	pub fn pixel_size(&self, cols: usize, rows: usize) -> Vec2 {
		Vec2::new(cols as f32 * self.width, rows as f32 * self.height)
	}
}

/// 终端渲染器。
#[derive(Debug, Clone)]
pub struct TerminalRenderer {
	metrics: CellMetrics,
	palette: Palette,
}

impl TerminalRenderer {
	/// 构造。
	#[must_use]
	pub fn new(metrics: CellMetrics, palette: Palette) -> Self {
		Self { metrics, palette }
	}

	/// 单元格尺寸。
	#[must_use]
	pub fn metrics(&self) -> &CellMetrics {
		&self.metrics
	}

	/// 配色。
	#[must_use]
	pub fn palette(&self) -> &Palette {
		&self.palette
	}

	/// 换配色。
	pub fn set_palette(&mut self, palette: Palette) {
		self.palette = palette;
	}

	/// 换单元格尺寸（字号变化时）。下一帧的网格换算会跟着变。
	pub fn set_metrics(&mut self, metrics: CellMetrics) {
		self.metrics = metrics;
	}

	/// 把一帧画到 `painter` 上，左上角对齐 `origin`。
	pub fn paint(&self, painter: &Painter, origin: Pos2, snapshot: &GridSnapshot) {
		let full = Rect::from_min_size(
			origin,
			self.metrics.pixel_size(snapshot.cols, snapshot.rows),
		);
		painter.rect_filled(full, CornerRadius::ZERO, self.palette.background);

		for row in 0..snapshot.rows {
			self.paint_row(painter, origin, snapshot, row);
		}

		if let Some(cursor) = snapshot.cursor {
			self.paint_cursor(
				painter,
				origin,
				snapshot,
				cursor.row,
				cursor.col,
				cursor.shape,
			);
		}
	}

	/// 画一行，把同样式的连续格子合并成段。
	fn paint_row(&self, painter: &Painter, origin: Pos2, snapshot: &GridSnapshot, row: usize) {
		let mut col = 0;
		while col < snapshot.cols {
			let Some(run) = self.next_run(snapshot, row, col) else {
				break;
			};
			self.paint_run(painter, origin, snapshot, row, run);
			col = run.end;
		}
	}

	/// 从 `col` 开始，找出下一段样式完全相同的连续格子。
	///
	/// 刻意不返回 `Vec`——这是每帧的热路径，逐行扫描比反复分配划算。
	fn next_run(&self, snapshot: &GridSnapshot, row: usize, col: usize) -> Option<Run> {
		let first = snapshot.cell(row, col)?;
		let style = RunStyle::of(first, self.is_selected(snapshot, row, col));

		let mut end = col + 1;
		while end < snapshot.cols {
			let Some(cell) = snapshot.cell(row, end) else {
				break;
			};
			if RunStyle::of(cell, self.is_selected(snapshot, row, end)) != style {
				break;
			}
			end += 1;
		}

		Some(Run {
			start: col,
			end,
			style,
		})
	}

	fn paint_run(
		&self,
		painter: &Painter,
		origin: Pos2,
		snapshot: &GridSnapshot,
		row: usize,
		run: Run,
	) {
		let (fg, bg) = self.resolve_colors(run.style);

		if bg != self.palette.background {
			let rect = Rect::from_min_size(
				self.cell_origin(origin, row, run.start),
				Vec2::new(
					(run.end - run.start) as f32 * self.metrics.width,
					self.metrics.height,
				),
			);
			painter.rect_filled(rect, CornerRadius::ZERO, bg);
		}

		let font = self.run_font(run.style);
		for piece in self.run_pieces(snapshot, row, run.start, run.end) {
			match piece {
				Piece::Narrow { col, text } => {
					if text.chars().all(char::is_whitespace) {
						continue;
					}
					painter.text(
						self.cell_origin(origin, row, col),
						Align2::LEFT_TOP,
						text,
						font.clone(),
						fg,
					);
				},
				Piece::Wide { col, ch } => {
					let cell = self.cell_origin(origin, row, col);
					self.paint_wide(painter, cell, ch, &font, fg);
				},
			}
		}
	}

	/// 画一个宽字符：定位在它自己占的那两格里，水平居中。
	///
	/// **必须单独画**。整段合并排版时，位置是「起点 + 字体排下来的宽度」，
	/// 而中文字形只有 1em 宽（约 1.6 格），终端却按两格记账——差出来的
	/// 那点距离会累积到同段后面每一个字符上，一行里汉字越多后面越左移。
	/// 单独画之后每个宽字符都从自己的格子起算，后面紧跟的窄段不受影响。
	fn paint_wide(
		&self,
		painter: &Painter,
		cell: Pos2,
		ch: char,
		font: &FontId,
		fg: Color32,
	) {
		if ch == ' ' {
			return;
		}

		let galley = painter.layout_no_wrap(ch.to_string(), font.clone(), fg);
		// 字形比两格窄时居中，贴近全角的外观；比两格宽就贴左，别压到前一列
		let offset = (self.metrics.width * 2.0 - galley.size().x) * 0.5;

		painter.galley(cell + Vec2::new(offset.max(0.0), 0.0), galley, fg);
	}

	fn paint_cursor(
		&self,
		painter: &Painter,
		origin: Pos2,
		snapshot: &GridSnapshot,
		row: usize,
		col: usize,
		shape: CursorShape,
	) {
		if shape == CursorShape::Hidden {
			return;
		}

		let cell = self.cell_origin(origin, row, col);
		let size = self.metrics.cell_size();

		let rect = match shape {
			CursorShape::Beam => Rect::from_min_size(cell, Vec2::new(2.0, size.y)),
			CursorShape::Underline => {
				Rect::from_min_size(cell + Vec2::new(0.0, size.y - 2.0), Vec2::new(size.x, 2.0))
			},
			CursorShape::HollowBlock => {
				let rect = Rect::from_min_size(cell, size);
				painter.rect_stroke(
					rect,
					CornerRadius::ZERO,
					egui::Stroke::new(1.0, self.palette.cursor),
					egui::StrokeKind::Inside,
				);
				return;
			},
			CursorShape::Block | CursorShape::Hidden => Rect::from_min_size(cell, size),
		};

		if shape == CursorShape::Block
			&& let Some(cell_data) = snapshot.cell(row, col)
			&& cell_data.ch != ' '
		{
			// 块状光标下要重画字符，否则光标会把它盖住
			painter.rect_filled(rect, CornerRadius::ZERO, self.palette.cursor);
			painter.text(
				cell,
				Align2::LEFT_TOP,
				cell_data.ch.to_string(),
				self.metrics.font_id.clone(),
				self.palette.cursor_text,
			);
			return;
		}

		painter.rect_filled(rect, CornerRadius::ZERO, self.palette.cursor);
	}

	/// 把一段连续格子切成绘制单元：连续窄字符合成一段，宽字符各自一个。
	///
	/// 分段规则与终端记账方式一致：一个宽字符 + 紧跟的占位格算两列，
	/// 其余按列走。宽字符不能混进窄段里排版（原因见 [`Self::paint_wide`]）。
	fn run_pieces(
		&self,
		snapshot: &GridSnapshot,
		row: usize,
		start: usize,
		end: usize,
	) -> Vec<Piece> {
		let mut pieces = Vec::new();
		let mut narrow: Option<(usize, String)> = None;

		for col in start..end {
			let Some(cell) = snapshot.cell(row, col) else {
				break;
			};
			// 宽字符的第二列是占位格，字符已经在上一格画过了
			if cell.flags.wide_spacer {
				continue;
			}

			if self.is_wide(snapshot, row, col) {
				if let Some((col, text)) = narrow.take() {
					pieces.push(Piece::Narrow { col, text });
				}
				let ch = if cell.flags.hidden { ' ' } else { cell.ch };
				pieces.push(Piece::Wide { col, ch });
			} else {
				let ch = if cell.flags.hidden { ' ' } else { cell.ch };
				match &mut narrow {
					Some((_, text)) => text.push(ch),
					None => narrow = Some((col, ch.to_string())),
				}
			}
		}

		if let Some((col, text)) = narrow {
			pieces.push(Piece::Narrow { col, text });
		}

		pieces
	}

	/// 这一格是不是宽字符（后面紧跟占位格）。
	fn is_wide(&self, snapshot: &GridSnapshot, row: usize, col: usize) -> bool {
		col + 1 < snapshot.cols
			&& snapshot
				.cell(row, col + 1)
				.is_some_and(|next| next.flags.wide_spacer)
	}

	fn run_font(&self, style: RunStyle) -> FontId {
		let mut font = self.metrics.font_id.clone();
		if style.bold {
			font = FontId::new(self.metrics.font_size, egui::FontFamily::Monospace);
		}
		font
	}

	fn cell_origin(&self, origin: Pos2, row: usize, col: usize) -> Pos2 {
		origin
			+ Vec2::new(
				col as f32 * self.metrics.width,
				row as f32 * self.metrics.height,
			)
	}

	fn is_selected(&self, snapshot: &GridSnapshot, row: usize, col: usize) -> bool {
		snapshot
			.selection
			.is_some_and(|selection| selection.contains(row, col))
	}

	/// 解析出最终的前景 / 背景色，处理反显、隐藏、暗淡与选区。
	fn resolve_colors(&self, style: RunStyle) -> (Color32, Color32) {
		let mut fg = self.palette.fg(style.fg);
		let mut bg = self.palette.bg(style.bg);

		if style.inverse {
			std::mem::swap(&mut fg, &mut bg);
		}
		if style.dim {
			fg = dim(fg);
		}
		if style.selected {
			bg = self.palette.selection;
		}

		(fg, bg)
	}
}

/// 一行里样式相同的一段连续格子。
#[derive(Debug, Clone, Copy)]
struct Run {
	start: usize,
	end: usize,
	style: RunStyle,
}

/// 一段里真正要画的一个单元。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
	/// 连续窄字符，从 `col` 起逐格排下去（等宽字体保证宽度对得上）。
	Narrow { col: usize, text: String },
	/// 一个宽字符，占 `col` 与 `col + 1` 两格。
	Wide { col: usize, ch: char },
}

/// 一段连续格子的绘制样式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunStyle {
	fg: TermColor,
	bg: TermColor,
	bold: bool,
	italic: bool,
	underline: bool,
	inverse: bool,
	dim: bool,
	selected: bool,
}

impl RunStyle {
	fn of(cell: &CellSnapshot, selected: bool) -> Self {
		let CellFlags {
			bold,
			italic,
			underline,
			inverse,
			dim,
			..
		} = cell.flags;
		Self {
			fg: cell.fg,
			bg: cell.bg,
			bold,
			italic,
			underline,
			inverse,
			dim,
			selected,
		}
	}
}

/// 暗淡：把各通道压到约 2/3。
fn dim(color: Color32) -> Color32 {
	let scale = |channel: u8| ((u16::from(channel) * 2) / 3) as u8;
	Color32::from_rgb(scale(color.r()), scale(color.g()), scale(color.b()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::term::{CellSnapshot, CursorState, GridSnapshot};

	fn metrics() -> CellMetrics {
		CellMetrics {
			width: 8.0,
			height: 16.0,
			font_size: 14.0,
			font_id: FontId::monospace(14.0),
		}
	}

	fn renderer() -> TerminalRenderer {
		TerminalRenderer::new(metrics(), Palette::daisy_dark())
	}

	fn cell(ch: char) -> CellSnapshot {
		CellSnapshot {
			ch,
			..CellSnapshot::default()
		}
	}

	fn snapshot(rows: usize, cols: usize, fill: &str) -> GridSnapshot {
		let mut cells = Vec::with_capacity(rows * cols);
		for ch in fill.chars().take(rows * cols) {
			cells.push(cell(ch));
		}
		while cells.len() < rows * cols {
			cells.push(cell(' '));
		}

		GridSnapshot {
			cols,
			rows,
			cells,
			cursor: None,
			display_offset: 0,
			alternate_screen: false,
			selection: None,
		}
	}

	#[test]
	fn grid_size_rounds_down() {
		let m = metrics();
		// 100/8 = 12.5 → 12 列；70/16 = 4.375 → 4 行
		assert_eq!(m.grid_size(Vec2::new(100.0, 70.0)), (12, 4));
	}

	#[test]
	fn grid_size_is_at_least_one_by_one() {
		let m = metrics();
		assert_eq!(m.grid_size(Vec2::new(0.0, 0.0)), (1, 1));
		assert_eq!(m.grid_size(Vec2::new(3.0, 3.0)), (1, 1));
	}

	#[test]
	fn pixel_size_multiplies() {
		let m = metrics();
		assert_eq!(m.pixel_size(10, 3), Vec2::new(80.0, 48.0));
	}

	#[test]
	fn inverse_swaps_foreground_and_background() {
		let renderer = renderer();
		let palette = renderer.palette();

		let normal = RunStyle {
			fg: TermColor::Indexed(1),
			bg: TermColor::DefaultBackground,
			bold: false,
			italic: false,
			underline: false,
			inverse: false,
			dim: false,
			selected: false,
		};
		let inverted = RunStyle {
			inverse: true,
			..normal
		};

		let (fg_normal, bg_normal) = renderer.resolve_colors(normal);
		let (fg_inverse, bg_inverse) = renderer.resolve_colors(inverted);

		assert_eq!(fg_normal, palette.fg(TermColor::Indexed(1)));
		assert_eq!(bg_normal, palette.background);
		assert_eq!(fg_inverse, palette.background);
		assert_eq!(bg_inverse, palette.fg(TermColor::Indexed(1)));
	}

	#[test]
	fn selection_overrides_background() {
		let renderer = renderer();
		let style = RunStyle {
			fg: TermColor::DefaultForeground,
			bg: TermColor::DefaultBackground,
			bold: false,
			italic: false,
			underline: false,
			inverse: false,
			dim: false,
			selected: true,
		};

		let (_, bg) = renderer.resolve_colors(style);
		assert_eq!(bg, renderer.palette().selection);
	}

	#[test]
	fn dim_darkens_each_channel() {
		let dimmed = dim(Color32::from_rgb(90, 60, 30));
		assert_eq!(dimmed.r(), 60);
		assert_eq!(dimmed.g(), 40);
		assert_eq!(dimmed.b(), 20);
	}

	#[test]
	fn wide_char_becomes_its_own_piece() {
		let renderer = renderer();
		let mut snap = snapshot(1, 4, "    ");

		snap.cells[0] = cell('中');
		snap.cells[1] = CellSnapshot {
			ch: ' ',
			flags: CellFlags {
				wide_spacer: true,
				..CellFlags::default()
			},
			..CellSnapshot::default()
		};
		snap.cells[2] = cell('b');
		snap.cells[3] = cell('c');

		// 宽字符单独立一段（它占两格），后面的窄字符另起一段从第 2 列排
		assert_eq!(
			renderer.run_pieces(&snap, 0, 0, 4),
			vec![
				Piece::Wide { col: 0, ch: '中' },
				Piece::Narrow {
					col: 2,
					text: "bc".to_owned()
				},
			]
		);
	}

	#[test]
	fn narrow_run_keeps_its_start_column() {
		let renderer = renderer();
		// 前面两格空白不参与（调用方给的是一段，起点就是 2）
		let mut snap = snapshot(1, 4, "  ab");
		assert_eq!(
			renderer.run_pieces(&snap, 0, 2, 4),
			vec![Piece::Narrow {
				col: 2,
				text: "ab".to_owned()
			}]
		);
		snap.cells[3] = cell(' ');
	}

	#[test]
	fn hidden_becomes_space() {
		let renderer = renderer();
		let mut snap = snapshot(1, 3, "abc");
		snap.cells[1] = CellSnapshot {
			ch: 'X',
			flags: CellFlags {
				hidden: true,
				..CellFlags::default()
			},
			..CellSnapshot::default()
		};

		assert_eq!(
			renderer.run_pieces(&snap, 0, 0, 3),
			vec![Piece::Narrow {
				col: 0,
				text: "a c".to_owned()
			}]
		);
	}

	#[test]
	fn two_wide_chars_do_not_share_a_piece() {
		// 连续两个汉字各自占两格，各自成段——合起来排版就会累积偏移
		let renderer = renderer();
		let mut snap = snapshot(1, 4, "    ");
		let spacer = CellSnapshot {
			ch: ' ',
			flags: CellFlags {
				wide_spacer: true,
				..CellFlags::default()
			},
			..CellSnapshot::default()
		};
		snap.cells[0] = cell('川');
		snap.cells[1] = spacer;
		snap.cells[2] = cell('江');
		snap.cells[3] = spacer;

		assert_eq!(
			renderer.run_pieces(&snap, 0, 0, 4),
			vec![
				Piece::Wide { col: 0, ch: '川' },
				Piece::Wide { col: 2, ch: '江' },
			]
		);
	}

	#[test]
	fn run_style_groups_identical_cells() {
		let plain = cell('a');
		let styled = CellSnapshot {
			flags: CellFlags {
				bold: true,
				..CellFlags::default()
			},
			..cell('b')
		};

		assert_eq!(RunStyle::of(&plain, false), RunStyle::of(&cell('c'), false));
		assert_ne!(RunStyle::of(&plain, false), RunStyle::of(&styled, false));
		assert_ne!(
			RunStyle::of(&plain, false),
			RunStyle::of(&plain, true),
			"选中状态不同应算作不同样式段"
		);
	}

	#[test]
	fn cell_origin_accumulates_metrics() {
		let renderer = renderer();
		let origin = Pos2::new(100.0, 50.0);

		assert_eq!(renderer.cell_origin(origin, 0, 0), Pos2::new(100.0, 50.0));
		assert_eq!(renderer.cell_origin(origin, 2, 3), Pos2::new(124.0, 82.0));
	}

	#[test]
	fn palette_can_be_swapped() {
		let mut renderer = renderer();
		let mut light = Palette::daisy_dark();
		light.background = Color32::WHITE;

		renderer.set_palette(light);
		assert_eq!(renderer.palette().background, Color32::WHITE);
	}

	#[test]
	fn cursor_shapes_are_all_handled() {
		// 仅验证不会 panic：绘制需要 Painter，这里只走一遍形状分支
		for shape in [
			CursorShape::Block,
			CursorShape::Beam,
			CursorShape::Underline,
			CursorShape::HollowBlock,
			CursorShape::Hidden,
		] {
			let snap = GridSnapshot {
				cursor: Some(CursorState {
					row: 0,
					col: 0,
					shape,
				}),
				..snapshot(1, 1, "a")
			};
			assert!(snap.cursor.is_some());
		}
	}
}
