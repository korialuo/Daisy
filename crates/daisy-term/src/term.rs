//! 终端网格。
//!
//! 把 PTY 输出的字节流喂给 `alacritty_terminal`，由它处理 ANSI/VT 转义序列，
//! 我们只负责维护网格并把内容导出成**与 alacritty 无关的快照结构**。
//!
//! ## 为什么导出自己的快照类型
//!
//! 渲染层不该依赖 `alacritty_terminal` 的类型。一来换解析器时不用改渲染代码，
//! 二来渲染逻辑可以脱离终端单测。所以这里把 `Cell` / `Color` / `Flags`
//! 全部翻译成 [`CellSnapshot`] / [`TermColor`] / [`CellFlags`]。

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

use crate::error::Result;

/// 默认回滚缓冲行数。
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// 终端回传给上层的事件。
#[derive(Debug, Clone)]
pub enum TermEvent {
	/// 窗口标题变更（`OSC 0/2`）。
	Title(String),
	/// 标题被重置。
	ResetTitle,
	/// 响铃（`BEL`）。
	Bell,
	/// 终端要求把这段字节写回 PTY。
	///
	/// 必须转发，否则 TUI 程序对终端的查询（设备属性、光标位置等）会拿不到回应。
	PtyWrite(Vec<u8>),
	/// 光标闪烁状态需要重算。
	CursorBlinkingChange,
	/// 子进程退出，附带退出码。
	ChildExit(Option<i32>),
	/// 终端请求把文本写进系统剪贴板（`OSC 52`）。
	///
	/// 方向是「终端内部 → 系统剪贴板」，`tmux` / `vim` 的复制会走这条路。
	ClipboardStore(String),
	/// 终端请求读取系统剪贴板。
	///
	/// 上层拿到后应调用 [`ClipboardResponder::respond`] 把当前剪贴板内容回填，
	/// 终端收到应答后才会继续（粘贴流程）。
	ClipboardLoad(ClipboardResponder),
}

/// 回答 `ClipboardLoad` 的回调。
///
/// 单独包一层是为了给 `Debug` 留出脱敏的空间——裸的函数指针打不出有用的信息。
#[derive(Clone)]
pub struct ClipboardResponder(Arc<dyn Fn(&str) -> String + Send + Sync + 'static>);

impl ClipboardResponder {
	/// 把剪贴板内容回填给终端。返回值是终端需要的应答序列。
	#[must_use]
	pub fn respond(&self, clipboard_text: &str) -> String {
		(self.0)(clipboard_text)
	}
}

impl std::fmt::Debug for ClipboardResponder {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("ClipboardResponder(..)")
	}
}

/// 颜色的语义形式。
///
/// `Indexed` 不在这里解析成 RGB——调色板属于主题，由渲染层决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermColor {
	/// 使用主题的默认前景色。
	DefaultForeground,
	/// 使用主题的默认背景色。
	DefaultBackground,
	/// 光标色。
	Cursor,
	/// 0-15 命名色，16-255 索引色。由渲染层查表。
	Indexed(u8),
	/// 24 位真彩色。
	Rgb(u8, u8, u8),
}

/// 单元格的文本属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellFlags {
	/// 粗体。
	pub bold: bool,
	/// 斜体。
	pub italic: bool,
	/// 下划线。
	pub underline: bool,
	/// 反显（前景背景互换）。
	pub inverse: bool,
	/// 暗淡。
	pub dim: bool,
	/// 隐藏（前景与背景同色）。
	pub hidden: bool,
	/// 删除线。
	pub strikeout: bool,
	/// 宽字符的占位格。渲染时应跳过，否则宽字符会被画两遍。
	pub wide_spacer: bool,
}

/// 一个单元格的快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSnapshot {
	/// 字符。空白格是 `' '`。
	pub ch: char,
	/// 前景色。
	pub fg: TermColor,
	/// 背景色。
	pub bg: TermColor,
	/// 文本属性。
	pub flags: CellFlags,
}

impl Default for CellSnapshot {
	fn default() -> Self {
		Self {
			ch: ' ',
			fg: TermColor::DefaultForeground,
			bg: TermColor::DefaultBackground,
			flags: CellFlags::default(),
		}
	}
}

