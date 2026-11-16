//! 界面视图。每个视图只负责「画 + 收动作」，不直接改应用状态。

use daisy_core::model::Group;

use crate::design;

pub mod host_form;
pub mod host_list;
pub mod proxy_form;
pub mod proxy_list;
pub mod rdp_view;
pub mod settings;
pub mod sftp_view;
pub mod toast;

/// 表单行标签的宽度。
///
/// 统一取 80：中文四字标签（「私钥口令」「回滚行数」）在 15px 字号下约 60px，
/// 留出余量。各视图各写一个宽度会让不同窗口的控件起始位置对不齐。
pub(crate) const LABEL_WIDTH: f32 = 80.0;

/// 标签与控件之间的间隔。
///
/// 之前有 8 与 10 两个值同时存在于不同视图里，下拉框与输入框的左边缘就差
/// 这么一点——单看每一行都正常，扫一眼整列就是歪的。
pub(crate) const FIELD_GAP: f32 = 10.0;

/// 行与行之间的间隔。
///
/// 比 `spacing.item_spacing` 默认的 8 稍大：表单行高 24，间距 8 时整片读起来
/// 像一堵墙；10 才让每一行立得住，又不至于散。
pub(crate) const ROW_GAP: f32 = 10.0;

/// 表单行里控件的统一高度。
///
/// 实测三个数：`TextEdit` 20、`DragValue` 27.5、`ComboBox` 27.5。
/// 混在一列里行距就会在 34 与 37.5 之间跳，扫一眼整张表就是「间距不匀」。
/// 统一按 28（下拉框加按钮内边距后的实际高度）走。
pub(crate) const ROW_HEIGHT: f32 = 28.0;

/// 字段与它自己的说明文字之间的间隔。说明要贴住字段，否则会像下一个字段的标题。
const HINT_GAP: f32 = 2.0;

/// 分节标题上方的间隔。
const SECTION_GAP: f32 = 26.0;

/// 分节标题与下方分隔线之间的间隔。
const SECTION_HEADER_GAP: f32 = 6.0;

/// 画一个与控件等高的表单标签，**文字左对齐**、与分节标题同一条左边界。
///
/// 曾经做过右对齐（想让 2 字与 4 字标签在同一竖线上收尾），但整张表读下来
/// 左边界参差更扎眼：分节标题、说明文字都是左起的，只有标签右对齐会单独
/// 跳出一套基准。统一左起之后，标签列、分节标题共用一条竖线，控件列共用
/// 另一条。
///
/// **必须用 `allocate_exact_size` 占位，不能用 `allocate_ui_with_layout`。**
/// 后者只把请求的尺寸当作上限，实际按内容收缩：实测「名称」占 26、
/// 「剪贴板」占 39、空标签占 0——于是控件左边缘在三处之间跳，空标签的行
/// （说明文字、测试连接按钮）整整左移 26pt。这正是「控件没对齐」的真因。
///
/// 文字用 `Painter` 画：`ui.label` 会自己决定宽度，而这里需要的是
/// 「一格固定宽的槽」，字号取自 `TextStyle::Body`，与其它标签一致。
pub(crate) fn field_label(ui: &mut egui::Ui, label: &str) -> egui::Response {
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(LABEL_WIDTH, ROW_HEIGHT), egui::Sense::hover());

	if !label.is_empty() {
		ui.painter().text(
			rect.left_center(),
			egui::Align2::LEFT_CENTER,
			label,
			egui::TextStyle::Body.resolve(ui.style()),
			ui.visuals().text_color(),
		);
	}

	response
}

/// 画一行「标签 + 任意控件」。
///
/// **不要换成 `horizontal_centered`**：它按「可用高度」给整行居中，在
/// `ScrollArea` 里可用高度是整个滚动区——第一行就把窗口高度吃光，后面的
/// 行全被挤到可视区外（实测整张表单只剩「名称」一行）。
/// 垂直对齐改由「所有控件统一 24 高」保证（见 [`text_row`]）。
///
/// 所有表单行**都必须**走这个函数（或 [`field`]）：`ComboBox`、`checkbox`、
/// `radio_value` 的返回值不是 `Response`，塞不进 [`field`] 的签名，早先就在这些
/// 地方各写了一遍 `ui.horizontal(|ui| { ui.label(..); .. })`——少了标签列与间隔，
/// 控件左边缘比别的行左移 8pt，整列看起来就是歪的。
pub(crate) fn field_ui(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
	ui.horizontal(|ui| {
		// 行内的横向间隔就是「标签 → 控件」的间隔，显式定成 [`FIELD_GAP`]：
		// 默认的 8 略挤，而且换个主题就漂。**不要再用 `add_space`**——
		// 横向布局会在两个元素之间自动插一次 `item_spacing`，`add_space`
		// 叠在上面的结果是 80 + 10 + 8 = 98，与常量对不上，也难解释。
		ui.spacing_mut().item_spacing.x = FIELD_GAP;
		#[cfg(all(test, unix))]
		{
			let label = field_label(ui, label);
			record_row(label.rect.left(), ui.next_widget_position().x);
		}
		#[cfg(not(all(test, unix)))]
		field_label(ui, label);
		add(ui);
	});
}

