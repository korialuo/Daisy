//! 设计系统：设计令牌 + 一组自绘控件。
//!
//! 视图层**不直接写颜色、圆角、字号、间距的数字**——一律从这里取。
//! 否则「调一次配色」要翻遍所有视图文件，改到第三个文件时前两个已经不一致了。
//!
//! # 为什么自绘控件
//!
//! egui 的默认控件是「能用的最小外观」：直角、等宽内边距、所有按钮一个样。
//! 想做出层次（主操作 / 次级 / 危险）和细节（左侧强调条、协议徽标、hover 过渡），
//! 与其和 `Widgets` 的五个状态位角力，不如直接用 `Painter` 画——
//! 位置、颜色、圆角全部可控，而且不依赖 egui 内部的样式推导规则。
//!
//! # 令牌从哪来
//!
//! [`apply`] 把当前令牌塞进 `Context` 的临时存储，控件用 [`tokens`] 取回。
//! 这样调用方不用一路传 `&Tokens`，也不会出现「同一个界面里两套色值」。

use egui::{
	Align2, Color32, Context, CornerRadius, FontId, Margin, Painter, Rect, Response, Sense, Shape,
	Stroke, StrokeKind, TextStyle, Ui, Vec2, pos2, vec2,
};

// ==================== 令牌 ====================

/// 深浅模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
	/// 深色（Daisy 默认）。
	Dark,
	/// 浅色。
	Light,
}

impl Mode {
	/// 当前模式是不是深色。
	#[must_use]
	pub fn is_dark(self) -> bool {
		matches!(self, Self::Dark)
	}
}

/// 字号层级。
///
/// 只留三级。层级一多，每个开发者都会挑一个「差不多」的，
/// 最后界面上出现 11.5 / 12 / 12.5 / 13 四种字号。
pub const FONT_XS: f32 = 11.0;
/// 正文、按钮、输入框。
pub const FONT_BASE: f32 = 13.0;
/// 区块标题。
pub const FONT_LG: f32 = 15.0;

/// 密码输入框的字体。
///
/// 掩码点 `•` 在中文里是满格宽（1em），一串密码看着像「每个点之间空一格」；
/// 这个字体族把点换成拉丁字形的 0.32em。具体见 `fonts::password_font`。
#[must_use]
pub fn password_font() -> FontId {
	crate::fonts::password_font(FONT_BASE)
}

/// 间距（4 的倍数）。
pub const GAP_XS: f32 = 4.0;
/// 元素之间。
pub const GAP_SM: f32 = 8.0;
/// 区块内部。
pub const GAP_MD: f32 = 12.0;

/// 小圆角：徽标、分段内的选中块。
pub const RADIUS_SM: u8 = 4;
/// 中圆角：按钮、输入框、列表行。
pub const RADIUS_MD: u8 = 6;
/// 大圆角：窗口、弹层。
pub const RADIUS_LG: u8 = 8;

/// 一套配色。
///
/// 字段分成三组：背景（由深到浅）、文字（由亮到暗）、语义色。
/// 语义色只用在意有所指的地方——所有按钮都上色等于没有重点。
#[derive(Debug, Clone, Copy)]
pub struct Tokens {
	/// 当前模式。
	pub mode: Mode,
	/// 最深的一层：终端画布、输入框内底。
	pub bg_deep: Color32,
	/// 窗口底色。
	pub bg_base: Color32,
	/// 面板（侧边栏、标签栏）。
	pub bg_surface: Color32,
	/// 抬起的一层：主按钮、分段控件的选中块。
	pub bg_raised: Color32,
	/// 悬停底色。
	pub bg_hover: Color32,
	/// 选中底色。
	pub bg_active: Color32,
	/// 分隔线、控件描边。
	pub border: Color32,
	/// 更弱的分隔线（面板之间的接缝）。
	pub border_soft: Color32,
	/// 主要文字。
	pub text: Color32,
	/// 次级文字（列表项副标题）。
	pub text_dim: Color32,
	/// 弱文字（标签、状态栏）。
	pub text_muted: Color32,
	/// 极弱文字（占位符、禁用态）。
	pub text_faint: Color32,
	/// 强调色（选中指示、聚焦描边）。
	pub accent: Color32,
	/// 强调色的淡填充（协议徽标底）。
	pub accent_fill: Color32,
	/// 强调色上的文字。
	pub accent_text: Color32,
	/// 成功（连接正常）。
	pub success: Color32,
	/// 成功色的淡填充（SSH 徽标底）。
	pub success_fill: Color32,
	/// 成功色上的文字。
	pub success_text: Color32,
	/// 危险（删除、错误）。
	pub danger: Color32,
	/// 警告。
	pub warn: Color32,
}