/// 光标状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
	/// 视口内的行（0 是最上面一行）。
	pub row: usize,
	/// 列。
	pub col: usize,
	/// 形状。
	pub shape: CursorShape,
}

/// 光标形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
	/// 方块。
	#[default]
	Block,
	/// 下划线。
	Underline,
	/// 竖线。
	Beam,
	/// 空心方块。
	HollowBlock,
	/// 隐藏。
	Hidden,
}

/// 选区范围，坐标已换算成**视口坐标**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionBounds {
	/// 起点 `(行, 列)`，左上。
	pub start: (usize, usize),
	/// 终点 `(行, 列)`，右下。
	pub end: (usize, usize),
	/// 是否为块状选区（矩形框选）。
	pub is_block: bool,
}

impl SelectionBounds {
	/// 判断某个视口格子是否落在选区内。
	#[must_use]
	pub fn contains(&self, row: usize, col: usize) -> bool {
		if self.is_block {
			return row >= self.start.0
				&& row <= self.end.0
				&& col >= self.start.1
				&& col <= self.end.1;
		}

		if row < self.start.0 || row > self.end.0 {
			return false;
		}
		if self.start.0 == self.end.0 {
			return col >= self.start.1 && col <= self.end.1;
		}
		if row == self.start.0 {
			return col >= self.start.1;
		}
		if row == self.end.0 {
			return col <= self.end.1;
		}
		true
	}
}

/// 一帧的网格内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridSnapshot {
	/// 列数。
	pub cols: usize,
	/// 行数。
	pub rows: usize,
	/// 单元格，行优先，长度恒为 `cols * rows`。
	pub cells: Vec<CellSnapshot>,
	/// 光标。不可见时为 `None`。
	pub cursor: Option<CursorState>,
	/// 回滚偏移。0 表示贴着底部（最新内容）。
	pub display_offset: usize,
	/// 是否处于备用屏幕（`vim` 之类会切过去）。
	pub alternate_screen: bool,
	/// 选区。没有选区时为 `None`。
	pub selection: Option<SelectionBounds>,
}

impl GridSnapshot {
	/// 取指定格子的快照。越界返回 `None`。
	#[must_use]
	pub fn cell(&self, row: usize, col: usize) -> Option<&CellSnapshot> {
		if row >= self.rows || col >= self.cols {
			return None;
		}
		self.cells.get(row * self.cols + col)
	}
}

// ==================== 事件代理 ====================

/// 把 alacritty 的事件转成我们自己的事件并收集起来。
#[derive(Clone, Debug)]
struct EventProxy {
	sink: Arc<Mutex<Vec<TermEvent>>>,
}

impl EventListener for EventProxy {
	fn send_event(&self, event: Event) {
		let mapped = match event {
			Event::Title(title) => Some(TermEvent::Title(title)),
			Event::ResetTitle => Some(TermEvent::ResetTitle),
			Event::Bell => Some(TermEvent::Bell),
			Event::PtyWrite(text) => Some(TermEvent::PtyWrite(text.into_bytes())),
			Event::CursorBlinkingChange => Some(TermEvent::CursorBlinkingChange),
			Event::ChildExit(status) => Some(TermEvent::ChildExit(status.code())),
			// 注意：alacritty 没有公开 ClipboardType，无法区分「剪贴板」与
			// 「主选区」。macOS 上两者本就合一，不区分没有实际影响。
			Event::ClipboardStore(_kind, text) => Some(TermEvent::ClipboardStore(text)),
			Event::ClipboardLoad(_kind, responder) => {
				Some(TermEvent::ClipboardLoad(ClipboardResponder(responder)))
			},
			// Wakeup 只是「该重绘了」的提示。我们按帧主动重绘，不需要它。
			// ColorRequest / TextAreaSizeRequest 需要回调式应答，暂不支持。
			Event::Wakeup
			| Event::Exit
			| Event::MouseCursorDirty
			| Event::ColorRequest(..)
			| Event::TextAreaSizeRequest(..) => None,
		};

		if let Some(event) = mapped
			&& let Ok(mut guard) = self.sink.lock()
		{
			guard.push(event);
		}
	}
}