/// [`field_ui`] 的简写：控件返回 `Response` 时用它，与旧代码保持兼容。
pub(crate) fn field(
	ui: &mut egui::Ui,
	label: &str,
	add: impl FnOnce(&mut egui::Ui) -> egui::Response,
) {
	field_ui(ui, label, |ui| {
		add(ui);
	});
}

/// 字段下方的说明文字。
///
/// 借同一套布局（固定宽的空标签占位 + 同样的间隔），说明文字自然与上方控件
/// 左对齐——直接 `ui.weak()` 会让它贴到标签那一列，看起来像另一个字段。
/// 行距临时压到 [`HINT_GAP`]，说明才贴得住它解释的那个字段。
///
/// **必须 `.wrap()`**：说明比输入框长，egui 在横向布局里默认不换行（`Extend`），
/// 一行行地把窗口顶宽——实测撑到 548 宽（设定是 440）。换行之后窗口宽度才
/// 由 [`dialog`] 说了算。
pub(crate) fn hint(ui: &mut egui::Ui, text: &str) {
	let spacing = ui.spacing().item_spacing.y;
	ui.spacing_mut().item_spacing.y = HINT_GAP;
	field_ui(ui, "", |ui| {
		ui.add(
			egui::Label::new(egui::RichText::new(text).size(design::FONT_XS).weak())
				.wrap()
				.selectable(false),
		);
	});
	ui.spacing_mut().item_spacing.y = spacing;
}

/// 分节标题 + 分隔线。
///
/// 标题上方留 [`SECTION_GAP`]，下方只留 [`SECTION_HEADER_GAP`] 再接分隔线：
/// 视觉上「标题」与它下面那组内容绑在一起，与上一组分开。
pub(crate) fn section(ui: &mut egui::Ui, title: &str) {
	section_with_note(ui, title, "");
}

/// 分节标题 + 同行的小字说明 + 分隔线。
///
/// 说明挤在同一行（小字、弱色）而不是另起一行：表单里另起一行会白白多出
/// 一整行高度，而这句话读一次就够，不需要占那么大版面。
pub(crate) fn section_with_note(ui: &mut egui::Ui, title: &str, note: &str) {
	ui.add_space(SECTION_GAP);
	ui.horizontal(|ui| {
		ui.add(egui::Label::new(egui::RichText::new(title).strong()).selectable(false));
		if !note.is_empty() {
			ui.add(
				egui::Label::new(egui::RichText::new(note).size(design::FONT_XS).weak())
					.selectable(false),
			);
		}
	});
	ui.add_space(SECTION_HEADER_GAP);
	ui.separator();
}

/// 分隔线上下留白。
///
/// 比行距小：分隔线本身就是一道视觉停顿，再各留一个行距会让两块内容
/// 隔得太远，读起来像两页纸。6 刚好让它「贴着上组的尾巴、托着下组的头」。
const DIVIDER_GAP: f32 = 8.0;

/// 表单里的分隔线：上下留出对称的呼吸空间。
///
/// 直接 `ui.separator()` 会贴着上一行（默认行距），看起来像把内容切断了。
pub(crate) fn divider(ui: &mut egui::Ui) {
	let spacing = ui.spacing().item_spacing.y;
	ui.spacing_mut().item_spacing.y = DIVIDER_GAP;
	ui.separator();
	ui.spacing_mut().item_spacing.y = spacing;
}

/// 文本输入框的标准宽度。
///
/// 曾经想用 `ui.available_width()` 让它自己吃满剩余空间，但那在 `ScrollArea`
/// 里是**不可用**的：滚动区的内容宽度反过来由内容决定，输入框要「尽可能宽」
/// 就形成正反馈，宽度一路失控（实测那一行被撑到整窗宽）。
///
/// 固定宽度 + 对话框按它定尺寸，右边距才可控；260 也刚好放得下最长的
/// hint 而不折行。
pub(crate) const FIELD_WIDTH: f32 = 280.0;