impl Tokens {
	/// 深色。以 macOS 深色模式为基准调过对比度：
	/// 正文 `text` 对 `bg_base` 约 12:1，`text_dim` 约 5.5:1，
	/// 都高于 WCAG AA 的 4.5:1。
	#[must_use]
	pub fn dark() -> Self {
		Self {
			mode: Mode::Dark,
			bg_deep: Color32::from_rgb(0x14, 0x14, 0x16),
			bg_base: Color32::from_rgb(0x1C, 0x1C, 0x1E),
			bg_surface: Color32::from_rgb(0x21, 0x21, 0x24),
			bg_raised: Color32::from_rgb(0x2E, 0x2E, 0x34),
			bg_hover: Color32::from_rgb(0x2A, 0x2A, 0x2F),
			bg_active: Color32::from_rgb(0x26, 0x27, 0x2B),
			border: Color32::from_rgb(0x33, 0x33, 0x3A),
			border_soft: Color32::from_rgb(0x2B, 0x2B, 0x31),
			text: Color32::from_rgb(0xED, 0xED, 0xF1),
			text_dim: Color32::from_rgb(0xC6, 0xC6, 0xCE),
			text_muted: Color32::from_rgb(0x8B, 0x8B, 0x95),
			text_faint: Color32::from_rgb(0x5E, 0x5E, 0x68),
			accent: Color32::from_rgb(0x5B, 0x9B, 0xF8),
			accent_fill: Color32::from_rgb(0x26, 0x34, 0x4A),
			accent_text: Color32::from_rgb(0x8F, 0xB8, 0xF0),
			success: Color32::from_rgb(0x61, 0xC5, 0x54),
			success_fill: Color32::from_rgb(0x1F, 0x3A, 0x30),
			success_text: Color32::from_rgb(0x8E, 0xD4, 0xB4),
			danger: Color32::from_rgb(0xE5, 0x67, 0x5C),
			warn: Color32::from_rgb(0xE0, 0xB3, 0x41),
		}
	}

	/// 浅色。
	#[must_use]
	pub fn light() -> Self {
		Self {
			mode: Mode::Light,
			bg_deep: Color32::from_rgb(0xFF, 0xFF, 0xFF),
			bg_base: Color32::from_rgb(0xF6, 0xF6, 0xF8),
			bg_surface: Color32::from_rgb(0xF1, 0xF1, 0xF4),
			bg_raised: Color32::from_rgb(0xE9, 0xE9, 0xEE),
			bg_hover: Color32::from_rgb(0xEC, 0xEC, 0xF0),
			bg_active: Color32::from_rgb(0xE2, 0xE6, 0xEE),
			border: Color32::from_rgb(0xD9, 0xD9, 0xDE),
			border_soft: Color32::from_rgb(0xE4, 0xE4, 0xE9),
			text: Color32::from_rgb(0x1B, 0x1B, 0x1F),
			text_dim: Color32::from_rgb(0x3C, 0x3C, 0x44),
			text_muted: Color32::from_rgb(0x6B, 0x6B, 0x76),
			text_faint: Color32::from_rgb(0x9A, 0x9A, 0xA4),
			accent: Color32::from_rgb(0x2B, 0x6C, 0xD9),
			accent_fill: Color32::from_rgb(0xE3, 0xED, 0xFB),
			accent_text: Color32::from_rgb(0x1F, 0x5B, 0xC0),
			success: Color32::from_rgb(0x2E, 0x8B, 0x3F),
			success_fill: Color32::from_rgb(0xE1, 0xF5, 0xEE),
			success_text: Color32::from_rgb(0x0F, 0x6E, 0x56),
			danger: Color32::from_rgb(0xC4, 0x38, 0x2C),
			warn: Color32::from_rgb(0xA8, 0x72, 0x10),
		}
	}
}

/// 令牌在 `Context` 里的存储键。
fn tokens_key() -> egui::Id {
	egui::Id::new("daisy.design.tokens")
}

/// 取当前令牌。没初始化过时回退深色——总比 panic 好，
/// 而且在 egui 的 `Context::default()`（单测里大量出现）下也能正常工作。
#[must_use]
pub fn tokens(ctx: &Context) -> Tokens {
	ctx.data(|data| data.get_temp::<Tokens>(tokens_key()))
		.unwrap_or_else(Tokens::dark)
}