// ==================== 尺寸 ====================

/// 给 `alacritty_terminal` 用的尺寸描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GridSize {
	cols: usize,
	rows: usize,
}

impl Dimensions for GridSize {
	fn total_lines(&self) -> usize {
		self.rows
	}

	fn screen_lines(&self) -> usize {
		self.rows
	}

	fn columns(&self) -> usize {
		self.cols
	}
}

// ==================== 网格 ====================

/// 终端网格。
pub struct TerminalGrid {
	term: Term<EventProxy>,
	parser: Processor,
	events: Arc<Mutex<Vec<TermEvent>>>,
	cols: usize,
	rows: usize,
}

impl TerminalGrid {
	/// 建一个网格。
	pub fn new(cols: u16, rows: u16, scrollback: usize) -> Result<Self> {
		let cols = usize::from(cols.max(1));
		let rows = usize::from(rows.max(1));

		let events = Arc::new(Mutex::new(Vec::new()));
		let proxy = EventProxy {
			sink: Arc::clone(&events),
		};

		let config = Config {
			scrolling_history: scrollback,
			..Config::default()
		};
		let term = Term::new(config, &GridSize { cols, rows }, proxy);

		Ok(Self {
			term,
			parser: Processor::new(),
			events,
			cols,
			rows,
		})
	}

	/// 喂入 PTY 输出。
	pub fn feed(&mut self, bytes: &[u8]) {
		self.parser.advance(&mut self.term, bytes);
	}

	/// 取走累积的事件。
	pub fn drain_events(&mut self) -> Vec<TermEvent> {
		match self.events.lock() {
			Ok(mut guard) => std::mem::take(&mut *guard),
			Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
		}
	}

	/// 调整尺寸。内容会被重排，回滚缓冲保留。
	pub fn resize(&mut self, cols: u16, rows: u16) {
		let cols = usize::from(cols.max(1));
		let rows = usize::from(rows.max(1));
		if (cols, rows) == (self.cols, self.rows) {
			return;
		}

		self.term.resize(GridSize { cols, rows });
		self.cols = cols;
		self.rows = rows;
	}

	/// 当前尺寸 `(cols, rows)`。
	#[must_use]
	pub fn size(&self) -> (usize, usize) {
		(self.cols, self.rows)
	}

	/// 导出当前视口的完整快照。
	#[must_use]
	pub fn snapshot(&self) -> GridSnapshot {
		let grid: &Grid<Cell> = self.term.grid();
		let offset = grid.display_offset() as i32;

		let mut cells = Vec::with_capacity(self.cols * self.rows);
		for row in 0..self.rows {
			// 网格坐标：视口行 0 对应 display_offset 处的历史行
			let line = Line(row as i32 - offset);
			for col in 0..self.cols {
				let cell = &grid[line][Column(col)];
				cells.push(cell_snapshot(cell));
			}
		}

		let content = self.term.renderable_content();
		let show_cursor = content.mode.contains(TermMode::SHOW_CURSOR);
		let cursor = if show_cursor {
			let point = content.cursor.point;
			let row = point.line.0 + offset;
			let row = if row < 0 {
				None
			} else {
				usize::try_from(row).ok()
			};
			row.filter(|row| *row < self.rows).map(|row| CursorState {
				row,
				col: point.column.0,
				shape: cursor_shape(content.cursor.shape),
			})
		} else {
			None
		};

		// 选区坐标同样是网格坐标，一并换算到视口
		let selection = content.selection.map(|range| {
			let to_view = |point: Point| {
				let row = point.line.0 + offset;
				(row.max(0) as usize, point.column.0)
			};
			SelectionBounds {
				start: to_view(range.start),
				end: to_view(range.end),
				is_block: range.is_block,
			}
		});

		GridSnapshot {
			cols: self.cols,
			rows: self.rows,
			cells,
			cursor,
			display_offset: grid.display_offset(),
			alternate_screen: content.mode.contains(TermMode::ALT_SCREEN),
			selection,
		}
	}

	/// 回滚指定的行数。正数向历史翻，负数回到底部。
	pub fn scroll(&mut self, lines: i32) {
		self.term
			.scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
	}