/// 小标签（徽标）的字号与高度。
///
/// 主机列表的 SSH / RDP、字体下拉的「等宽」都用这一套：同一个界面里的小标签
/// 只能有一种样子，各画各的迟早出现 12pt 和 10pt 并排。
const BADGE_FONT_SIZE: f32 = 10.0;
const BADGE_HEIGHT: f32 = 14.0;

/// 在 `painter` 上画一个徽标，返回它占的宽度。
///
/// `left` 是左边缘、`center_y` 是垂直中心：调用方通常已经算好「跟哪段文字的
/// 墨迹对齐」，这里不再猜基线。
pub(crate) fn paint_badge(
	painter: &egui::Painter,
	left: f32,
	center_y: f32,
	text: &str,
	fg: egui::Color32,
	bg: egui::Color32,
) -> f32 {
	let font = egui::FontId::proportional(BADGE_FONT_SIZE);
	let width = painter
		.layout_no_wrap(text.to_owned(), font.clone(), fg)
		.size()
		.x
		+ 8.0;

	let rect = egui::Rect::from_min_size(
		egui::pos2(left, center_y - BADGE_HEIGHT * 0.5),
		egui::vec2(width, BADGE_HEIGHT),
	);
	painter.rect_filled(rect, 3, bg);
	painter.text(rect.center(), egui::Align2::CENTER_CENTER, text, font, fg);
	width
}

/// 右键菜单的最小宽度。
///
/// 菜单宽度由最宽的那一项决定，两三个字的菜单（「复制」「下载」）只有 70–90pt，
/// 一列项挤在窄条里像下拉框。给个下限，短菜单才看得出是菜单。
const MENU_MIN_WIDTH: f32 = 150.0;

/// 画一个右键菜单：统一宽度与行内边距。
///
/// 所有 `context_menu` 的闭包体都过这里——菜单宽度散在各处时，
/// 迟早出现「这个菜单比那个宽一截」。
pub(crate) fn menu(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
	ui.set_min_width(MENU_MIN_WIDTH);
	// 菜单项左右要比普通按钮松：贴着边框的菜单像被裁过
	ui.spacing_mut().button_padding.x = 14.0;
	add(ui);
}

/// 统一高度与宽度的文本输入。
///
/// `TextEdit` 自己只有 20 高，下拉框与按钮是 24——同一列里高度不一，标签文字
/// 与控件文字就会差两个像素。这里强制成 `interact_size.y`，整列才对得齐。
///
/// **必须显式 `vertical_align(Center)`**：`TextEdit` 的默认对齐是 `LEFT_TOP`
/// （`epaint` 的多行语义），强制加高之后文字全挤在上沿——28 高的框里文字
/// 比中心高 2pt 多，一列看下来就是「文字都在框的上面」。横向仍保持左对齐。
///
/// **左右内边距要自己给**：`TextEdit` 的默认值是 `Margin::symmetric(4, 2)`，
/// 4pt 在 28 高的框里几乎看不出来——占位文字和光标都像贴着左边框（实测
/// 文字墨迹离边框只有 2.5pt）。取 10，与胶囊按钮、搜索框的内边距同档。
pub(crate) fn text_row(ui: &mut egui::Ui, edit: egui::TextEdit<'_>) -> egui::Response {
	ui.add_sized(
		[FIELD_WIDTH, ROW_HEIGHT],
		edit.vertical_align(egui::Align::Center)
			.margin(egui::Margin {
				left: TEXT_PAD_X,
				right: TEXT_PAD_X,
				top: TEXT_PAD_Y,
				bottom: TEXT_PAD_Y,
			}),
	)
}

/// 密码输入框：几何与 [`text_row`] 一致，只是换一份**掩码点更窄**的字体。
///
/// `•` 在中文字体里是满格宽（1em），输满一行只能放下十几个点、看着像点之间
/// 空了一格；`design::password_font` 那一族把它换成拉丁字形的 0.32em。
/// 字号仍取正文档，行高与其它行一致。
pub(crate) fn password_row(ui: &mut egui::Ui, edit: egui::TextEdit<'_>) -> egui::Response {
	text_row(ui, edit.font(design::password_font()))
}

/// 文本输入框的左右内边距。
///
/// 光标与占位文字都从这条内边距起步，改一处两个都跟着走。
const TEXT_PAD_X: i8 = 10;
/// 文本输入框的上下内边距，保持 `TextEdit` 的默认值。
///
/// **不能跟着放宽**：`TextEdit` 的高度是 `min(行框高 + 上下边距, 可用高)`，
/// 13px 中文行框 19.5，上下各 5 就是 29.5——会把 [`ROW_HEIGHT`] 的 28 顶开，
/// 整列高度又不一致了。
const TEXT_PAD_Y: i8 = 2;

