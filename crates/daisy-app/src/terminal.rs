//! 终端视图：把 [`TerminalSession`] 接到 egui 上。
//!
//! 每帧的流程：
//!
//! 1. `pump` 抽取 PTY 输出喂给网格
//! 2. 按可用区域算出列数行数，必要时 resize
//! 3. 处理鼠标选区与右键菜单（复制 / 粘贴）
//! 4. 处理键盘与滚轮
//! 5. 画快照
//!
//! ## 选区与剪贴板
//!
//! 选区本身在 `daisy-term` 里（`TerminalGrid` 的 `start_selection` /
//! `update_selection` / `selection_text`），这一层只负责把 egui 的指针事件
//! 换算成格坐标，以及和系统剪贴板打交道。
//!
//! **右键菜单的「粘贴」有两条不同来源**（这是需求明确要求的）：
//!
//! | 情况 | 贴什么 | 菜单文案 |
//! | --- | --- | --- |
//! | 有选区 | **选中的那段文本**（把刚划出来的命令再跑一次） | 粘贴（选中内容） |
//! | 无选区 | 系统剪贴板 | 粘贴（剪贴板） |
//!
//! 键盘的 `⌘V` 不在此列——它走 `daisy_term` 的 `Event::Paste` 编码，
//! 永远是剪贴板。所以「有选区时也想贴剪贴板」是有路的：点一下画面清掉选区，
//! 或者直接用键盘。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use daisy_term::{
	CellMetrics, DEFAULT_SCROLLBACK, Palette, SessionLink, TerminalRenderer, TerminalSession,
};

use crate::error::Result;

/// 默认字号。
const DEFAULT_FONT_SIZE: f32 = 14.0;

/// 滚轮一格滚动多少行。
const SCROLL_LINES_PER_NOTCH: i32 = 3;

/// 光标闪烁周期。
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// 下一个终端视图的编号。
///
/// 每个终端要有一个**自己独占**的 egui 控件 id：不给的话 egui 按「同一父区域里
/// 第几个控件」自动编号，于是所有终端标签页拿到的是同一个 id——焦点会在标签之间
/// 互相冒充（A 标签的焦点状态被 B 标签的控件认领），关闭标签后焦点归属也变得
/// 无从判断。
static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

fn next_view_id() -> egui::Id {
	egui::Id::new("daisy_terminal").with(NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed))
}

/// 唤醒去重闸门：**一帧最多放行一次**。
///
/// 两头都要防：
///
/// - 不接唤醒 → 界面只能靠光标闪烁（500ms）或用户自己的鼠标/按键事件醒来，
///   远端输出平均要等 250ms、最坏 500ms 才上屏，手感就是「慢半拍」；
/// - 不压频率 → `yes`、`cat` 大文件这类输出一秒上千块，每块都
///   `request_repaint` 等于让后台线程把事件循环吵个上千次。
///
/// 语义：帧初 [`Self::reset`]，之后第一块数据 [`Self::try_pass`] 通过并申请一帧，
/// 同帧内其余的合并掉（数据仍在队列里，那一帧抽得到）。
#[derive(Clone, Default)]
struct WakeGate(Arc<AtomicBool>);

impl WakeGate {
	/// 本帧还没人叫过 → 返回 `true`，调用方负责申请重绘。
	fn try_pass(&self) -> bool {
		!self.0.swap(true, Ordering::AcqRel)
	}

	/// 帧开头调用，重新开闸。
	fn reset(&self) {
		self.0.store(false, Ordering::Release);
	}
}

/// 把「链路有新数据」接到 egui 的重绘上。
fn install_waker(session: &mut TerminalSession, ctx: &egui::Context) -> WakeGate {
	let gate = WakeGate::default();
	let hook = gate.clone();
	let ctx = ctx.clone();

	session.set_waker(Arc::new(move || {
		if hook.try_pass() {
			ctx.request_repaint();
		}
	}));

	gate
}

/// 会话来源。决定「结束时那句话」怎么写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
	/// 本地 shell（`$SHELL`）。结束的是本机进程。
	Local,
	/// SSH 远端 shell。结束的是连接。
	Remote,
}

impl Origin {
	/// 会话结束时要写进画面的提示。`None` = 还没结束，不提示。
	///
	/// 远端只说「断开」：退出码是 SSH 通道的语义（`-1` 表示没有退出状态），
	/// 摆给用户看只会让人以为是自己退出得不对。
	fn ended_notice(self, exited: bool, exit_code: Option<i32>) -> Option<String> {
		if !exited {
			return None;
		}
		Some(match self {
			Self::Remote => "连接已断开".to_string(),
			Self::Local => match exit_code {
				Some(code) => format!("进程已退出（{code}）"),
				None => "进程已退出".to_string(),
			},
		})
	}
}

/// 结束提示实际写进网格的字节。
///
/// 空一行是**按光标位置**补的：光标已经停在新行行首时补一个 `\r\n`，
/// 否则补两个。写死两个会在「shell 自己收过尾」的情况下多出一整行空白——
/// bash 与 zsh、登录 shell 与 `sh -c`，收尾有没有换行并不一致，
/// 同一条 `exit` 在两种壳下就会差一行。
///
/// 紧贴着最后一行写也不行：看起来像 shell 自己输出的内容，
/// 用户会以为还能继续敲命令。
fn ended_notice_bytes(notice: &str, cursor_at_line_start: bool) -> Vec<u8> {
	let mut bytes = Vec::with_capacity(notice.len() + 8);
	if cursor_at_line_start {
		bytes.extend_from_slice(b"\r\n");
	} else {
		bytes.extend_from_slice(b"\r\n\r\n");
	}
	bytes.extend_from_slice(notice.as_bytes());
	bytes.extend_from_slice(b"\r\n");
	bytes
}

/// 一个终端标签页。
pub struct TerminalView {
	session: TerminalSession,
	renderer: TerminalRenderer,
	/// 标签页标题，建视图时定下，之后不再变。
	///
	/// **不跟 shell 上报的标题走**（`OSC 0` / `OSC 2`）。远端一 `cd` 就把标题
	/// 改成 `root@host:~/cert/scskj.net`，同一台主机的标签横竖看不出来是哪个，
	/// 而且长度还随路径变。用户要的是「我连的是哪台服务器」，不是远端此刻在哪。
	title: String,
	/// 唤醒去重闸门，见 [`WakeGate`]。
	wake_gate: WakeGate,
	/// 会话来源，决定断开提示的措辞。
	origin: Origin,
	/// 结束提示是否已经写过——`is_exited()` 会一直为真，不记就每帧写一遍。
	notice_written: bool,
	/// 本视图独占的控件 id，见 [`next_view_id`]。
	id: egui::Id,
	/// 下一帧是否要主动拿键盘焦点。
	///
	/// **不能每帧无条件抢**：那会让用户点侧边栏搜索框、改设置时焦点被拽回终端，
	/// 输入框刚点上就失焦。所以只在「视图刚建好」和「用户切到本标签页」时置位，
	/// `ui()` 消费一次就清掉。
	pending_focus: bool,
	/// 上一次拖动落在哪一格。
	///
	/// 拖动时指针会在**同一格**里连发很多次 `dragged()`，每格只更新一次就够了；
	/// 更重要的是 `update_selection` 会重算整段选区，无谓调用会让长选区每次
	/// 拖动都重排一遍。
	last_drag_cell: Option<(usize, usize)>,
	/// 右键菜单这一帧是不是开着。
	///
	/// 开着的时候不能把这帧的按键喂给终端：用户按 Esc 是想关菜单，
	/// 不是想给远端发一个 ESC。
	menu_open: bool,
	/// 待上报给应用层的提示（剪贴板不可用之类）。
	///
	/// 视图自己弹不出 toast——那是 `DaisyApp` 的东西。攒在这里由
	/// `SessionManager` 收走，见 [`Self::take_notice`]。
	notice: Option<String>,
}

impl TerminalView {
	/// 启动一个本地 shell 会话。
	pub fn local_shell(ctx: &egui::Context) -> Result<Self> {
		let metrics = CellMetrics::measure(ctx, DEFAULT_FONT_SIZE);
		let (cols, rows) = metrics.grid_size(egui::Vec2::new(900.0, 600.0));

		let mut session = TerminalSession::local_shell(cols, rows)?;
		// PTY 读取线程在会话建好之前就起来了，所以是「后装」回调
		let wake_gate = install_waker(&mut session, ctx);

		Ok(Self {
			session,
			renderer: TerminalRenderer::new(metrics, Palette::daisy_dark()),
			title: "本地终端".to_owned(),
			wake_gate,
			origin: Origin::Local,
			notice_written: false,
			id: next_view_id(),
			// 刚建好就该能打字：启动应用、点「+ 新建终端」都不用再点一下画面
			pending_focus: true,
			last_drag_cell: None,
			menu_open: false,
			notice: None,
		})
	}