// ==================== 应用 ====================

/// 把配色与排版落进 egui 的样式。
///
/// `all_styles_mut` 会同时写深色和浅色两套 style——我们只用一套，
/// 两套写成一样才不会出现「系统切了外观、界面没跟着变」。
pub fn apply(ctx: &Context, mode: Mode) {
	let tokens = match mode {
		Mode::Dark => Tokens::dark(),
		Mode::Light => Tokens::light(),
	};
	ctx.data_mut(|data| data.insert_temp(tokens_key(), tokens));
	ctx.all_styles_mut(|style| style_from(&tokens, style));
}

/// 把令牌铺进一份 `Style`。抽出来是为了能单测——
/// `apply` 要 `Context`，而断言样式只需要一份 `Style`。
fn style_from(tokens: &Tokens, style: &mut egui::Style) {
	let visuals = &mut style.visuals;
	visuals.dark_mode = tokens.mode.is_dark();

	// ---- 面板与弹层 ----
	visuals.panel_fill = tokens.bg_base;
	visuals.window_fill = tokens.bg_surface;
	visuals.extreme_bg_color = tokens.bg_deep;
	visuals.faint_bg_color = tokens.bg_surface;
	visuals.code_bg_color = tokens.bg_deep;
	visuals.text_edit_bg_color = Some(tokens.bg_deep);
	visuals.window_corner_radius = CornerRadius::same(RADIUS_LG);
	visuals.menu_corner_radius = CornerRadius::same(RADIUS_MD);
	visuals.window_stroke = Stroke::new(0.5, tokens.border);
	visuals.window_highlight_topmost = false;
	// 对话框用柔和投影把它从内容区托起来。主窗口不是 `egui::Window`，
	// 不会吃到这条，所以不会出现「窗口外围一圈脏影」。
	visuals.window_shadow = egui::epaint::Shadow {
		offset: [0, 10],
		blur: 28,
		spread: 0,
		color: Color32::from_black_alpha(if tokens.mode.is_dark() { 130 } else { 40 }),
	};
	visuals.popup_shadow = egui::epaint::Shadow {
		offset: [0, 6],
		blur: 20,
		spread: 0,
		color: Color32::from_black_alpha(if tokens.mode.is_dark() { 90 } else { 28 }),
	};

	// ---- 文字与语义色 ----
	visuals.override_text_color = Some(tokens.text);
	visuals.weak_text_color = Some(tokens.text_muted);
	visuals.hyperlink_color = tokens.accent;
	visuals.warn_fg_color = tokens.warn;
	visuals.error_fg_color = tokens.danger;
	visuals.selection.bg_fill = tokens.accent.gamma_multiply(0.30);
	visuals.selection.stroke = Stroke::new(1.0, tokens.text);
	visuals.disabled_alpha = 0.45;
	// 折叠箭头用竖线会显乱，我们的分组树自己画箭头
	visuals.indent_has_left_vline = false;
	// 按钮要画底：`ui.button` 的底色受这个开关控制，关掉会退化成一行裸文字
	// （设置里的「打开」、表单里的「测试连接」都会看不见边界）。
	// 主次之分交给自绘按钮，不靠「有没有底色」。
	visuals.button_frame = true;
	visuals.collapsing_header_frame = false;

	// ---- 控件五态 ----
	//
	// 关键决定：`weak_bg_fill`（按钮底）默认**透明**。
	// egui 默认给每个按钮一块灰底，这就是「满屏方块、分不出主次」的来源。
	// 现在只有悬停/按下才有底色，主操作由 [`filled_button`] 单独上色。
	let radius = CornerRadius::same(RADIUS_MD);
	let widgets = &mut visuals.widgets;

	widgets.noninteractive.corner_radius = radius;
	widgets.noninteractive.bg_fill = tokens.bg_base;
	widgets.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
	// egui 的 `ui.separator()` 就用这条描边。设成 NONE 会让
	// 界面里所有分隔线（菜单分组、表单页脚）一起消失。
	widgets.noninteractive.bg_stroke = Stroke::new(0.5, tokens.border_soft);
	widgets.noninteractive.fg_stroke = Stroke::new(1.0, tokens.text);

	widgets.inactive.corner_radius = radius;
	widgets.inactive.bg_fill = tokens.bg_raised;
	// 必须留底：`ComboBox` 和普通 `ui.button`（设置里的「打开」、
	// 表单里的「测试连接」）都靠它画可点区域。设成透明会让它们
	// 退化成一行裸文字，用户看不出那是控件。
	// 主次不靠「有没有底色」区分——那是自绘按钮的事。
	widgets.inactive.weak_bg_fill = tokens.bg_raised;
	widgets.inactive.bg_stroke = Stroke::NONE;
	widgets.inactive.fg_stroke = Stroke::new(1.0, tokens.text_dim);

	widgets.hovered.corner_radius = radius;
	widgets.hovered.bg_fill = tokens.bg_hover;
	widgets.hovered.weak_bg_fill = tokens.bg_hover;
	widgets.hovered.bg_stroke = Stroke::NONE;
	widgets.hovered.fg_stroke = Stroke::new(1.0, tokens.text);

	widgets.active.corner_radius = radius;
	widgets.active.bg_fill = tokens.bg_active;
	widgets.active.weak_bg_fill = tokens.bg_active;
	widgets.active.bg_stroke = Stroke::NONE;
	widgets.active.fg_stroke = Stroke::new(1.0, tokens.text);

	widgets.open.corner_radius = radius;
	widgets.open.bg_fill = tokens.bg_hover;
	widgets.open.weak_bg_fill = tokens.bg_hover;
	widgets.open.bg_stroke = Stroke::NONE;
	widgets.open.fg_stroke = Stroke::new(1.0, tokens.text);

	// ---- 间距与排版 ----
	let spacing = &mut style.spacing;
	spacing.item_spacing = Vec2::new(GAP_SM, GAP_SM);
	spacing.button_padding = Vec2::new(10.0, 4.0);
	spacing.interact_size.y = 24.0;
	spacing.menu_margin = Margin::same(6);
	spacing.menu_spacing = 2.0;
	spacing.indent = 14.0;
	spacing.combo_height = 200.0;
	spacing.scroll.bar_width = 8.0;
	spacing.scroll.bar_inner_margin = 2.0;
	spacing.scroll.floating = true;

	style
		.text_styles
		.insert(TextStyle::Small, FontId::proportional(FONT_XS));
	style
		.text_styles
		.insert(TextStyle::Body, FontId::proportional(FONT_BASE));
	style
		.text_styles
		.insert(TextStyle::Button, FontId::proportional(FONT_BASE));
	style
		.text_styles
		.insert(TextStyle::Heading, FontId::proportional(FONT_LG));
}