/// 底部按钮条占用的高度（分隔线 + 按钮行 + 上下间隔）。
/// 滚动区要把它让出来。`ScrollArea` 默认吃满可用高度，排在它后面的按钮就会
/// 被挤出可视区——具体表现是最后一行说明被从中间切断，看着像界面坏了。
pub(crate) const FOOTER_HEIGHT: f32 = ROW_HEIGHT + 26.0;

/// 表单主体：滚动区，并给底部按钮条留出位置。
///
/// 滚动条**常显**：表单在 740 高的窗口里放不下（编辑 RDP 主机时下面还有
/// 证书指纹一节），不给可见的滚动条，用户会以为「分组 / 代理」那几行没画出来。
pub(crate) fn scroll_body(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
	let height = (ui.available_height() - FOOTER_HEIGHT).max(120.0);
	egui::ScrollArea::vertical()
		.max_height(height)
		.scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
		.show(ui, add);
}

/// 数字输入的标准尺寸：与文本输入同高，**值左对齐**。
///
/// 两个坑叠在一起，缺一个都还是居中：
///
/// ① `DragValue` 非编辑态其实是个 `Button`，文字对齐取自
///    `ui.layout().horizontal_align()`——也就是布局的 `main_align`。
///    `Layout::left_to_right(valign)` 的 `main_align` **默认是 Center**
///    （egui 注释：按钮里的字居中最好看），只传 `Align::LEFT` 当 valign 没用，
///    必须再 `.with_main_align(Align::Min)`。
/// ② `add_sized` 内部用 `centered_and_justified`（`main_align` 也是 Center），
///    会把上面设好的对齐冲掉。所以这里改成「先建一个指定尺寸的子 ui，
///    再在里头 add」，而不是 `add_sized`。
pub(crate) fn number_row(
	ui: &mut egui::Ui,
	width: f32,
	value: egui::DragValue<'_>,
) -> egui::Response {
	ui.allocate_ui_with_layout(
		egui::vec2(width, ROW_HEIGHT),
		egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Min),
		|ui| {
			// 宽度靠 `interact_size` 给：`DragValue` 里的 Button 用
			// `min_size(ui.spacing().interact_size)`，设好它就撑到 `width` 宽，
			// 又不会像 `add_sized` 那样把对齐方式换成「居中且铺满」。
			ui.spacing_mut().interact_size = egui::vec2(width, ROW_HEIGHT);
			// 数字的左内缩跟文本框对齐：按钮默认 `button_padding.x = 4`，
			// 同一列里「地址」的 10pt 与「端口」的 4pt 并排就会看出错位。
			ui.spacing_mut().button_padding.x = TEXT_PAD_X as f32;
			ui.add(value)
		},
	)
	.inner
}

/// 对话框内容区的左右内边距。
///
/// 窗口外框的内边距设成 0（见 [`dialog`]），由这里统一给。只有内边距归零，
/// 标题栏里的关闭键才可能真正贴住窗口右边缘。
pub(crate) const PAD_X: f32 = 14.0;

/// 对话框内容区的底部内边距。
const PAD_BOTTOM: f32 = 10.0;

/// 标题栏高度。
const TITLE_HEIGHT: f32 = 32.0;

/// 打开一个对话框：自绘标题栏 + 内容区。
///
/// 不用 egui 自带的标题栏，两个原因都在肉眼可见处：
///
/// ① 它把标题**居中**。对话框里其它文字（标签、说明、分节标题）全是左起的，
///    居中的标题单独跳出一套基准，看着像另一个体系。
/// ② 它给关闭键套了一圈 `Heading` 字高的内边距，再叠上外框的 8pt 内边距，
///    实测 × 距窗口右边缘 23pt。在 440 宽的对话框上，这个「悬空」很明显。
///
/// 所以外框内边距归零，标题栏与内容区各自管自己的内边距。
///
/// 位置基准取 `content_rect`（扣掉侧边栏等面板）的中心：对话框不该压住侧边栏。
pub(crate) fn dialog(
	ctx: &egui::Context,
	title: &str,
	size: [f32; 2],
	open: &mut bool,
	add_contents: impl FnOnce(&mut egui::Ui),
) {
	let frame = egui::Frame::window(&ctx.style_of(ctx.theme())).inner_margin(egui::Margin::ZERO);

	egui::Window::new(title)
		.pivot(egui::Align2::CENTER_CENTER)
		.default_pos(ctx.content_rect().center())
		// `fixed_size` 而不是 `default_size` + `min_size`：后两者都拦不住 egui
		// 把窗口收缩到内容高度，而内容里的 `ScrollArea` 又按「窗口给的高度」
		// 取上限——互相牵制会把内容裁在窗口底边。固定尺寸一次解决。
		.fixed_size(size)
		.resizable(false)
		.collapsible(false)
		.frame(frame)
		.title_bar(false)
		// 没有标题栏时 egui 会把**整块面板**都当作拖动区，与表单里的 `DragValue`
		// 拖动、指纹文字选择相互打架。对话框固定在中央，不需要拖动。
		.drag_area(egui::WindowDrag::Off)
		.show(ctx, |ui| {
			if title_bar(ui, title) {
				*open = false;
			}
			egui::Frame::NONE
				.inner_margin(egui::Margin {
					left: PAD_X as i8,
					right: PAD_X as i8,
					top: 0,
					bottom: PAD_BOTTOM as i8,
				})
				.show(ui, |ui| add_contents(ui));
		});
}