	/// 标签页标题。建视图时定下，之后不再变。
	#[must_use]
	pub fn title(&self) -> &str {
		&self.title
	}

	/// 挂到一个既有的字节链路上（SSH 远端 shell 走这条）。
	///
	/// 网格、渲染、输入编码与本地终端完全共用——差别只在字节从哪来。
	pub fn attach(ctx: &egui::Context, link: Box<dyn SessionLink>, title: &str) -> Result<Self> {
		let metrics = CellMetrics::measure(ctx, DEFAULT_FONT_SIZE);
		let (cols, rows) = metrics.grid_size(egui::Vec2::new(900.0, 600.0));

		let mut session = TerminalSession::attach(link, cols, rows, DEFAULT_SCROLLBACK)?;
		// SSH 搬运任务同样先于视图跑起来，回调在这里补装
		let wake_gate = install_waker(&mut session, ctx);

		Ok(Self {
			session,
			renderer: TerminalRenderer::new(metrics, Palette::daisy_dark()),
			title: title.to_owned(),
			wake_gate,
			// 走这条路的只有 SSH 远端 shell
			origin: Origin::Remote,
			notice_written: false,
			id: next_view_id(),
			// 连接过程可能要几秒，用户这段时间可能去点了别处；
			// 连上就直接能敲，不用再回来点一下画面
			pending_focus: true,
			last_drag_cell: None,
			menu_open: false,
			notice: None,
		})
	}

	/// 结束会话。让对端收到关闭，而不是只看到连接断开。
	pub fn close(&mut self) -> Result<bool> {
		Ok(self.session.kill()?)
	}