// ==================== 自绘控件 ====================

/// 按钮的语义级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
	/// 主操作：有底色，文字最亮。
	Filled,
	/// 次级操作：平时透明，悬停才显底色。
	Ghost,
	/// 危险操作：悬停时底色泛红。
	Danger,
}

impl ButtonKind {
	fn fill(self, tokens: &Tokens, hovered: bool) -> Color32 {
		match (self, hovered) {
			(ButtonKind::Filled, false) => tokens.bg_raised,
			(ButtonKind::Filled, true) => tokens.bg_hover,
			(ButtonKind::Ghost, false) => Color32::TRANSPARENT,
			(ButtonKind::Ghost, true) => tokens.bg_hover,
			(ButtonKind::Danger, false) => Color32::TRANSPARENT,
			(ButtonKind::Danger, true) => tokens.danger.gamma_multiply(0.18),
		}
	}

	fn text_color(self, tokens: &Tokens) -> Color32 {
		match self {
			ButtonKind::Filled => tokens.text,
			ButtonKind::Ghost => tokens.text_muted,
			ButtonKind::Danger => tokens.text_muted,
		}
	}

	fn hover_text_color(self, tokens: &Tokens) -> Color32 {
		match self {
			ButtonKind::Danger => tokens.danger,
			_ => tokens.text,
		}
	}
}

/// 画一个按钮。
///
/// 宽度按文字算，不用 `add_sized`——等宽按钮会让「删除」和「新建」一样重，
/// 这正是原界面的问题之一。
fn button(ui: &mut Ui, text: &str, kind: ButtonKind, height: f32) -> Response {
	let tokens = tokens(ui.ctx());
	let font = FontId::proportional(FONT_BASE);
	let width = ui
		.painter()
		.layout_no_wrap(text.to_owned(), font.clone(), tokens.text)
		.size()
		.x + 20.0;
	let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());

	let enabled = ui.is_enabled();
	let hovered = enabled && response.hovered();
	let fill = if enabled {
		kind.fill(&tokens, hovered)
	} else {
		Color32::TRANSPARENT
	};
	let fg = if !enabled {
		tokens.text_faint
	} else if hovered {
		kind.hover_text_color(&tokens)
	} else {
		kind.text_color(&tokens)
	};

	let painter = ui.painter();
	if fill != Color32::TRANSPARENT {
		painter.rect_filled(rect, RADIUS_MD, fill);
	}
	painter.text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
	response
}