/// 自绘标题栏：标题左起、关闭键贴右边缘。返回是否点了关闭。
fn title_bar(ui: &mut egui::Ui, title: &str) -> bool {
	let mut closed = false;
	let width = ui.available_width();

	ui.allocate_ui_with_layout(
		egui::vec2(width, TITLE_HEIGHT),
		egui::Layout::left_to_right(egui::Align::Center),
		|ui| {
			ui.add_space(PAD_X);
			ui.add(
				egui::Label::new(egui::RichText::new(title).size(design::FONT_LG).strong())
					.selectable(false),
			);
			// 剩余宽度全部交给右对齐布局，关闭键才会落在最右边。
			ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
				closed = close_button(ui);
			});
		},
	);

	ui.separator();
	closed
}

/// 关闭键。与标题栏等高，右边缘与窗口边缘齐平。
fn close_button(ui: &mut egui::Ui) -> bool {
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(TITLE_HEIGHT, TITLE_HEIGHT), egui::Sense::click());

	let tokens = design::tokens(ui.ctx());
	if response.hovered() {
		// 悬停底色往里缩 4：贴着圆角窗口画一块方底色很丑
		ui.painter()
			.rect_filled(rect.shrink(4.0), design::RADIUS_SM, tokens.bg_hover);
	}
	let color = if response.hovered() {
		tokens.text
	} else {
		tokens.text_muted
	};
	design::icon_close(ui.painter(), rect.shrink(TITLE_HEIGHT * 0.33), color);

	response
		.on_hover_cursor(egui::CursorIcon::PointingHand)
		.on_hover_text("关闭")
		.clicked()
}

/// 分组在界面上的展示名：子分组写作「父 / 子」。
///
/// 只支持两层，平铺成一行才不会让下拉与右键菜单在分组多时失控。
/// 上级已不存在时（理论上外键挡住了）退化为分组自己的名字。
pub(crate) fn group_display_name(group: &Group, groups: &[Group]) -> String {
	match group
		.parent_id
		.and_then(|parent| groups.iter().find(|g| g.id == Some(parent)))
	{
		Some(parent) => format!("{} / {}", parent.name, group.name),
		None => group.name.clone(),
	}
}

// ---------- 表单行几何的回归钩子 ----------
//
// 「标签列宽度被内容收缩」这类问题肉眼很难查：每一行看起来都正常，
// 只有把十几行的左边缘**数值**放在一起比才看得出来。测试里没有别的办法
// 拿到 `Response.rect`，所以在 `field_ui` 里记一笔（只在测试构建里编译）。
//
// 限定 unix：承载它的用例要真起一个 `egui::Context`，而 Windows 侧的交叉
// `check --all-targets` 会把 `cfg(test)` 的代码也编一遍——用例被 `cfg(unix)`
// 摘掉之后，这套记录代码就成了「定义了没人用」，`-D warnings` 直接红。

#[cfg(all(test, unix))]
thread_local! {
	/// 每个表单行的 `(标签左边缘, 控件左边缘)`。
	static ROW_METRICS: std::cell::RefCell<Vec<(f32, f32)>> =
		const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(all(test, unix))]
fn record_row(label_left: f32, control_left: f32) {
	ROW_METRICS.with(|rows| rows.borrow_mut().push((label_left, control_left)));
}

/// 清空已记录的行位置（每个用例开头调一次）。
#[cfg(all(test, unix))]
pub(crate) fn reset_row_metrics() {
	ROW_METRICS.with(|rows| rows.borrow_mut().clear());
}

/// 取出已记录的行位置。
#[cfg(all(test, unix))]
pub(crate) fn row_metrics() -> Vec<(f32, f32)> {
	ROW_METRICS.with(|rows| rows.borrow().clone())
}