	/// 直接往链路写字节（无人值守验证钩子用，绕开键盘编码）。
	pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
		Ok(self.session.write(bytes)?)
	}

	/// 热更新外观：换配色与字号（设置页改动即时生效走这里）。
	///
	/// 下一帧的 `sync_size` 会按新字号重算网格尺寸并通知对端。
	pub fn apply_appearance(&mut self, ctx: &egui::Context, palette: Palette, font_size: f32) {
		self.renderer.set_palette(palette);
		self.renderer
			.set_metrics(CellMetrics::measure(ctx, font_size));
	}

	/// 只换配色。
	pub fn set_palette(&mut self, palette: Palette) {
		self.renderer.set_palette(palette);
	}

	/// 只重量单元格尺寸——**必须在新字体真正生效之后调用**。
	///
	/// `ctx.set_fonts` 不会立刻生效：egui 把它存进 `new_font_definitions`，
	/// 下一趟 `begin_pass` 才换。同一帧里量 `'M'` 量到的还是旧字体的宽度，
	/// 差出来的一档会一直用到下一次重量为止：整屏文字都按错位的列宽排，
	/// 越靠右偏得越多，宽字符之间被拉开两倍多。
	pub fn measure_cells(&mut self, ctx: &egui::Context, font_size: f32) {
		self.renderer
			.set_metrics(CellMetrics::measure(ctx, font_size));
	}

	/// 请求下一帧把键盘焦点拿过来。
	///
	/// 新建标签页、切回本标签页时调用。只置标志、不立刻请求——
	/// 控件要等下一帧 `ui()` 里 allocate 出来才有 id 可以聚焦。
	pub fn request_focus(&mut self) {
		self.pending_focus = true;
	}

	/// 绘制并处理交互。
	pub fn ui(&mut self, ui: &mut egui::Ui) {
		// 先清标志、再抽数据：这之后到达的每一块新输出都会重新申请一帧。
		// 顺序反过来会把「本帧刚抽过的数据」也算成新数据，白白多画一帧。
		self.wake_gate.reset();
		// 注意：这里**不读** `update.title`。远端 shell 会周期性上报 `OSC 0/2`
		// 标题（一 `cd` 就变），标签页要的是数据库里配的服务器名，不是远端路径。
		let update = self.session.pump();
		if update.exited.is_some() || self.session.is_exited() {
			self.write_ended_notice();
		}

		// 控件 id 由本视图自己定（见 `next_view_id`），所以整块绘制要包在这个 id 下，
		// 否则 `allocate_exact_size` 拿到的仍是「父区域里第几个控件」那个自动编号。
		let id = self.id;
		ui.push_id(id, |ui| self.paint(ui));
	}

	/// 真正的绘制与交互。id 由 [`Self::ui`] 的 `push_id` 划定。
	fn paint(&mut self, ui: &mut egui::Ui) {
		self.sync_size(ui);
		let (rect, response) = self.allocate(ui);

		// 新建 / 切回本标签页时主动收下键盘焦点，否则得先点一下画面才能打字
		if std::mem::take(&mut self.pending_focus) {
			response.request_focus();
		}
		if response.clicked() || response.drag_started() {
			response.request_focus();
		}

		// 选区与右键菜单都在键盘之前：右键菜单一旦打开，用户是在点菜单，
		// 不该再把这一帧的按键当成终端输入
		self.handle_selection(&response, rect);
		self.handle_context_menu(&response);

		if response.has_focus() {
			// 必须每帧设：`set_focus_lock_filter` 只认「上一帧和这一帧都持有焦点」
			// 的控件，设一次会被后续帧丢掉。
			lock_keys(ui.ctx(), response.id);
			if !self.menu_open {
				self.handle_keyboard(ui);
			}
		}
		self.handle_scroll(ui);

		let mut snapshot = self.session.snapshot();
		// 会话结束后不画光标：一块留在屏幕上的方块光标等于在催用户输入，
		// 而这条链路已经没人收了
		if self.session.is_exited() {
			snapshot.cursor = None;
		}

		if response.has_focus() {
			let caret = snapshot.cursor.map(|cursor| (cursor.row, cursor.col));
			request_ime(ui, rect, caret, self.renderer.metrics());
		}

		self.renderer.paint(ui.painter(), rect.min, &snapshot);

		self.paint_scroll_hint(ui, rect);

		if self.session.needs_animation() {
			ui.ctx().request_repaint_after(CURSOR_BLINK_INTERVAL);
		}
	}

	/// 会话结束时，把提示写进终端画面（只写一次）。
	///
	/// 以前只在右下角画一行小字：远端一断，用户看到的是一片静止的屏幕，
	/// 分不清是「命令还在跑」还是「连接没了」。
	fn write_ended_notice(&mut self) {
		if self.notice_written {
			return;
		}
		let Some(notice) = self
			.origin
			.ended_notice(self.session.is_exited(), self.session.exit_code())
		else {
			return;
		};
		// 光标停在行首说明当前行是空的，这一个空行就是要留的那一行
		let at_line_start = self
			.session
			.snapshot()
			.cursor
			.is_some_and(|cursor| cursor.col == 0);
		self.notice_written = true;
		self.session
			.inject(&ended_notice_bytes(&notice, at_line_start));
		tracing::info!("终端会话已结束，已在画面写入提示：{notice}");
	}

	/// 按可用区域调整网格尺寸。
	fn sync_size(&mut self, ui: &egui::Ui) {
		let available = ui.available_size();
		// 布局未定或尺寸异常时不调整，避免把网格压成 1×1
		if !available.x.is_finite() || !available.y.is_finite() || available.x < 1.0 {
			return;
		}

		let (cols, rows) = self.renderer.metrics().grid_size(available);
		if (cols, rows) != (self.session.cols(), self.session.rows())
			&& let Err(error) = self.session.resize(cols, rows)
		{
			tracing::warn!("调整终端尺寸失败：{error}");
		}
	}

	fn allocate(&self, ui: &mut egui::Ui) -> (egui::Rect, egui::Response) {
		let size = self.renderer.metrics().pixel_size(
			usize::from(self.session.cols()),
			usize::from(self.session.rows()),
		);
		ui.allocate_exact_size(size, egui::Sense::click_and_drag())
	}

	fn handle_keyboard(&mut self, ui: &egui::Ui) {
		let events = ui.input(|input| input.events.clone());
		if events.is_empty() {
			return;
		}

		// ⌘C（macOS）/ Ctrl+Shift+C（其它平台）：复制选区。
		//
		// `daisy_term::input` 里 `Event::Copy | Event::Cut => {}` 明确把这几个
		// 事件留给界面层——它认为「复制是应用的事」。这里不接就是彻底没反应。
		// 没有选区时什么都不做：`Ctrl+C` 该照常发中断（0x03），别抢。
		if events
			.iter()
			.any(|event| matches!(event, egui::Event::Copy))
			&& let Some(text) = self.selected_text()
		{
			self.copy_to_clipboard(&text);
		}

		if let Err(error) = self.session.handle_key_events(&events) {
			tracing::debug!("终端输入写入失败：{error}");
		}
	}

	// ==================== 选区与剪贴板 ====================

	/// 这一格是不是已经在选区内。
	///
	/// 用来判断「这一笔拖动是接在已有选区上，还是重开一笔」。
	fn covers(&self, row: usize, col: usize) -> bool {
		self.session
			.snapshot()
			.selection
			.is_some_and(|bounds| bounds.contains(row, col))
	}

	/// 当前选区的左上角（视口坐标）。没有选区时为 `None`。
	fn selection_start(&self) -> Option<(usize, usize)> {
		self.session.snapshot().selection.map(|bounds| bounds.start)
	}

	/// 当前选中的文本。空串按「没有选区」处理。
	fn selected_text(&self) -> Option<String> {
		self.session
			.grid()
			.selection_text()
			.filter(|text| !text.is_empty())
	}

	/// 拖动选字：按住左键划选、双击选词、三击选整行、单击清掉选区。
	fn handle_selection(&mut self, response: &egui::Response, rect: egui::Rect) {
		let metrics = self.renderer.metrics().cell_size();
		let cols = usize::from(self.session.cols());
		let rows = usize::from(self.session.rows());

		if response.drag_started() {
			// 起点必须取**按下那一刻**的位置。
			//
			// `drag_started()` 要等指针挪过阈值才为真，那一帧的
			// `interact_pointer_pos()` 已经是拖动后的位置了——拿它当起点，
			// 最前面几格会被吞掉（实测：拖 6 格只能选中后半截）。
			let origin = response
				.ctx
				.input(|input| input.pointer.press_origin())
				.or_else(|| response.interact_pointer_pos());

			if let Some(pointer) = origin {
				let (row, col) = cell_at(rect.min, metrics, cols, rows, pointer);

				// 锚点：按在已有选区**里面**就沿用它的左上角，按在外面就以
				// 按下这一格为锚点（新的划选）。
				//
				// 这里必须重建成 `Simple` 选区。双击给的是 `Semantic`、三击给的是
				// `Lines`，只要把这两种语义留着，拖动就会按整词 / 整行跳——
				// 想给「双击选中的词」后面再补两个字符都做不到。改成锚点 + 终点
				// 之后，拖动一律逐字符增减，同时整词仍然是双击那一笔选出来的。
				let anchor = self
					.selection_start()
					.filter(|_| self.covers(row, col))
					.unwrap_or((row, col));
				self.session.grid_mut().start_selection(anchor.0, anchor.1);

				// 同一帧里 `dragged()` 也会为真，先把终点推到当前指针。
				// 不推的话选区会停在锚点上（长度 0），而 `last_drag_cell` 又已经
				// 占位，下面那段更新会被跳过——表现就是「这一下拖动没反应」。
				let current = response.interact_pointer_pos().unwrap_or(pointer);
				let (end_row, end_col) = cell_at(rect.min, metrics, cols, rows, current);
				self.session.grid_mut().update_selection(end_row, end_col);
				self.last_drag_cell = Some((end_row, end_col));
			}
		}

		// 不能写成 `else if`：egui 常常在**同一帧**里既报 drag_started
		// 又报 dragged。早退就会停在「起点=终点」，选区长度为 0，
		// `selection_text()` 直接返回 `None`——表现为怎么拖都选不上。
		if response.dragged()
			&& let Some(pointer) = response.interact_pointer_pos()
		{
			let (row, col) = cell_at(rect.min, metrics, cols, rows, pointer);
			if self.last_drag_cell != Some((row, col)) {
				self.session.grid_mut().update_selection(row, col);
				self.last_drag_cell = Some((row, col));
			}
		}

		if response.dragged() || response.drag_started() {
			return;
		}

		self.last_drag_cell = None;

		// 三击 → 整行；双击 → 整词；单击 → 清干净。
		// 判断顺序不能反：双击那一帧 `clicked()` 也为真。
		if response.triple_clicked() {
			if let Some(pointer) = response.interact_pointer_pos() {
				let (row, _) = cell_at(rect.min, metrics, cols, rows, pointer);
				self.session.grid_mut().select_line(row);
			}
		} else if response.double_clicked() {
			if let Some(pointer) = response.interact_pointer_pos() {
				let (row, col) = cell_at(rect.min, metrics, cols, rows, pointer);
				self.session.grid_mut().select_word(row, col);
			}
		} else if response.clicked() {
			self.session.grid_mut().clear_selection();
		}
	}

	/// 右键菜单：复制 / 粘贴。
	///
	/// 粘贴的语义按要求分两种（菜单文案如实写出，避免用户以为意图落空）：
	///
	/// - **有选区** → 把选中的那段贴进当前 shell。用途是把刚划出来的命令
	///   再执行一次，所以它贴的不是剪贴板；
	/// - **无选区** → 贴系统剪贴板。
	///
	/// 任何时候想贴剪贴板，先点一下画面清掉选区，或者直接用键盘粘贴。
	fn handle_context_menu(&mut self, response: &egui::Response) {
		let shown = response.clone().context_menu(|ui| {
			crate::views::menu(ui, |ui| {
				let selected = self.selected_text();
				let has_selection = selected.is_some();

				if ui
					.add_enabled(has_selection, egui::Button::new("复制"))
					.on_hover_text("把选中的文本复制到系统剪贴板")
					.clicked()
				{
					if let Some(text) = selected.as_deref() {
						self.copy_to_clipboard(text);
					}
					// 点完菜单焦点会跑到菜单上，这里补回终端，否则要点一下画面才能继续打字
					self.pending_focus = true;
					ui.close();
				}

				let label = if has_selection {
					"粘贴（选中内容）"
				} else {
					"粘贴（剪贴板）"
				};
				if ui
					.button(label)
					.on_hover_text(if has_selection {
						"把当前选中的文本粘贴进 shell"
					} else {
						"把系统剪贴板的内容粘贴进 shell"
					})
					.clicked()
				{
					self.paste_from_menu();
					self.pending_focus = true;
					ui.close();
				}
			});
		});

		self.menu_open = shown.is_some();
	}

	/// 写系统剪贴板。失败只有一种可见反馈方式：攒一条提示交给应用层弹 toast。
	fn copy_to_clipboard(&mut self, text: &str) {
		match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
			Ok(()) => tracing::debug!("已复制 {} 字到剪贴板", text.chars().count()),
			Err(error) => {
				tracing::warn!("写剪贴板失败：{error}");
				self.notice = Some(format!("复制失败：{error}"));
			},
		}
	}

	/// 右键菜单里的「粘贴」：有选区贴选区，没选区贴剪贴板。
	fn paste_from_menu(&mut self) {
		let Some(text) = paste_payload(self.selected_text(), clipboard_text) else {
			// 静默失败最难受：用户以为点坏了。
			self.notice = Some("剪贴板为空，没有可粘贴的内容".to_string());
			return;
		};

		// 走 `session.paste` 而不是直接 write：终端处于括号粘贴模式时
		// 它会自己加 `ESC[200~ / ESC[201~`，多行内容不会被 shell 逐行执行。
		if let Err(error) = self.session.paste(&text) {
			tracing::warn!("粘贴到终端失败：{error}");
			self.notice = Some(format!("粘贴失败：{error}"));
		}
	}

	/// 取走一条待上报的提示。应用层每帧调一次，转成 toast。
	pub fn take_notice(&mut self) -> Option<String> {
		self.notice.take()
	}

	fn handle_scroll(&mut self, ui: &egui::Ui) {
		// egui 0.36 只公开了平滑后的滚动量，原始值对外不可见。
		// 对终端来说平滑滚动反而更自然，直接用即可。
		let delta = ui.input(|input| input.smooth_scroll_delta.y);
		if delta == 0.0 {
			return;
		}

		// 向上滚 = 看历史，方向与 `scroll` 的语义一致
		let lines = if delta > 0.0 {
			SCROLL_LINES_PER_NOTCH
		} else {
			-SCROLL_LINES_PER_NOTCH
		};
		self.session.scroll(lines);
	}

	/// 右下角显示回滚位置，避免用户以为终端卡住了。
	fn paint_scroll_hint(&self, ui: &egui::Ui, rect: egui::Rect) {
		let offset = self.session.display_offset();
		if offset == 0 {
			return;
		}

		let painter = ui.painter();
		let font = egui::FontId::proportional(12.0);
		let color = ui.visuals().weak_text_color();

		let anchor = egui::Pos2::new(rect.right() - 8.0, rect.bottom() - 6.0);
		painter.text(
			anchor,
			egui::Align2::RIGHT_BOTTOM,
			format!("回滚 {offset} 行 · 滚到底部继续"),
			font,
			color,
		);
	}
}