/// 主操作按钮（新建、确定）。
pub fn filled_button(ui: &mut Ui, text: &str) -> Response {
	button(ui, text, ButtonKind::Filled, 28.0)
}

/// 次级按钮（取消、编辑、设置）。平时只有文字，悬停才显底。
pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
	button(ui, text, ButtonKind::Ghost, 26.0)
}

/// 危险按钮（删除）。平时与次级一致，悬停才泛红——
/// 常驻红色会让整个界面显得警报长鸣。
pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
	button(ui, text, ButtonKind::Danger, 26.0)
}

/// 占满可用宽度的主按钮（侧边栏顶部的「新建主机」）。
///
/// 带一个左置加号：纯文字的满宽按钮在小面板里只是一块色块，
/// 有了图标才立刻读得出「新建」——这是整个侧边栏唯一的主操作，
/// 不该靠用户读文案才认出来。
pub fn block_button(ui: &mut Ui, text: &str) -> Response {
	let width = ui.available_width();
	block_button_sized(ui, text, width)
}

/// 指定宽度的主按钮，用于和别的按钮并排。
///
/// 并排时不能再用 `available_width()`：那一行里已经放进了一个按钮，
/// 剩下的宽度不足以放下主按钮自己的内容。
pub fn block_button_sized(ui: &mut Ui, text: &str, width: f32) -> Response {
	let tokens = tokens(ui.ctx());
	let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 32.0), Sense::click());

	let fill = if response.hovered() {
		tokens.bg_hover
	} else {
		tokens.bg_raised
	};
	let painter = ui.painter();
	painter.rect_filled(rect, RADIUS_MD, fill);

	let icon_rect =
		Rect::from_center_size(pos2(rect.left() + 17.0, rect.center().y), Vec2::splat(11.0));
	icon_plus(painter, icon_rect, tokens.text_dim);
	painter.text(
		pos2(rect.left() + 32.0, rect.center().y),
		Align2::LEFT_CENTER,
		text,
		FontId::proportional(FONT_BASE),
		tokens.text,
	);
	response
}

/// 与 [`block_button`] 同高的次按钮：描边 + 居中文字，没有图标。
///
/// 「新建分组」原来只是搜索框右侧的一枚**裸加号**——读起来像「搜索选项」，
/// 谁也认不出它是新建分组。挪到主按钮旁边、四个字写全，才是一目了然。
///
/// 用描边而不是填充：同高的填充色块会和主按钮抢同等的分量，
/// 侧边栏就又回到「两个主操作」的老问题。
pub fn outline_button(ui: &mut Ui, text: &str, width: f32) -> Response {
	let tokens = tokens(ui.ctx());
	let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 32.0), Sense::click());
	let hovered = response.hovered();

	let painter = ui.painter();
	if hovered {
		painter.rect_filled(rect, RADIUS_MD, tokens.bg_hover);
	}
	painter.rect_stroke(
		rect,
		RADIUS_MD,
		Stroke::new(1.0, tokens.border),
		StrokeKind::Inside,
	);
	painter.text(
		rect.center(),
		Align2::CENTER_CENTER,
		text,
		FontId::proportional(FONT_BASE),
		if hovered {
			tokens.text
		} else {
			tokens.text_dim
		},
	);
	response
}

/// 次按钮在并排行里需要的宽度：文字宽度 + 左右内边距。
///
/// 宽度按文字量出来而不是写死——换文案、换字体都不会把字挤掉。
pub fn outline_button_width(ui: &Ui, text: &str) -> f32 {
	let tokens = tokens(ui.ctx());
	let text_width = ui
		.painter()
		.layout_no_wrap(
			text.to_owned(),
			FontId::proportional(FONT_BASE),
			tokens.text,
		)
		.size()
		.x;
	text_width + 26.0
}

// ---- 图标 ----
//
// 一律用 `Painter` 画。中文字体（Hiragino Sans GB）没有 ▸ ▾ ⚙ 这些字形，
// 当文本打出来是方框（实测过）。线条还顺带保证了 1px 的一致描边。