	/// 回到最新内容。
	pub fn scroll_to_bottom(&mut self) {
		self.term
			.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
	}

	/// 当前回滚偏移，0 表示贴底。
	#[must_use]
	pub fn display_offset(&self) -> usize {
		self.term.grid().display_offset()
	}

	/// 当前终端模式。
	#[must_use]
	pub fn mode(&self) -> TermMode {
		*self.term.mode()
	}

	/// 是否处于备用屏幕。
	#[must_use]
	pub fn is_alternate_screen(&self) -> bool {
		self.term.mode().contains(TermMode::ALT_SCREEN)
	}

	/// 回滚缓冲的行数。
	#[must_use]
	pub fn history_size(&self) -> usize {
		self.term.grid().history_size()
	}

	/// 取当前选区文本。没有选区时返回 `None`。
	#[must_use]
	pub fn selection_text(&self) -> Option<String> {
		self.term.selection_to_string()
	}

	/// 是否已有选区。
	#[must_use]
	pub fn has_selection(&self) -> bool {
		self.term.selection.is_some()
	}

	/// 以「视口坐标」开始一次简单选区。
	///
	/// 入参是视口内的 `(行, 列)`，内部会换算成网格坐标，因此界面层不需要
	/// 关心回滚偏移。
	pub fn start_selection(&mut self, row: usize, col: usize) {
		let point = self.to_grid_point(row, col);
		self.term.selection = Some(Selection::new(
			SelectionType::Simple,
			point,
			Direction::Left,
		));
	}

	/// 拖动更新选区终点。
	pub fn update_selection(&mut self, row: usize, col: usize) {
		let point = self.to_grid_point(row, col);
		if let Some(selection) = self.term.selection.as_mut() {
			selection.update(point, Direction::Right);
		}
	}

	/// 按整行选中。
	pub fn select_line(&mut self, row: usize) {
		let point = self.to_grid_point(row, 0);
		self.term.selection = Some(Selection::new(SelectionType::Lines, point, Direction::Left));
	}

	/// 按「词」选中：双击时用。
	///
	/// `Semantic` 由 alacritty 按 `semantic_escape_chars`（默认
	/// `,│`+/@-._~!$&'()*` 这些）自动扩到词的边界，我们只需要给出落点。
	/// 落在空白上时它会退化成单个字符，这是 alacritty 自己的行为。
	pub fn select_word(&mut self, row: usize, col: usize) {
		let point = self.to_grid_point(row, col);
		self.term.selection = Some(Selection::new(
			SelectionType::Semantic,
			point,
			Direction::Left,
		));
	}

	/// 清除选区。
	pub fn clear_selection(&mut self) {
		self.term.selection = None;
	}

	/// 视口坐标 → 网格坐标。
	fn to_grid_point(&self, row: usize, col: usize) -> Point {
		let offset = self.term.grid().display_offset() as i32;
		let line = Line(row as i32 - offset);
		Point::new(line, Column(col.min(self.cols.saturating_sub(1))))
	}
}

impl std::fmt::Debug for TerminalGrid {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("TerminalGrid")
			.field("cols", &self.cols)
			.field("rows", &self.rows)
			.field("display_offset", &self.display_offset())
			.finish_non_exhaustive()
	}
}

// ==================== 转换 ====================

/// 把控制字符换成空格。
///
/// `\t` 在网格里表示「跳到下一个制表位」，它落点那一格视觉上是空白。
/// 原样交给渲染层会被拼进整段 galley，而 egui 对 `\t` 的步进远小于一格——
/// 整行后面的内容全部左移，**同一行里制表符个数不同就左移不同距离**。
/// 真机现象：macOS 的 `ls` 用制表符排版多列，于是文件名一列一列都对不齐。
fn printable(ch: char) -> char {
	if ch.is_control() { ' ' } else { ch }
}

fn cell_snapshot(cell: &Cell) -> CellSnapshot {
	CellSnapshot {
		ch: printable(cell.c),
		fg: convert_color(cell.fg),
		bg: convert_color(cell.bg),
		flags: convert_flags(cell.flags),
	}
}