/// 告诉系统输入法「文字要落在这儿」。
///
/// egui 只在 `PlatformOutput::ime` 有值时才让 winit 调 `set_ime_allowed(true)`。
/// 不设的话，中文输入法敲出来的字符根本到不了应用——macOS 把按键交给输入法后，
/// 应用这边既收不到 `Event::Text` 也收不到有意义的 `Event::Key`，
/// 表现就是「切了输入法也打不出一个字」。`TextEdit` 自己会设这一项，
/// 终端是自绘控件，得手动补上。
fn request_ime(
	ui: &egui::Ui,
	rect: egui::Rect,
	caret: Option<(usize, usize)>,
	metrics: &CellMetrics,
) {
	let cell = metrics.cell_size();
	// 候选窗贴着光标格弹出；没有光标就用整个画面，至少别飘到屏幕角落
	let caret_rect = caret.map_or(rect, |(row, col)| {
		egui::Rect::from_min_size(
			rect.min + egui::vec2(col as f32 * cell.x, row as f32 * cell.y),
			cell,
		)
	});

	// 与 `TextEdit` 同一套换算：所在图层可能带着平移
	let to_global = ui
		.ctx()
		.layer_transform_to_global(ui.layer_id())
		.unwrap_or_default();

	ui.output_mut(|output| {
		output.ime = Some(egui::output::IMEOutput {
			purpose: egui::IMEPurpose::Normal,
			rect: to_global * rect,
			cursor_rect: to_global * caret_rect,
			should_interrupt_composition: false,
		});
	});
}

/// 把 Tab / 方向键 / Esc 锁给终端，不让 egui 拿去遍历控件。
///
/// 不锁的后果不是「少一个键」，而是**整个键盘失灵**：
///
/// - `Tab` 被 egui 当成「下一个控件」→ 焦点当场离开终端。用户按 Tab 想补全，
///   看到的却是「没补全」，接着敲什么都进不去（终端已经不是焦点控件了）；
/// - `Escape` 更直接：egui 收到它就 `focused_widget = None`，焦点彻底悬空。
///
/// 方向键同理——shell 的行内编辑、补全菜单翻页全靠它们，被拿去挪焦点同样是哑的。
///
/// `set_focus_lock_filter` 内部要求「上一帧与这一帧都持有焦点」，所以**必须每帧调**。
fn lock_keys(ctx: &egui::Context, id: egui::Id) {
	ctx.memory_mut(|memory| {
		memory.set_focus_lock_filter(
			id,
			egui::EventFilter {
				tab: true,
				horizontal_arrows: true,
				vertical_arrows: true,
				escape: true,
			},
		);
	});
}

/// 指针位置 → 视口内的 `(行, 列)`。
///
/// 画面从 `origin` 起按 `cell` 尺寸一格一格铺，所以直接除就够了。
///
/// **越界一律夹到边界**，不返回 `None`：拖到画面外还要继续选是常见操作
/// （选到行尾、一直拖到最后一行），夹住比「拖出去就断了」符合手感。
fn cell_at(
	origin: egui::Pos2,
	cell: egui::Vec2,
	cols: usize,
	rows: usize,
	pointer: egui::Pos2,
) -> (usize, usize) {
	let cols = cols.max(1);
	let rows = rows.max(1);
	let relative = pointer - origin;

	// 浮点转整数是饱和转换：负的坐标会变成 0，不会绕成一个巨大的下标
	let col = (relative.x / cell.x).floor().max(0.0) as usize;
	let row = (relative.y / cell.y).floor().max(0.0) as usize;

	(row.min(rows - 1), col.min(cols - 1))
}

/// 决定这次「粘贴」贴什么。
///
/// 规则来自需求本身，也决定了右键菜单上那行文案：
///
/// 1. **有选区** → 贴选中的那段。用途是把刚划出来的命令再执行一次；
/// 2. **没有选区** → 贴系统剪贴板；
/// 3. 两者都没有 → `None`，调用方负责给出提示。
///
/// 抽成纯函数是为了能不起剪贴板也能测——真实剪贴板在 CI 上不可控。
fn paste_payload(
	selected: Option<String>,
	from_clipboard: impl FnOnce() -> Option<String>,
) -> Option<String> {
	match selected {
		Some(text) if !text.is_empty() => Some(text),
		_ => from_clipboard().filter(|text| !text.is_empty()),
	}
}

/// 读系统剪贴板。
///
/// 拿不到一律返回 `None`——**剪贴板为空在 macOS 上本身就是一条错误**
/// （arboard 报 `ContentNotAvailable`），不必替用户区分「空」与「坏」。
fn clipboard_text() -> Option<String> {
	match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
		Ok(text) => Some(text),
		Err(error) => {
			tracing::debug!("读取剪贴板失败（剪贴板为空时也会这样）：{error}");
			None
		},
	}
}

#[cfg(test)]
// `screen_text` 只被拉起真实 shell 的用例用到（`/bin/sh`），Windows 目标下
// 没被引用——同 daisy-term，这是平台差异不是死代码。
#[cfg_attr(windows, allow(dead_code, unused_imports))]
mod tests {
	use std::time::{Duration, Instant};

	use super::*;

	/// 把快照拍平成文本，方便断言「屏幕上有这句话」。
	fn screen_text(view: &TerminalView) -> String {
		let snapshot = view.session.snapshot();
		let mut out = String::new();
		for row in 0..snapshot.rows {
			for col in 0..snapshot.cols {
				if let Some(cell) = snapshot.cell(row, col)
					&& !cell.flags.wide_spacer
				{
					out.push(cell.ch);
				}
			}
			out.push('\n');
		}
		out
	}

	/// 指针落在第 3 行第 5 格，应当换算成 `(2, 4)`。
	#[test]
	fn cell_at_maps_pointer_to_grid() {
		let cell = egui::vec2(8.0, 16.0);
		let origin = egui::pos2(100.0, 50.0);

		// 正落在第 2 行第 4 列的左上角
		let pointer = egui::pos2(100.0 + 4.0 * 8.0, 50.0 + 2.0 * 16.0);
		assert_eq!(cell_at(origin, cell, 80, 24, pointer), (2, 4));

		// 同一格内偏右下一点，仍是同一格
		let pointer = egui::pos2(100.0 + 4.0 * 8.0 + 7.0, 50.0 + 2.0 * 16.0 + 15.0);
		assert_eq!(cell_at(origin, cell, 80, 24, pointer), (2, 4));
	}

	/// 拖出画面不该断：越界夹到最后一格 / 第 0 格。
	#[test]
	fn cell_at_clamps_outside_the_grid() {
		let cell = egui::vec2(8.0, 16.0);
		let origin = egui::pos2(100.0, 50.0);

		// 右下角外面很远
		let pointer = egui::pos2(9_999.0, 9_999.0);
		assert_eq!(cell_at(origin, cell, 80, 24, pointer), (23, 79));

		// 左上角外面（负偏移不能绕成巨大下标）
		let pointer = egui::pos2(0.0, 0.0);
		assert_eq!(cell_at(origin, cell, 80, 24, pointer), (0, 0));
	}

	/// 有选区时，右键菜单的「粘贴」贴的是**选区**，不是剪贴板。
	///
	/// 这是本次需求的明确要求：把刚划出来的一段命令再执行一次。
	#[test]
	fn paste_prefers_the_selection_over_the_clipboard() {
		let mut asked_clipboard = false;
		let payload = paste_payload(Some("ls -l".to_string()), || {
			asked_clipboard = true;
			Some("从剪贴板来".to_string())
		});

		assert_eq!(payload.as_deref(), Some("ls -l"));
		assert!(!asked_clipboard, "有选区时不该去碰剪贴板");
	}

	#[test]
	fn paste_falls_back_to_the_clipboard_without_a_selection() {
		let payload = paste_payload(None, || Some("从剪贴板来".to_string()));
		assert_eq!(payload.as_deref(), Some("从剪贴板来"));
	}

	/// 空选区（拖了 0 格）等同于没有选区。
	#[test]
	fn an_empty_selection_falls_back_to_the_clipboard() {
		let payload = paste_payload(Some(String::new()), || Some("从剪贴板来".to_string()));
		assert_eq!(payload.as_deref(), Some("从剪贴板来"));
	}

	/// 两边都空：返回 `None`，由调用方提示「剪贴板为空」。
	#[test]
	fn paste_without_selection_or_clipboard_yields_nothing() {
		assert_eq!(paste_payload(None, || None), None);
		assert_eq!(paste_payload(None, || Some(String::new())), None);
	}

	// ---------- 鼠标事件的合成工具 ----------
	//
	// 选区的接线只能靠真事件驱动才测得出来：指针位置、拖动阈值、双击判定
	// 全在 egui 内部，纯函数测不到。这三个小工具把「屏幕尺寸 + 事件序列」
	// 固定下来，测试里只关心坐标。