/// 加号。
pub fn icon_plus(painter: &Painter, rect: Rect, color: Color32) {
	let center = rect.center();
	let arm = rect.width() * 0.42;
	let stroke = Stroke::new(1.4, color);
	painter.line_segment([center - vec2(0.0, arm), center + vec2(0.0, arm)], stroke);
	painter.line_segment([center - vec2(arm, 0.0), center + vec2(arm, 0.0)], stroke);
}

/// 放大镜。
pub fn icon_search(painter: &Painter, rect: Rect, color: Color32) {
	let stroke = Stroke::new(1.4, color);
	let radius = rect.width() * 0.32;
	let center = rect.center() - vec2(rect.width() * 0.08, rect.width() * 0.08);
	painter.circle_stroke(center, radius, stroke);
	let tail = center + vec2(radius * 0.72, radius * 0.72);
	painter.line_segment(
		[tail, tail + vec2(rect.width() * 0.18, rect.width() * 0.18)],
		stroke,
	);
}

/// 折叠箭头。`open` 为真时朝下（已展开），否则朝右。
pub fn icon_chevron(painter: &Painter, rect: Rect, color: Color32, open: bool) {
	let center = rect.center();
	let arm = rect.width() * 0.30;
	let stroke = Stroke::new(1.4, color);
	let points = if open {
		vec![
			center + vec2(-arm, -arm * 0.45),
			center + vec2(0.0, arm * 0.45),
			center + vec2(arm, -arm * 0.45),
		]
	} else {
		vec![
			center + vec2(-arm * 0.45, -arm),
			center + vec2(arm * 0.45, 0.0),
			center + vec2(-arm * 0.45, arm),
		]
	};
	painter.add(Shape::line(points, stroke));
}

/// 关闭（×）。
///
/// `rect` 是**十字本身**的包围盒：笔画撑满它。别把点击区域直接传进来——
/// 点击区通常有 18~20pt，按 0.3 比例缩出来的十字只有 5pt，
/// 看起来像一粒灰点（实测标签页上就是这么缩小的）。
pub fn icon_close(painter: &Painter, rect: Rect, color: Color32) {
	let center = rect.center();
	let arm = rect.width() * 0.5;
	// 笔画随图形缩放：小图标用固定 1.4 会糊成一团（6pt 的十字配 1.4 的笔，
	// 中间几乎实心）。下限 1.0 保证再小也不会被抗锯齿吃掉。
	let stroke = Stroke::new((rect.width() * 0.18).clamp(1.0, 1.6), color);
	painter.line_segment([center + vec2(-arm, -arm), center + vec2(arm, arm)], stroke);
	painter.line_segment([center + vec2(-arm, arm), center + vec2(arm, -arm)], stroke);
}

// ==================== 复合控件 ====================

/// 分段控件（macOS 风格的 segmented control）。
///
/// 替代原来「两个 `selectable_value` 并排」——那两个选中时各自填一块蓝，
/// 看起来像两个独立按钮而不是同一个开关的两档。
///
/// 返回被点中的新索引。
pub fn segmented(ui: &mut Ui, labels: &[&str], selected: usize) -> Option<usize> {
	let tokens = tokens(ui.ctx());
	let height = 26.0;
	let width = ui.available_width();
	let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
	let painter = ui.painter();

	painter.rect_filled(rect, RADIUS_MD, tokens.bg_deep);

	let padding = 2.0;
	let segment = (rect.width() - padding * 2.0) / labels.len() as f32;
	for (index, label) in labels.iter().enumerate() {
		let left = rect.left() + padding + segment * index as f32;
		let cell = Rect::from_min_size(
			pos2(left, rect.top() + padding),
			vec2(segment, rect.height() - padding * 2.0),
		);
		let is_selected = index == selected;
		let is_hovered = response
			.hover_pos()
			.is_some_and(|pointer| cell.contains(pointer));

		if is_selected {
			painter.rect_filled(cell, RADIUS_SM, tokens.bg_raised);
		} else if is_hovered {
			painter.rect_filled(cell, RADIUS_SM, tokens.bg_hover);
		}
		let color = if is_selected {
			tokens.text
		} else if is_hovered {
			tokens.text_dim
		} else {
			tokens.text_muted
		};
		painter.text(
			cell.center(),
			Align2::CENTER_CENTER,
			*label,
			FontId::proportional(FONT_XS + 1.0),
			color,
		);
	}

	if response.clicked()
		&& let Some(pointer) = response.interact_pointer_pos()
	{
		let offset = pointer.x - rect.left() - padding;
		if offset >= 0.0 {
			let index = (offset / segment) as usize;
			if index < labels.len() {
				return Some(index);
			}
		}
	}
	None
}