fn convert_flags(flags: Flags) -> CellFlags {
	CellFlags {
		bold: flags.contains(Flags::BOLD),
		italic: flags.contains(Flags::ITALIC),
		underline: flags.contains(Flags::UNDERLINE),
		inverse: flags.contains(Flags::INVERSE),
		dim: flags.contains(Flags::DIM),
		hidden: flags.contains(Flags::HIDDEN),
		strikeout: flags.contains(Flags::STRIKEOUT),
		wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
	}
}

fn convert_color(color: Color) -> TermColor {
	match color {
		Color::Spec(rgb) => TermColor::Rgb(rgb.r, rgb.g, rgb.b),
		Color::Indexed(index) => TermColor::Indexed(index),
		Color::Named(named) => map_named(named),
	}
}

/// 命名色映射。
///
/// `NamedColor` 的判别值是显式指定的：前 16 个就是标准 ANSI 色（0-15），
/// 之后是 `Foreground` / `Background` / `Cursor` 这些语义色，再往后是 `Dim*` 系列。
fn map_named(named: NamedColor) -> TermColor {
	match named {
		NamedColor::Foreground => TermColor::DefaultForeground,
		NamedColor::Background => TermColor::DefaultBackground,
		NamedColor::Cursor => TermColor::Cursor,
		other => {
			let index = other as usize;
			if index < 16 {
				TermColor::Indexed(index as u8)
			} else {
				// Dim* 系列目前按默认前景处理。要还原真实的变暗效果，
				// 应当把 `CellFlags::dim` 交给渲染层做亮度衰减。
				TermColor::DefaultForeground
			}
		},
	}
}