	/// 测试用的屏幕尺寸。终端铺满它，左上角即原点。
	#[cfg(unix)]
	fn test_screen() -> Option<egui::Rect> {
		Some(egui::Rect::from_min_size(
			egui::Pos2::ZERO,
			egui::vec2(900.0, 600.0),
		))
	}

	/// 第 `col` 列、第 `row` 行中心点的屏幕坐标。
	#[cfg(unix)]
	fn cell_point(view: &TerminalView, col: usize, row: usize) -> egui::Pos2 {
		let cell = view.renderer.metrics().cell_size();
		egui::pos2(cell.x * (col as f32 + 0.5), cell.y * (row as f32 + 0.5))
	}

	/// 合成一次单击（按下 + 抬起，位置不变）。连着调两次就是双击。
	#[cfg(unix)]
	fn synthesize_click(ctx: &egui::Context, view: &mut TerminalView, point: egui::Pos2) {
		let modifiers = egui::Modifiers::default();
		let mut input = egui::RawInput {
			events: vec![
				egui::Event::PointerMoved(point),
				egui::Event::PointerButton {
					pos: point,
					button: egui::PointerButton::Primary,
					pressed: true,
					modifiers,
				},
				egui::Event::PointerButton {
					pos: point,
					button: egui::PointerButton::Primary,
					pressed: false,
					modifiers,
				},
			],
			..Default::default()
		};
		input.screen_rect = test_screen();
		let mut output = ctx.run_ui(input, |ui| view.ui(ui));
		output.textures_delta.clear();
	}

	/// 合成一次右键点击。用来验「右键能不能弹出菜单」这条纯接线。
	#[cfg(unix)]
	fn synthesize_right_click(ctx: &egui::Context, view: &mut TerminalView, point: egui::Pos2) {
		let modifiers = egui::Modifiers::default();
		let mut input = egui::RawInput {
			events: vec![
				egui::Event::PointerMoved(point),
				egui::Event::PointerButton {
					pos: point,
					button: egui::PointerButton::Secondary,
					pressed: true,
					modifiers,
				},
				egui::Event::PointerButton {
					pos: point,
					button: egui::PointerButton::Secondary,
					pressed: false,
					modifiers,
				},
			],
			..Default::default()
		};
		input.screen_rect = test_screen();
		let mut output = ctx.run_ui(input, |ui| view.ui(ui));
		output.textures_delta.clear();
	}

	/// 右键真的能弹出菜单。
	///
	/// 这条以前只能靠手点：`context_menu` 要等 egui 判定「在控件上右键抬起」才
	/// 肯开，而菜单是浮层，`menu_open` 又是从它的返回值推出来的。合成事件能
	/// 把这条链一次跑通，顺便挡住「不小心把 `Sense` 改成 `hover`」这类改坏。
	#[cfg(unix)]
	#[test]
	fn right_click_opens_the_context_menu() {
		let (ctx, mut view) = terminal_with(b"hello world\r\n");
		assert!(!view.menu_open, "前置：一开始不该有菜单");

		let point = cell_point(&view, 3, 0);
		synthesize_right_click(&ctx, &mut view, point);

		assert!(view.menu_open, "右键应当弹出菜单");
	}

	/// 右键不能清掉选区——菜单上的「复制」与「粘贴（选中内容）」都靠它。
	#[cfg(unix)]
	#[test]
	fn right_click_keeps_the_selection() {
		let (ctx, mut view) = terminal_with(b"hello world\r\n");

		view.session.grid_mut().start_selection(0, 0);
		view.session.grid_mut().update_selection(0, 4);
		assert_eq!(view.selected_text().as_deref(), Some("hello"));

		let point = cell_point(&view, 3, 0);
		synthesize_right_click(&ctx, &mut view, point);

		assert_eq!(
			view.selected_text().as_deref(),
			Some("hello"),
			"右键之后选区应当还在（快照：{:?}）",
			screen_text(&view)
		);
	}

	/// 合成一次拖动：按下 → 移动（越过 drag 阈值）→ 抬起。
	#[cfg(unix)]
	fn synthesize_drag(
		ctx: &egui::Context,
		view: &mut TerminalView,
		from: egui::Pos2,
		to: egui::Pos2,
	) {
		let modifiers = egui::Modifiers::default();
		let press = egui::Event::PointerButton {
			pos: from,
			button: egui::PointerButton::Primary,
			pressed: true,
			modifiers,
		};
		let release = egui::Event::PointerButton {
			pos: to,
			button: egui::PointerButton::Primary,
			pressed: false,
			modifiers,
		};

		for events in [
			vec![egui::Event::PointerMoved(from), press],
			vec![egui::Event::PointerMoved(to)],
			vec![release],
		] {
			let mut input = egui::RawInput {
				events,
				..Default::default()
			};
			input.screen_rect = test_screen();
			let mut output = ctx.run_ui(input, |ui| view.ui(ui));
			output.textures_delta.clear();
		}
	}

	/// 起一个本地终端并在第 0 行灌入一段文本。
	///
	/// **每一帧都要给同一个 `screen_rect`**。`RawInput::default()` 不带屏幕尺寸，
	/// egui 会按 10000×10000 的无限画布排版，终端被算成 1185×625；等到后面
	/// 某一帧给了 900×600，`sync_size` 就会触发一次 resize——而 alacritty 的
	/// resize 会清掉选区。这个不一致曾让「右键是否清空选区」的用例误报成 bug。
	#[cfg(unix)]
	fn terminal_with(text: &[u8]) -> (egui::Context, TerminalView) {
		let ctx = egui::Context::default();

		let warmup_input = egui::RawInput {
			screen_rect: test_screen(),
			..Default::default()
		};
		let mut warmup = ctx.run_ui(warmup_input, |_ui| {});
		warmup.textures_delta.clear();

		let mut view = TerminalView::local_shell(&ctx).expect("启动本地终端失败");

		let first_input = egui::RawInput {
			screen_rect: test_screen(),
			..Default::default()
		};
		let mut first = ctx.run_ui(first_input, |ui| view.ui(ui));
		first.textures_delta.clear();

		// 直接灌进网格：不依赖 shell 什么时候打印提示符
		view.session.inject(text);
		let pump_input = egui::RawInput {
			screen_rect: test_screen(),
			..Default::default()
		};
		let mut pump = ctx.run_ui(pump_input, |ui| view.ui(ui));
		pump.textures_delta.clear();

		(ctx, view)
	}

	/// 双击选中落点所在的那一段文本。
	#[cfg(unix)]
	#[test]
	fn double_click_selects_the_word() {
		let (ctx, mut view) = terminal_with(b"echo hello world\r\n");

		// 落在 "hello" 中间
		let point = cell_point(&view, 6, 0);
		synthesize_click(&ctx, &mut view, point);
		synthesize_click(&ctx, &mut view, point);

		assert_eq!(
			view.selected_text().as_deref(),
			Some("hello"),
			"双击应选中整词（快照：{:?}）",
			screen_text(&view)
		);
	}

	/// 双击之后再拖：**逐字符**延伸，不按整词跳。
	///
	/// 整词只在双击那一笔生效；一旦开始拖，终点就跟着指针一格一格走。
	/// 以前是把 alacritty 的 `Semantic` 语义留着让终点自己扩，结果拖起来
	/// 是一次跳一个词，想补两个字符都做不到（用户实测反馈的正是这条）。
	///
	/// 期望值：锚点仍是 "hello" 的起点（第 5 列），终点到第 14 列 →
	/// "hello worl"，而不是按词补全的 "hello world"。
	#[cfg(unix)]
	#[test]
	fn dragging_after_a_double_click_moves_by_character() {
		let (ctx, mut view) = terminal_with(b"echo hello world\r\n");

		let start = cell_point(&view, 6, 0);
		synthesize_click(&ctx, &mut view, start);
		synthesize_click(&ctx, &mut view, start);
		assert_eq!(view.selected_text().as_deref(), Some("hello"));

		// 从 "hello" 上按下，拖到 "world" 中间（第 14 列）
		let end = cell_point(&view, 14, 0);
		synthesize_drag(&ctx, &mut view, start, end);

		assert_eq!(
			view.selected_text().as_deref(),
			Some("hello worl"),
			"双击后拖动应逐字符延伸（若按整词跳会得到 \"hello world\"）。快照：{:?}",
			screen_text(&view)
		);
	}

	/// 双击之后再往回拖：同样是逐字符**收缩**。
	#[cfg(unix)]
	#[test]
	fn dragging_back_after_a_double_click_shrinks_by_character() {
		let (ctx, mut view) = terminal_with(b"echo hello world\r\n");

		let word = cell_point(&view, 6, 0);
		synthesize_click(&ctx, &mut view, word);
		synthesize_click(&ctx, &mut view, word);
		assert_eq!(view.selected_text().as_deref(), Some("hello"));

		// 在 "hello" 里（第 8 列）按下，往左拖到第 6 列
		let press = cell_point(&view, 8, 0);
		let end = cell_point(&view, 6, 0);
		synthesize_drag(&ctx, &mut view, press, end);

		assert_eq!(
			view.selected_text().as_deref(),
			Some("he"),
			"往回拖也应当逐字符收缩（快照：{:?}）",
			screen_text(&view)
		);
	}