/// 搜索框：圆角底 + 左置放大镜 + 聚焦时强调描边。
pub fn search_field(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
	let tokens = tokens(ui.ctx());
	let inner = egui::Frame::NONE
		.fill(tokens.bg_deep)
		.corner_radius(RADIUS_MD)
		.inner_margin(Margin::symmetric(9, 5))
		.show(ui, |ui| {
			ui.horizontal(|ui| {
				ui.spacing_mut().item_spacing.x = 7.0;
				let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
				icon_search(ui.painter(), icon_rect, tokens.text_faint);
				ui.add(
					egui::TextEdit::singleline(text)
						.hint_text(hint)
						.frame(egui::Frame::NONE)
						.desired_width(f32::INFINITY),
				)
			})
			.inner
		});

	// 聚焦描边画在框外面——画在里面会被文字压住
	if inner.inner.has_focus() {
		ui.painter().rect_stroke(
			inner.response.rect,
			RADIUS_MD,
			Stroke::new(1.0, tokens.accent),
			StrokeKind::Inside,
		);
	}
	inner.inner
}

/// 一枚小胶囊按钮（成排的档位单选，如缩放模式）。
///
/// 和 [`segmented`] 的区别：segmented 是**占满宽度**的整块开关，
/// 胶囊是按内容宽度排开的一组，右边还能接别的东西（滑条、下拉框）。
///
/// 用它替代 `selectable_label`：后者选中时填的是 `selection.bg_fill`
/// （那是给文本选中用的强调色），一排按钮全蓝会盖过画面本身。
pub fn chip(ui: &mut Ui, label: &str, selected: bool) -> Response {
	let tokens = tokens(ui.ctx());
	let font = FontId::proportional(FONT_BASE);
	let width = ui
		.painter()
		.layout_no_wrap(label.to_owned(), font.clone(), tokens.text)
		.size()
		.x + 18.0;
	let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::click());

	let fill = if selected {
		tokens.bg_raised
	} else if response.hovered() {
		tokens.bg_hover
	} else {
		Color32::TRANSPARENT
	};
	if fill != Color32::TRANSPARENT {
		ui.painter().rect_filled(rect, RADIUS_SM, fill);
	}
	let color = if selected {
		tokens.text
	} else if response.hovered() {
		tokens.text_dim
	} else {
		tokens.text_muted
	};
	ui.painter()
		.text(rect.center(), Align2::CENTER_CENTER, label, font, color);
	response
}