fn cursor_shape(shape: alacritty_terminal::vte::ansi::CursorShape) -> CursorShape {
	use alacritty_terminal::vte::ansi::CursorShape as VteShape;

	match shape {
		VteShape::Block => CursorShape::Block,
		VteShape::Underline => CursorShape::Underline,
		VteShape::Beam => CursorShape::Beam,
		VteShape::HollowBlock => CursorShape::HollowBlock,
		VteShape::Hidden => CursorShape::Hidden,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn grid() -> TerminalGrid {
		TerminalGrid::new(20, 5, 100).expect("建网格失败")
	}

	/// 把某一行读成去尾空格的字符串。
	fn row_text(snapshot: &GridSnapshot, row: usize) -> String {
		let mut text = String::new();
		for col in 0..snapshot.cols {
			if let Some(cell) = snapshot.cell(row, col) {
				text.push(cell.ch);
			}
		}
		text.trim_end().to_string()
	}

	#[test]
	fn plain_text_lands_in_grid() {
		let mut term = grid();
		term.feed(b"hello");

		let snapshot = term.snapshot();
		assert_eq!(row_text(&snapshot, 0), "hello");
		assert_eq!(snapshot.cols, 20);
		assert_eq!(snapshot.rows, 5);
		assert_eq!(snapshot.cells.len(), 100);
	}

	/// macOS 的 `ls` 用**制表符**排版多列。网格里那一格是「跳到下一个制表位」的标记，
	/// 视觉上是空白；若原样当成 `\t` 交给渲染层，它会按 egui 的口径步进（远小于一格），
	/// 整行后面的内容全部左移，同一行里制表符个数不同就左移不同距离——
	/// 真机现象就是文件名一列一列都对不齐。
	#[test]
	fn tabs_align_columns_like_a_real_terminal() {
		// 60 列：足够放下 32 列处的第二列，不然会折行
		let mut term = TerminalGrid::new(60, 5, 100).expect("建网格失败");
		// 与 `/bin/ls` 实际发出的字节一致：用若干制表符把第二列推到第 32 列
		term.feed(b"docker\t\t\t\tprl_disk_tool\r\ndocker-credential-osxkeychain\tprlcopy");

		let snapshot = term.snapshot();
		for row in 0..2 {
			for col in 0..snapshot.cols {
				let cell = snapshot.cell(row, col).expect("格子在范围内");
				assert!(
					!cell.ch.is_control(),
					"第 {row} 行第 {col} 列是控制字符 {:?}，会污染渲染",
					cell.ch
				);
			}
		}

		// 两行的第二列都必须落在第 32 列（8 的倍数，制表位）
		for (row, expected) in [(0, "prl_disk_tool"), (1, "prlcopy")] {
			let mut text = String::new();
			for col in 32..snapshot.cols {
				if let Some(cell) = snapshot.cell(row, col) {
					text.push(cell.ch);
				}
			}
			assert!(
				text.starts_with(expected),
				"第 {row} 行第 32 列应为 {expected}，实际 {text:?}"
			);
		}
	}

	#[test]
	fn newline_moves_to_next_row() {
		let mut term = grid();
		term.feed(b"one\r\ntwo");

		let snapshot = term.snapshot();
		assert_eq!(row_text(&snapshot, 0), "one");
		assert_eq!(row_text(&snapshot, 1), "two");
	}

	#[test]
	fn carriage_return_overwrites_line() {
		let mut term = grid();
		term.feed(b"abcdef\rXYZ");

		let snapshot = term.snapshot();
		assert_eq!(row_text(&snapshot, 0), "XYZdef");
	}

	#[test]
	fn cursor_moves_with_csi() {
		let mut term = grid();
		// 移到第 3 行第 4 列（CSI 行列都是从 1 开始）
		term.feed(b"\x1b[3;4H");

		let snapshot = term.snapshot();
		let cursor = snapshot.cursor.expect("光标应可见");
		assert_eq!((cursor.row, cursor.col), (2, 3));
	}

	#[test]
	fn basic_sgr_sets_named_color() {
		let mut term = grid();
		// 31 = 前景红（NamedColor::Red = 1）
		term.feed(b"\x1b[31mR");

		let snapshot = term.snapshot();
		let cell = snapshot.cell(0, 0).expect("应有格子");
		assert_eq!(cell.fg, TermColor::Indexed(1));
		assert_eq!(cell.bg, TermColor::DefaultBackground);
	}

	#[test]
	fn bright_color_maps_to_high_index() {
		let mut term = grid();
		// 91 = 亮红，NamedColor::BrightRed = 9
		term.feed(b"\x1b[91mR");

		let cell = *term.snapshot().cell(0, 0).expect("应有格子");
		assert_eq!(cell.fg, TermColor::Indexed(9));
	}

	#[test]
	fn indexed_256_color_is_preserved() {
		let mut term = grid();
		// 38;5;208 = 256 色模式下的橙色
		term.feed(b"\x1b[38;5;208mX");

		let cell = *term.snapshot().cell(0, 0).expect("应有格子");
		assert_eq!(cell.fg, TermColor::Indexed(208));
	}

	#[test]
	fn truecolor_is_preserved() {
		let mut term = grid();
		// 38;2;r;g;b = 24 位真彩色
		term.feed(b"\x1b[38;2;18;52;86mX");

		let cell = *term.snapshot().cell(0, 0).expect("应有格子");
		assert_eq!(cell.fg, TermColor::Rgb(18, 52, 86));
	}

	#[test]
	fn background_color_is_tracked() {
		let mut term = grid();
		term.feed(b"\x1b[48;2;1;2;3mX");

		let cell = *term.snapshot().cell(0, 0).expect("应有格子");
		assert_eq!(cell.bg, TermColor::Rgb(1, 2, 3));
	}

	#[test]
	fn text_attributes_are_tracked() {
		let mut term = grid();
		term.feed(b"\x1b[1;3;4;7;9mX");

		let cell = *term.snapshot().cell(0, 0).expect("应有格子");
		assert!(cell.flags.bold, "粗体未生效");
		assert!(cell.flags.italic, "斜体未生效");
		assert!(cell.flags.underline, "下划线未生效");
		assert!(cell.flags.inverse, "反显未生效");
		assert!(cell.flags.strikeout, "删除线未生效");
	}

	#[test]
	fn sgr_reset_clears_attributes() {
		let mut term = grid();
		term.feed(b"\x1b[1;31mA\x1b[0mB");

		let snapshot = term.snapshot();
		let first = *snapshot.cell(0, 0).expect("应有格子");
		let second = *snapshot.cell(0, 1).expect("应有格子");

		assert!(first.flags.bold);
		assert_eq!(first.fg, TermColor::Indexed(1));

		assert!(!second.flags.bold, "重置后不应保留粗体");
		assert_eq!(
			second.fg,
			TermColor::DefaultForeground,
			"重置后应回到默认前景"
		);
	}

	#[test]
	fn wide_char_marks_spacer() {
		let mut term = grid();
		term.feed("中".as_bytes());

		let snapshot = term.snapshot();
		let first = *snapshot.cell(0, 0).expect("应有格子");
		let second = *snapshot.cell(0, 1).expect("应有格子");

		assert_eq!(first.ch, '中');
		assert!(!first.flags.wide_spacer);
		assert!(second.flags.wide_spacer, "宽字符的第二列应标记为占位符");
	}

	#[test]
	fn erase_display_clears_screen() {
		let mut term = grid();
		term.feed(b"dirty\x1b[2J");

		let snapshot = term.snapshot();
		assert_eq!(row_text(&snapshot, 0), "", "清屏后不应有残留");
	}

	#[test]
	fn scrollback_is_reachable() {
		let mut term = grid();
		// 视口 5 行，写 10 行（每个 \r\n 都会推进一行，末尾再留一个空行），
		// 于是网格里共有 line0..line10 共 11 行，其中前 6 行进入回滚缓冲。
		for index in 0..10 {
			term.feed(format!("line{index}\r\n").as_bytes());
		}

		assert!(
			term.history_size() >= 6,
			"回滚缓冲应有内容，实际 {}",
			term.history_size()
		);

		// 贴着底部时，视口显示最后 5 行：line6..line9，最后一行是换行留下的空行
		let bottom = term.snapshot();
		assert_eq!(term.display_offset(), 0);
		assert_eq!(row_text(&bottom, 0), "line6");
		assert_eq!(row_text(&bottom, 3), "line9");
		assert_eq!(row_text(&bottom, 4), "", "末尾应是换行留下的空行");

		// 向上翻 5 行，视口正好显示 line1..line5
		term.scroll(5);
		let scrolled = term.snapshot();
		assert_eq!(scrolled.display_offset, 5);
		assert_eq!(row_text(&scrolled, 0), "line1");
		assert_eq!(row_text(&scrolled, 4), "line5");

		// 回到底部
		term.scroll_to_bottom();
		assert_eq!(term.display_offset(), 0, "回到底部后偏移应归零");
		assert_eq!(row_text(&term.snapshot(), 0), "line6");
	}

	#[test]
	fn title_event_is_emitted() {
		let mut term = grid();
		// OSC 0 ; title BEL
		term.feed(b"\x1b]0;my-title\x07");

		let events = term.drain_events();
		assert!(
			events
				.iter()
				.any(|event| matches!(event, TermEvent::Title(t) if t == "my-title")),
			"未收到标题事件：{events:?}"
		);
	}

	#[test]
	fn pty_write_event_is_emitted_for_queries() {
		let mut term = grid();
		// DSF：请求主设备属性，终端应产生回写
		term.feed(b"\x1b[c");

		let events = term.drain_events();
		assert!(
			events
				.iter()
				.any(|event| matches!(event, TermEvent::PtyWrite(_))),
			"终端查询未产生 PtyWrite 事件：{events:?}"
		);
	}

	#[test]
	fn drain_events_empties_queue() {
		let mut term = grid();
		term.feed(b"\x1b]0;t\x07");

		assert_eq!(term.drain_events().len(), 1);
		assert!(term.drain_events().is_empty(), "取过一次后应清空");
	}

	#[test]
	fn resize_changes_dimensions() {
		let mut term = grid();
		term.feed(b"keep-me");

		term.resize(40, 10);
		assert_eq!(term.size(), (40, 10));

		let snapshot = term.snapshot();
		assert_eq!(snapshot.cells.len(), 400);
		assert_eq!(row_text(&snapshot, 0), "keep-me", "resize 后内容应保留");
	}

	#[test]
	fn resize_to_same_size_is_noop() {
		let mut term = grid();
		term.feed(b"abc");
		term.resize(20, 5);

		assert_eq!(row_text(&term.snapshot(), 0), "abc");
	}

	#[test]
	fn alternate_screen_is_reported() {
		let mut term = grid();
		assert!(!term.is_alternate_screen());

		// 切到备用屏幕
		term.feed(b"\x1b[?1049h");
		assert!(term.is_alternate_screen(), "应已切到备用屏幕");

		term.feed(b"\x1b[?1049l");
		assert!(!term.is_alternate_screen(), "应已切回主屏幕");
	}

	#[test]
	fn cursor_can_be_hidden() {
		let mut term = grid();
		term.feed(b"\x1b[?25l");
		assert!(term.snapshot().cursor.is_none(), "隐藏光标后快照不应带光标");

		term.feed(b"\x1b[?25h");
		assert!(term.snapshot().cursor.is_some(), "恢复后光标应重新出现");
	}

	#[test]
	fn invalid_escape_sequence_does_not_panic() {
		let mut term = grid();
		// 半截的 CSI，后面没有终止字节
		term.feed(b"\x1b[");
		term.feed(b"1;2");
		term.feed(b"Hok");

		let snapshot = term.snapshot();
		assert!(!snapshot.cells.is_empty(), "网格不应崩坏");
	}

	#[test]
	fn snapshot_cell_out_of_bounds() {
		let term = grid();
		let snapshot = term.snapshot();
		assert!(snapshot.cell(99, 0).is_none());
		assert!(snapshot.cell(0, 99).is_none());
	}

	#[test]
	fn no_selection_by_default() {
		let term = grid();
		assert!(!term.has_selection());
		assert!(term.selection_text().is_none());
	}

	#[test]
	fn simple_selection_covers_range() {
		let mut term = grid();
		term.feed(b"hello world");

		term.start_selection(0, 0);
		term.update_selection(0, 5);

		assert!(term.has_selection());
		let text = term.selection_text().expect("应有选区文本");
		assert_eq!(text.trim_end(), "hello");
	}

	#[test]
	fn line_selection_takes_whole_row() {
		let mut term = grid();
		term.feed(b"first\r\nsecond");

		term.select_line(0);
		let text = term.selection_text().expect("应有选区文本");
		assert!(text.contains("first"), "整行选中应包含该行内容：{text:?}");
		assert!(!text.contains("second"), "不应选到下一行：{text:?}");
	}

	#[test]
	fn word_selection_takes_the_word_under_the_cursor() {
		let mut term = grid();
		term.feed(b"echo hello world");

		// 落在 "hello" 中间，应扩到整个词
		term.select_word(0, 6);
		let text = term.selection_text().expect("应有选区文本");
		assert_eq!(text, "hello", "双击应选中整词");
	}

	/// 路径按**整个**选中，不按 `/` 切。
	///
	/// 这条钉的是 alacritty 的默认 `semantic_escape_chars`：分隔符里没有 `/`，
	/// 所以在终端里双击一个路径拿到的是完整路径——这正是我们要的行为，
	/// 别哪天换了字符集把它改掉。
	#[test]
	fn word_selection_keeps_a_path_whole() {
		let mut term = grid();
		term.feed(b"cd /usr/local/bin");

		term.select_word(0, 9);
		let text = term.selection_text().expect("应有选区文本");
		assert_eq!(text, "/usr/local/bin");
	}

	#[test]
	fn clearing_selection_removes_it() {
		let mut term = grid();
		term.feed(b"abc");
		term.start_selection(0, 0);
		term.update_selection(0, 3);
		assert!(term.has_selection());

		term.clear_selection();
		assert!(!term.has_selection());
		assert!(term.selection_text().is_none());
	}

	#[test]
	fn update_without_start_is_a_noop() {
		let mut term = grid();
		term.feed(b"abc");
		term.update_selection(0, 2);
		assert!(!term.has_selection(), "未开始选区时拖动不应创建选区");
	}

	#[test]
	fn selection_uses_viewport_coordinates() {
		let mut term = grid();
		for index in 0..10 {
			term.feed(format!("row{index}\r\n").as_bytes());
		}

		// 向上翻 5 行后，视口第 0 行对应网格里的 row1
		term.scroll(5);
		term.select_line(0);

		let text = term.selection_text().expect("应有选区文本");
		assert!(
			text.contains("row1"),
			"视口坐标换算错误，实际选区：{text:?}"
		);
	}
}