	/// 在别处按下拖动 = 重开一笔按格选区，不能接着扩之前那个词。
	#[cfg(unix)]
	#[test]
	fn dragging_elsewhere_starts_a_fresh_selection() {
		let (ctx, mut view) = terminal_with(b"echo hello world\r\n");

		let hello = cell_point(&view, 6, 0);
		synthesize_click(&ctx, &mut view, hello);
		synthesize_click(&ctx, &mut view, hello);

		// 从第 0 列划到第 3 列：正好覆盖 "echo"（选区含终点那一格）
		let echo = cell_point(&view, 0, 0);
		let mid = cell_point(&view, 3, 0);
		synthesize_drag(&ctx, &mut view, echo, mid);

		assert_eq!(
			view.selected_text().as_deref(),
			Some("echo"),
			"按在选区外拖动应重开一笔（快照：{:?}）",
			screen_text(&view)
		);
	}

	/// 端到端：鼠标拖过画面就产生选区。
	///
	/// 这条测的是**接线**，不是算法：`grid.start_selection/update_selection`
	/// 在 daisy-term 里已有单测，但「egui 的指针事件到底有没有接上去」只有
	/// 喂真事件才看得出来。
	#[cfg(unix)]
	#[test]
	fn dragging_the_mouse_selects_text() {
		let (ctx, mut view) = terminal_with(b"hello world\r\n");

		let start = cell_point(&view, 0, 0);
		let end = cell_point(&view, 4, 0);
		synthesize_drag(&ctx, &mut view, start, end);

		assert_eq!(
			view.selected_text().as_deref(),
			Some("hello"),
			"拖过哪几格就该选中哪几格（快照：{:?}）",
			screen_text(&view)
		);
	}

	/// 单击清掉选区：换个位置重新划，不该带着上一次的残留。
	#[cfg(unix)]
	#[test]
	fn clicking_clears_the_selection() {
		let (ctx, mut view) = terminal_with(b"hello world\r\n");

		// 先造一个选区（不经鼠标，避免依赖拖动阈值）
		view.session.grid_mut().start_selection(0, 0);
		view.session.grid_mut().update_selection(0, 4);
		assert!(view.selected_text().is_some(), "前置：应当有选区");

		let point = cell_point(&view, 2, 0);
		synthesize_click(&ctx, &mut view, point);

		assert!(view.selected_text().is_none(), "单击之后选区应当被清掉");
	}

	/// 只记录写入字节的链路。
	struct RecordingLink {
		written: Arc<std::sync::Mutex<Vec<u8>>>,
	}

	impl daisy_term::SessionLink for RecordingLink {
		fn try_recv(&mut self) -> Option<daisy_term::LinkEvent> {
			None
		}

		fn write(&mut self, data: &[u8]) -> daisy_term::Result<()> {
			self.written
				.lock()
				.unwrap_or_else(std::sync::PoisonError::into_inner)
				.extend_from_slice(data);
			Ok(())
		}

		fn resize(&mut self, _cols: u16, _rows: u16) -> daisy_term::Result<()> {
			Ok(())
		}

		fn shutdown(&mut self) -> daisy_term::Result<bool> {
			Ok(true)
		}
	}

	/// 建一个挂着假链路的终端，并跑一帧让它拿到键盘焦点。
	///
	/// 假链路的意义：能直接断言「写了哪几个字节」——不用等 shell 回显，
	/// 也不受提示符、行规程（有没有 `echo`、是不是 raw 模式）干扰。
	#[cfg(unix)]
	fn attached_view(ctx: &egui::Context) -> (TerminalView, Arc<std::sync::Mutex<Vec<u8>>>) {
		let written = Arc::new(std::sync::Mutex::new(Vec::new()));
		let link = RecordingLink {
			written: Arc::clone(&written),
		};
		let mut view = TerminalView::attach(ctx, Box::new(link), "假链路").expect("挂载失败");

		// 空跑三帧，让「焦点」和「焦点锁」都真正就位：
		// 第 1 帧只登记 `request_focus`，第 2 帧才拿到焦点，
		// 而 `set_focus_lock_filter` 还要求「上一帧也持有焦点」——第 3 帧才装得上。
		for _ in 0..3 {
			let mut output = ctx.run_ui(
				egui::RawInput {
					screen_rect: test_screen(),
					..Default::default()
				},
				|ui| view.ui(ui),
			);
			output.textures_delta.clear();
		}

		(view, written)
	}