/// 一行小字（状态栏、计数）。
pub fn caption(ui: &mut Ui, text: impl Into<String>, color: Color32) {
	ui.label(egui::RichText::new(text.into()).size(FONT_XS).color(color));
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 深色下正文与底色的对比度要够。
	///
	/// 用 WCAG 的相对亮度公式算——不是凭肉眼「看着还行」。
	/// 界面上最要紧的是列表正文，它掉到 4.5:1 以下就开始费眼。
	#[test]
	fn dark_text_contrast_meets_wcag_aa() {
		fn luminance(color: Color32) -> f32 {
			let channel = |value: u8| {
				let value = f32::from(value) / 255.0;
				if value <= 0.039_28 {
					value / 12.92
				} else {
					((value + 0.055) / 1.055).powf(2.4)
				}
			};
			0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
		}
		fn contrast(foreground: Color32, background: Color32) -> f32 {
			let a = luminance(foreground);
			let b = luminance(background);
			let (high, low) = if a > b { (a, b) } else { (b, a) };
			(high + 0.05) / (low + 0.05)
		}

		let tokens = Tokens::dark();
		for background in [tokens.bg_base, tokens.bg_surface, tokens.bg_active] {
			let ratio = contrast(tokens.text, background);
			assert!(ratio >= 4.5, "正文对比度只有 {ratio:.2}:1");
			let ratio = contrast(tokens.text_dim, background);
			assert!(ratio >= 4.5, "副标题对比度只有 {ratio:.2}:1");
		}
	}

	/// 浅色同样要过。两套主题只调一套是常见疏漏。
	#[test]
	fn light_text_contrast_meets_wcag_aa() {
		fn luminance(color: Color32) -> f32 {
			let channel = |value: u8| {
				let value = f32::from(value) / 255.0;
				if value <= 0.039_28 {
					value / 12.92
				} else {
					((value + 0.055) / 1.055).powf(2.4)
				}
			};
			0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
		}
		fn contrast(foreground: Color32, background: Color32) -> f32 {
			let a = luminance(foreground);
			let b = luminance(background);
			let (high, low) = if a > b { (a, b) } else { (b, a) };
			(high + 0.05) / (low + 0.05)
		}

		let tokens = Tokens::light();
		for background in [tokens.bg_base, tokens.bg_surface, tokens.bg_active] {
			let ratio = contrast(tokens.text, background);
			assert!(ratio >= 4.5, "正文对比度只有 {ratio:.2}:1");
			let ratio = contrast(tokens.text_dim, background);
			assert!(ratio >= 4.5, "副标题对比度只有 {ratio:.2}:1");
		}
	}

	/// 按钮的三种语义级别必须真的长得不一样。
	///
	/// 原界面的问题就是「删除」和「新建」一个样。这条用例钉住：
	/// 主操作有底色、次级透明、危险只在悬停时泛红。
	#[test]
	fn button_kinds_are_visually_distinct() {
		let tokens = Tokens::dark();
		assert_ne!(
			ButtonKind::Filled.fill(&tokens, false),
			Color32::TRANSPARENT,
			"主操作必须自带底色"
		);
		assert_eq!(
			ButtonKind::Ghost.fill(&tokens, false),
			Color32::TRANSPARENT,
			"次级按钮平时不该有底色"
		);
		assert_eq!(
			ButtonKind::Danger.fill(&tokens, false),
			Color32::TRANSPARENT,
			"危险按钮平时不该常驻红色"
		);
		assert_ne!(
			ButtonKind::Danger.hover_text_color(&tokens),
			ButtonKind::Ghost.hover_text_color(&tokens),
			"删除悬停要看得出是危险操作"
		);
	}

	/// 样式铺完后，原生控件必须保留可辨认的底色。
	///
	/// 这条钉住两个真踩过的坑：
	/// - 把 `weak_bg_fill` 设成透明，`ComboBox`、`ui.button`（设置里的
	///   「打开」、表单里的「测试连接」）就退化成一串裸文字，看不出能点；
	/// - 把 `button_frame` 关掉，`Button` 干脆不画底，同上。
	///
	/// 主次之分交给自绘按钮（[`filled_button`] / [`ghost_button`]），
	/// 不靠「原生控件有没有底色」。
	#[test]
	fn style_keeps_native_controls_visible() {
		let mut style = egui::Style::default();
		style_from(&Tokens::dark(), &mut style);

		assert_eq!(
			style.visuals.widgets.inactive.weak_bg_fill,
			Tokens::dark().bg_raised,
			"下拉框和普通按钮都靠这个底色才看得出是控件"
		);
		assert!(
			style.visuals.button_frame,
			"关掉它，`ui.button` 会退化成一行裸文字"
		);
		assert_eq!(
			style.visuals.widgets.inactive.corner_radius,
			CornerRadius::same(RADIUS_MD)
		);
		assert_eq!(style.visuals.panel_fill, Tokens::dark().bg_base);
	}

	/// `ui.separator()` 用 `noninteractive.bg_stroke` 画线。
	///
	/// 设成 `Stroke::NONE` 会让界面里**所有**分隔线一起消失——
	/// 菜单分组、表单页脚的分界都会没了，而且不会有任何报错。
	#[test]
	fn separator_stroke_is_not_none() {
		let mut style = egui::Style::default();
		style_from(&Tokens::dark(), &mut style);
		assert!(
			style.visuals.widgets.noninteractive.bg_stroke.width > 0.0,
			"separator 会整条消失"
		);
	}

	/// 没初始化过令牌时也不能炸。
	#[test]
	fn tokens_fall_back_to_dark() {
		let ctx = Context::default();
		assert_eq!(tokens(&ctx).mode, Mode::Dark);
	}

	/// 深浅两套必须真的不同，否则「跟随系统」等于没做。
	#[test]
	fn modes_differ() {
		let dark = Tokens::dark();
		let light = Tokens::light();
		assert_ne!(dark.bg_base, light.bg_base);
		assert_ne!(dark.text, light.text);
		assert!(dark.mode.is_dark());
		assert!(!light.mode.is_dark());
	}
}