	/// 取走链路里收到的字节。
	#[cfg(unix)]
	fn written_bytes(written: &Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<u8> {
		written
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.clone()
	}

	/// 合成一帧，事件就是 `events`。
	#[cfg(unix)]
	fn pass_with_events(
		ctx: &egui::Context,
		view: &mut TerminalView,
		events: Vec<egui::Event>,
	) {
		let input = egui::RawInput {
			screen_rect: test_screen(),
			events,
			..Default::default()
		};
		let mut output = ctx.run_ui(input, |ui| view.ui(ui));
		output.textures_delta.clear();
	}

	/// 一次 key-down。
	#[cfg(unix)]
	fn key_down(key: egui::Key, modifiers: egui::Modifiers, repeat: bool) -> egui::Event {
		egui::Event::Key {
			key,
			physical_key: Some(key),
			pressed: true,
			repeat,
			modifiers,
		}
	}

	/// 端到端：右键菜单的「粘贴」在有选区时，**把选中的那段原样写进链路**。
	///
	/// 用假链路而不是真 shell：这样能直接断言「写了哪几个字节」，
	/// 不用等终端回显、也不受 shell 提示符干扰。
	#[test]
	fn pasting_the_selection_writes_it_to_the_link() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let written = Arc::new(std::sync::Mutex::new(Vec::new()));
		let link = RecordingLink {
			written: Arc::clone(&written),
		};
		let mut view = TerminalView::attach(&ctx, Box::new(link), "假链路").expect("挂载失败");

		// 画面内容直接灌进网格（不经过链路），再选中前 4 个字符
		view.session.inject(b"echo hi\r\n");
		view.session.grid_mut().start_selection(0, 0);
		view.session.grid_mut().update_selection(0, 3);
		assert_eq!(view.selected_text().as_deref(), Some("echo"));

		view.paste_from_menu();

		let payload = written
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner)
			.clone();
		assert_eq!(
			String::from_utf8_lossy(&payload),
			"echo",
			"应当把选中的文本写进链路，而不是剪贴板里的东西"
		);
	}

	// ---------- 键盘：Tab 自动补全 / 长按退格 ----------
	//
	// 这两条都是「事件到底有没有走到链路」的接线测试：编码规则在
	// `daisy_term::input` 里已有单测，但 egui 会不会先把按键截胡（拿去遍历
	// 控件、取消焦点）只有喂真事件才看得出来。

	/// Tab 必须原样送进 shell（那是自动补全），且不能把键盘焦点带走。
	///
	/// 不锁焦点的话 egui 会拿 Tab 去做「下一个控件」，终端当场失焦：
	/// 用户看到的是「Tab 不补全」，接下来敲什么都进不去。
	#[cfg(unix)]
	#[test]
	fn tab_reaches_the_shell_and_keeps_the_focus() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let (mut view, written) = attached_view(&ctx);
		let focused_before = ctx.memory(|memory| memory.focused());
		assert!(focused_before.is_some(), "前置：终端应当已经拿到焦点");

		pass_with_events(
			&ctx,
			&mut view,
			vec![key_down(egui::Key::Tab, egui::Modifiers::default(), false)],
		);

		assert_eq!(
			written_bytes(&written),
			b"\t",
			"Tab 应当原样进 shell（自动补全靠它）"
		);
		assert_eq!(
			ctx.memory(|memory| memory.focused()),
			focused_before,
			"Tab 不该把焦点从终端带走——带走了之后所有输入都会失效"
		);
	}

	/// Shift+Tab 在终端里是 `CSI Z`（反向补全），同样不能被当成「上一个控件」。
	#[cfg(unix)]
	#[test]
	fn shift_tab_stays_in_the_terminal() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let (mut view, written) = attached_view(&ctx);
		let focused_before = ctx.memory(|memory| memory.focused());

		let shift = egui::Modifiers {
			shift: true,
			..egui::Modifiers::default()
		};
		pass_with_events(&ctx, &mut view, vec![key_down(egui::Key::Tab, shift, false)]);

		assert_eq!(written_bytes(&written), b"\x1b[Z");
		assert_eq!(ctx.memory(|memory| memory.focused()), focused_before);
	}

	/// 长按退格：egui 会把「同一个键的第二次及以后」标成 `repeat`，
	/// 这些重复事件**每一个都要产生一个 DEL**，不能只认第一次。
	#[cfg(unix)]
	#[test]
	fn holding_backspace_keeps_sending_del() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let (mut view, written) = attached_view(&ctx);

		// 一帧里 5 个重复事件（长按时事件密度很高，常常一帧挤进好几个）
		let events = (0..5)
			.map(|index| key_down(egui::Key::Backspace, egui::Modifiers::default(), index > 0))
			.collect();
		pass_with_events(&ctx, &mut view, events);

		assert_eq!(
			written_bytes(&written),
			vec![0x7F; 5],
			"按住退格时每一个重复事件都要删一个字"
		);
	}

	/// 连着好几帧的重复退格也要一路删下去——`keys_down` 跨帧记账，
	/// 不能因为「上一帧已经按着」就把后面的全当噪声丢掉。
	#[cfg(unix)]
	#[test]
	fn backspace_repeat_survives_across_frames() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let (mut view, written) = attached_view(&ctx);

		for _ in 0..4 {
			pass_with_events(
				&ctx,
				&mut view,
				vec![key_down(egui::Key::Backspace, egui::Modifiers::default(), true)],
			);
		}

		assert_eq!(written_bytes(&written), vec![0x7F; 4]);
	}

	/// Esc 不能被 egui 当成「取消焦点」，否则按一下 Esc 终端就哑了。
	#[cfg(unix)]
	#[test]
	fn escape_does_not_drop_the_focus() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let (mut view, written) = attached_view(&ctx);
		let focused_before = ctx.memory(|memory| memory.focused());

		pass_with_events(
			&ctx,
			&mut view,
			vec![key_down(egui::Key::Escape, egui::Modifiers::default(), false)],
		);

		assert_eq!(written_bytes(&written), vec![0x1B], "Esc 应当进 shell（vim 靠它）");
		assert_eq!(ctx.memory(|memory| memory.focused()), focused_before);
	}

	/// 方向键同理：shell 的补全菜单、`less` 翻页都要它，不能被拿去挪焦点。
	#[cfg(unix)]
	#[test]
	fn arrow_keys_stay_in_the_terminal() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let (mut view, written) = attached_view(&ctx);
		let focused_before = ctx.memory(|memory| memory.focused());

		pass_with_events(
			&ctx,
			&mut view,
			vec![
				key_down(egui::Key::ArrowUp, egui::Modifiers::default(), false),
				key_down(egui::Key::ArrowLeft, egui::Modifiers::default(), false),
			],
		);

		assert_eq!(written_bytes(&written), b"\x1b[A\x1b[D");
		assert_eq!(ctx.memory(|memory| memory.focused()), focused_before);
	}

	/// 持有焦点时必须向系统申请 IME。
	///
	/// egui 只在 `output.ime` 有值时才会让 winit 打开输入法。不申请的话，
	/// 切到中文输入法后敲的每个键都会被系统吞掉：既没有 `Event::Text`，
	/// 也没有 `Event::Ime`——用户看到的就是「切了输入法还是打不出字」。
	#[cfg(unix)]
	#[test]
	fn focused_terminal_asks_for_ime() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let (mut view, _written) = attached_view(&ctx);
		let output = ctx.run_ui(
			egui::RawInput {
				screen_rect: test_screen(),
				..Default::default()
			},
			|ui| view.ui(ui),
		);

		let ime = output.platform_output.ime.expect("持有焦点时应申请 IME");
		assert!(
			ime.rect.width() > 0.0 && ime.rect.height() > 0.0,
			"IME 区域要覆盖终端画面，否则候选窗没处停"
		);
		assert_eq!(ime.purpose, egui::IMEPurpose::Normal);
	}

	/// 输入法提交的中文要原样写进链路。
	#[cfg(unix)]
	#[test]
	fn ime_commit_reaches_the_shell() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let (mut view, written) = attached_view(&ctx);
		pass_with_events(
			&ctx,
			&mut view,
			vec![egui::Event::Ime(egui::ImeEvent::Commit("中文".to_owned()))],
		);

		assert_eq!(written_bytes(&written), "中文".as_bytes());
	}

	/// 标签页标题不跟远端 shell 的 OSC 标题走。
	///
	/// 远端每 `cd` 一次就发一遍 `OSC 0`，标题会变成 `root@host:~/cert/scskj.net`：
	/// 同一台主机的标签横竖看不出是哪个，长度还随路径变。标签页回答的是
	/// 「我连的是哪台服务器」，那是数据库里配的名字，远端改不了。
	#[cfg(unix)]
	#[test]
	fn tab_title_ignores_shell_osc_title() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let link = RecordingLink {
			written: Arc::new(std::sync::Mutex::new(Vec::new())),
		};
		let mut view = TerminalView::attach(&ctx, Box::new(link), "川江-326").expect("挂载失败");

		// 远端把标题改成一段又长又带路径的字符串
		view.session.inject(b"\x1b]0;root@VM-0-2:~/cert/scskj.net\x07");
		pass_with_events(&ctx, &mut view, Vec::new());

		assert_eq!(view.title(), "川江-326", "标题应固定在建视图时给的名字");
	}

	#[test]
	fn wake_gate_passes_once_per_frame() {
		let gate = WakeGate::default();
		assert!(gate.try_pass(), "帧内第一块数据应申请一帧");
		assert!(!gate.try_pass(), "同帧后续数据应被合并，不得再吵事件循环");
		assert!(!gate.try_pass());

		gate.reset();
		assert!(gate.try_pass(), "新的一帧应重新开闸");
	}

	/// 端到端回归：本地 PTY 有输出时，即使界面一次都不来拉数据，
	/// egui 也该被叫醒。
	///
	/// 这是「shell 刷新有点慢」的根因测试：接唤醒之前，界面只能靠光标闪烁
	/// （500ms 一下）或用户碰鼠标/敲键才刷新，输出平均滞后 250ms、最坏 500ms。
	///
	/// 判据用「重绘原因里的文件是不是本文件」而不是 `has_requested_repaint()`：
	/// 后者对 `request_repaint_after` 也返回真，光标闪烁会让它恒为真，测不出东西。
	#[cfg(unix)]
	#[test]
	fn local_shell_output_requests_a_repaint() {
		// 无后端的空帧：纹理增量没人消费，必须显式清掉，否则 drop 时 panic
		fn empty_pass(ctx: &egui::Context) {
			let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| {});
			output.textures_delta.clear();
		}

		let ctx = egui::Context::default();
		// 首帧只为装载字体：`CellMetrics::measure` 需要它
		empty_pass(&ctx);

		let mut view = TerminalView::local_shell(&ctx).expect("启动本地终端失败");

		// PTY 的行规程会把输入回显回来——不依赖 shell 具体是什么
		view.session.write(b"echo daisy-wake\n").expect("写入失败");

		let deadline = Instant::now() + Duration::from_secs(5);
		let mut woken = false;
		while Instant::now() < deadline {
			empty_pass(&ctx);
			// 只有本文件的唤醒闭包会就近申请重绘，文件对上就是我们的路径
			woken = ctx
				.repaint_causes()
				.iter()
				.any(|cause| cause.file.ends_with("terminal.rs"));
			if woken {
				break;
			}
			std::thread::sleep(Duration::from_millis(5));
		}

		assert!(
			woken,
			"PTY 有输出时必须申请重绘，否则输出只能等下一次周期性重绘"
		);
	}

	#[test]
	fn ended_notice_differs_for_local_and_remote() {
		// 远端只说「断开」：-1 这类通道语义的退出码对用户没有意义
		assert_eq!(
			Origin::Remote.ended_notice(true, Some(-1)).as_deref(),
			Some("连接已断开")
		);
		assert_eq!(
			Origin::Remote.ended_notice(true, Some(0)).as_deref(),
			Some("连接已断开")
		);
		// 本地要带退出码——那是用户自己敲 `exit 3` 的结果
		assert_eq!(
			Origin::Local.ended_notice(true, Some(0)).as_deref(),
			Some("进程已退出（0）")
		);
		assert_eq!(
			Origin::Local.ended_notice(true, Some(3)).as_deref(),
			Some("进程已退出（3）")
		);
		// 没结束就不提示
		assert_eq!(Origin::Remote.ended_notice(false, None), None);
	}

	#[test]
	fn ended_notice_leaves_a_blank_line_above_it() {
		// 光标停在行尾（shell 收尾没换行）：补两个换行才空出一行
		let text = String::from_utf8_lossy(&ended_notice_bytes("连接已断开", false)).into_owned();
		assert!(
			text.starts_with("\r\n\r\n连接已断开"),
			"提示前方应空一行，实际：{text:?}"
		);
		assert!(text.ends_with("\r\n"), "提示末尾应换行，实际：{text:?}");

		// 光标已在新行行首（shell 自己收过尾）：再补两个就多空一行
		let text = String::from_utf8_lossy(&ended_notice_bytes("连接已断开", true)).into_owned();
		assert!(
			text.starts_with("\r\n连接已断开"),
			"行首只需补一个换行，实际：{text:?}"
		);
	}

	/// 端到端：本地 shell 退出后，提示真的写进了画面——不是只画在角落里。
	#[cfg(unix)]
	#[test]
	fn exit_writes_the_notice_into_the_screen() {
		fn pass(ctx: &egui::Context, view: &mut TerminalView) {
			let mut output = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui));
			output.textures_delta.clear();
		}

		let ctx = egui::Context::default();
		// 首帧只为装载字体（`CellMetrics::measure` 要用），纹理增量没人消费
		ctx.run_ui(egui::RawInput::default(), |_ui| {})
			.textures_delta
			.clear();

		let mut view = TerminalView::local_shell(&ctx).expect("启动本地终端失败");
		view.session.write(b"exit 3\n").expect("写入失败");

		let deadline = Instant::now() + Duration::from_secs(5);
		let mut screen = String::new();
		while Instant::now() < deadline {
			pass(&ctx, &mut view);
			screen = screen_text(&view);
			if screen.contains("进程已退出（3）") {
				break;
			}
			std::thread::sleep(Duration::from_millis(20));
		}

		assert!(
			screen.contains("进程已退出（3）"),
			"退出提示应写在终端画面里，实际：{screen:?}"
		);
	}

	/// 新建终端必须自己把键盘焦点拿走。
	///
	/// 以前只写了「点一下才 `request_focus`」，于是应用刚启动、或者点
	/// 「+ 新建终端」之后，用户得先点一下画面才能打字——SSH 连上后同样如此。
	#[cfg(unix)]
	#[test]
	fn new_terminal_takes_the_keyboard_without_a_click() {
		// 首帧只为装载字体：`CellMetrics::measure` 需要它
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let mut view = TerminalView::local_shell(&ctx).expect("启动本地终端失败");
		assert!(
			ctx.memory(|memory| memory.focused()).is_none(),
			"开跑前不该有任何控件持有焦点"
		);

		let mut output = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui));
		output.textures_delta.clear();
		assert!(
			ctx.memory(|memory| memory.focused()).is_some(),
			"新建终端要先拿到键盘焦点，否则用户必须点一下画面才能输入"
		);

		// 抢到一次就该收手：之后用户去点输入框，不能被拽回终端
		let field = std::cell::Cell::new(None);
		let mut text = String::new();
		let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
			let response = ui.add(egui::TextEdit::singleline(&mut text));
			field.set(Some(response.id));
			response.request_focus();
			view.ui(ui);
		});
		output.textures_delta.clear();
		assert_eq!(
			ctx.memory(|memory| memory.focused()),
			field.get(),
			"焦点只在新建那一次请求，不能在每帧把用户手上的焦点抢回来"
		);
	}

	/// 端到端：标签页一开就能打字——不需要任何点击。
	///
	/// 上面那条只看「焦点归谁」，这条看**键真的进了 PTY**：注入一段文本，
	/// 等行规程回显。焦点若没拿到，`handle_keyboard` 根本不会被调用，
	/// 屏幕上一个字都不出来。
	#[cfg(unix)]
	#[test]
	fn typing_works_right_after_the_tab_opens() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(egui::RawInput::default(), |_ui| {});
		warmup.textures_delta.clear();

		let mut view = TerminalView::local_shell(&ctx).expect("启动本地终端失败");
		// 第一帧：拿焦点
		let mut first = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui));
		first.textures_delta.clear();

		// 第二帧：注入文本（模拟键盘输入），中间没有任何点击
		let mut input = egui::RawInput::default();
		input
			.events
			.push(egui::Event::Text("daisy-focus".to_string()));
		let mut second = ctx.run_ui(input, |ui| view.ui(ui));
		second.textures_delta.clear();

		// PTY 的行规程会把输入回显回来
		let deadline = Instant::now() + Duration::from_secs(5);
		let mut screen = String::new();
		while Instant::now() < deadline {
			screen = screen_text(&view);
			if screen.contains("daisy-focus") {
				return;
			}
			let mut output = ctx.run_ui(egui::RawInput::default(), |ui| view.ui(ui));
			output.textures_delta.clear();
			std::thread::sleep(Duration::from_millis(20));
		}

		panic!("没有点击就应能输入，实际画面：{screen:?}");
	}
}


#[cfg(test)]
mod appearance_tests {
	use daisy_term::CellMetrics;

	use super::*;

	fn frame(ctx: &egui::Context) {
		let mut output = ctx.run_ui(
			egui::RawInput {
				screen_rect: Some(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(600.0, 400.0),
				)),
				..Default::default()
			},
			|ui| {
				ui.label("x");
			},
		);
		output.textures_delta.clear();
	}

	/// 单元格尺寸只能在新字体**真正生效之后**量。
	///
	/// egui 的 `Context::set_fonts` 不会立刻换字体：它把 `FontDefinitions`
	/// 存进 `memory.new_font_definitions`，下一趟 `begin_pass` 才落地。
	/// 所以在 `install_fonts()` 之后紧接着量 `'M'`，量到的还是旧字体的宽度——
	/// 换成比例字体（`Helvetica` 的 M 是 0.83em，内置 Hack 是 0.60em）时
	/// 差出近四成，整屏按错的列宽排：列越靠右偏得越多，宽字符之间被拉开
	/// 成两三格，行高也跟着变、上下行叠在一起。
	///
	/// 应用层的对策是 `main.rs` 里的 `appearance_pending`：换字体那帧只记标志，
	/// 下一帧开头再量。这条用例钉住那个「下一帧」是必须的。
	#[test]
	fn cell_metrics_lag_behind_set_fonts_by_one_frame() {
		let ctx = egui::Context::default();

		// 先切到比例字体，跑一帧让它生效
		crate::fonts::install_fonts(&ctx, Some("Helvetica"));
		frame(&ctx);
		let wide = CellMetrics::measure(&ctx, 14.0).width;

		// 换回内置默认，**同一帧内**量：还是 Helvetica 的宽度
		crate::fonts::install_fonts(&ctx, None);
		let immediate = CellMetrics::measure(&ctx, 14.0).width;
		assert!(
			(immediate - wide).abs() < 0.01,
			"同一帧内就换了字体？egui 的时序变了，appearance_pending 可以重估"
		);

		// 再跑一帧，新字体生效，这时量到的才是默认字体的宽度
		frame(&ctx);
		let after = CellMetrics::measure(&ctx, 14.0).width;
		assert!(
			after < wide - 2.0,
			"默认等宽字体应明显窄于 Helvetica：before={wide} after={after}"
		);
	}

	/// 换字体不该改动已连标签页的配色与内容，只换单元格尺寸。
	#[test]
	fn switching_fonts_keeps_the_grid_contents() {
		let ctx = egui::Context::default();
		crate::fonts::install_fonts(&ctx, None);
		// 字体要跑过一帧才可用（`Context::fonts_mut` 在那之前会 panic）
		frame(&ctx);

		let mut view = TerminalView::attach(&ctx, Box::new(SilentLink), "假链路")
			.expect("挂载失败");
		view.session.inject(b"Movies\t\tOrbStack");

		let before = view.session.snapshot();
		let width_before = view.renderer.metrics().width;

		// 换成比例字体，走「下一帧才量」的路径
		crate::fonts::install_fonts(&ctx, Some("Helvetica"));
		frame(&ctx);
		view.measure_cells(&ctx, 14.0);

		let after = view.session.snapshot();
		assert!(
			view.renderer.metrics().width > width_before + 1.0,
			"比例字体的单元格应当更宽：{} → {}",
			width_before,
			view.renderer.metrics().width
		);
		assert_eq!(before.cols, after.cols, "量单元格尺寸不该改网格列数");
		assert_eq!(before.cells.len(), after.cells.len(), "格子内容不该被改动");
	}

	/// 什么都不写的链路。
	struct SilentLink;

	impl daisy_term::SessionLink for SilentLink {
		fn try_recv(&mut self) -> Option<daisy_term::LinkEvent> {
			None
		}

		fn write(&mut self, _data: &[u8]) -> daisy_term::Result<()> {
			Ok(())
		}

		fn resize(&mut self, _cols: u16, _rows: u16) -> daisy_term::Result<()> {
			Ok(())
		}

		fn shutdown(&mut self) -> daisy_term::Result<bool> {
			Ok(true)
		}
	}
}
