//! 多会话标签页管理。
//!
//! 一个标签页对应一条字节链路（本地 PTY 或 SSH 远端 shell）加一套终端网格。
//! 连接过程是异步的，因此标签页有三个状态：连接中 / 已连接 / 失败。
//!
//! ```text
//!   open_local()  ─▶ TerminalView（本地 PTY）
//!   open_ssh()    ─▶ Connecting ──poll()──▶ Live(TerminalView)   （SSH）
//!                                    └─────▶ Failed(文案)
//! ```

use daisy_rdp::ConnectionParams;
use daisy_ssh::{HostKeyVerdict, SshParams};
use daisy_term::Palette;

use crate::connect::{ConnectOutcome, Connector, SshConnection};
use crate::design;
use crate::store::Store;
use crate::terminal::TerminalView;
use crate::views::rdp_view::{RdpAction, RdpView};

/// 标签页的当前状态。
///
/// `Live` 装箱：终端视图（会话 + 渲染器 + 标题）是重载，
/// 而连接中/失败才是每帧可能走到的分支。
enum TabState {
	/// 连接中（仅 SSH——RDP 的连接过程由 [`RdpView`] 自己管）。
	/// `request_id` 用于把异步结果对回本标签页。
	Connecting {
		/// 连接请求号。
		request_id: u64,
	},
	/// 已连接的内容。
	Live(TabContent),
	/// 连接失败，文案直接展示。
	Failed(String),
}

/// 终端内容与面板边缘之间的留白（逻辑点）。
const TERMINAL_PADDING: i8 = 8;

/// 标签页里装的东西。
///
/// 终端与 RDP 画面对标签机制来说只需要三件事：**标题、能关闭、能画**。
/// 用枚举而不是 `dyn Trait`——只有两个变体，枚举能把「漏处理某个变体」
/// 变成编译错误；`dyn` 则要到运行时才发现。
pub enum TabContent {
	/// 本地或 SSH 终端。
	Terminal(Box<TerminalView>),
	/// RDP 远程桌面。
	Rdp(Box<RdpView>),
}

impl TabContent {
	/// 标签标题。
	fn title(&self) -> &str {
		match self {
			Self::Terminal(view) => view.title(),
			Self::Rdp(view) => view.title(),
		}
	}

	/// 显式收链路。返回错误文案（正常关闭为 `None`）。
	fn close(&mut self) -> Option<String> {
		match self {
			Self::Terminal(view) => view.close().err().map(|error| error.to_string()),
			Self::Rdp(view) => view.close(),
		}
	}

	/// 向终端写一串字节（无人值守验证钩子用）。
	///
	/// RDP 画面没有「写一段文本」这回事，返回 `false` 让调用方知道没写成。
	fn write_text(&mut self, bytes: &[u8]) -> bool {
		match self {
			Self::Terminal(view) => view.write_bytes(bytes).is_ok(),
			Self::Rdp(_) => false,
		}
	}

	/// 让内容在下一帧主动拿键盘焦点。
	///
	/// 只对终端有意义：RDP 画面的焦点是「点一下才接管」，自动抢过去会把用户
	/// 正在别处敲的快捷键一并送进远端，不是这里该管的事。
	fn request_focus(&mut self) {
		if let Self::Terminal(view) = self {
			view.request_focus();
		}
	}

	/// 绘制内容，返回本帧产生的动作。
	///
	/// 终端没有可改的设置，所以恒为 `None`；动作只来自 RDP 画面
	/// （目前是缩放设置）。
	fn ui(&mut self, ui: &mut egui::Ui) -> Option<RdpAction> {
		match self {
			Self::Terminal(view) => {
				// 终端四周留白：字贴着面板边缘会显得局促。
				// 套一层 Frame 而不是在终端内部加偏移，是因为
				// `sync_size` 按 `available_size()` 算行列——外层的
				// 内边距会自动扣进去，两处不必各算一遍。
				// RDP 画面要铺满可用区域，所以不加。
				egui::Frame::NONE
					.inner_margin(egui::Margin::same(TERMINAL_PADDING))
					.show(ui, |ui| view.ui(ui));
				None
			},
			Self::Rdp(view) => view.ui(ui),
		}
	}

	/// 只换配色（字体还没生效时用）。
	fn set_palette(&mut self, palette: Palette) {
		if let Self::Terminal(view) = self {
			view.set_palette(palette);
		}
	}

	/// 只重量单元格尺寸（新字体生效之后用）。
	fn measure_cells(&mut self, ctx: &egui::Context, font_size: f32) {
		if let Self::Terminal(view) = self {
			view.measure_cells(ctx, font_size);
		}
	}
}

/// 一个标签页。
struct SessionTab {
	/// 标题（连接成功后可能被终端上报的标题覆盖）。
	title: String,
	/// 状态。
	state: TabState,
	/// 来源主机（`host:port`）。SSH 标签页用来在首连时记 known_hosts。
	endpoint: Option<(String, u16)>,
}

/// 会话管理器：持有全部标签页与连接器。
pub struct SessionManager {
	tabs: Vec<SessionTab>,
	active: usize,
	next_request_id: u64,
	connector: Connector,
	/// 当前终端外观。**新建标签页时立即应用**——否则启动时序稍差
	/// （先建标签页、后应用配置）就会让第一个标签页用旧配色。
	palette: Palette,
	font_size: f32,
}

impl SessionManager {
	/// 空的管理器。界面随后至少开一个标签页才有内容可看。
	pub fn new() -> Self {
		Self {
			tabs: Vec::new(),
			active: 0,
			next_request_id: 1,
			connector: Connector::new(),
			// 默认深色；设置页改主题后由 `set_palette` 更新
			palette: Palette::daisy_dark(),
			font_size: 14.0,
		}
	}

	/// 连接器。连通性测试之类的临时任务借它跑。
	#[must_use]
	pub fn connector(&self) -> &Connector {
		&self.connector
	}

	/// 只换配色，不动单元格尺寸。
	///
	/// 换过终端字体之后**不能**顺手量单元格尺寸：`ctx.set_fonts` 是下一趟
	/// 才生效的（egui 把它存成 `new_font_definitions`），此刻量到的 `M`
	/// 还是旧字体的宽度。配色没有这个问题，随改随生效。
	pub fn set_palette(&mut self, palette: Palette) {
		self.palette = palette;
		for tab in &mut self.tabs {
			if let TabState::Live(content) = &mut tab.state {
				content.set_palette(self.palette.clone());
			}
		}
	}

	/// 重量单元格尺寸。**必须在新字体真正生效之后调用**（下一帧）。
	pub fn set_font_size(&mut self, ctx: &egui::Context, font_size: f32) {
		self.font_size = font_size;
		for tab in &mut self.tabs {
			if let TabState::Live(content) = &mut tab.state {
				content.measure_cells(ctx, self.font_size);
			}
		}
	}

	/// 把当前外观应用到一个新建的视图。
	fn style_new_view(&self, ctx: &egui::Context, view: &mut TerminalView) {
		view.apply_appearance(ctx, self.palette.clone(), self.font_size);
	}

	/// 标签页数量。
	#[must_use]
	pub fn len(&self) -> usize {
		self.tabs.len()
	}

	/// 开一个本地终端标签页。
	pub fn open_local(&mut self, ctx: &egui::Context) {
		match TerminalView::local_shell(ctx) {
			Ok(mut view) => {
				tracing::info!("已打开本地终端标签页");
				self.style_new_view(ctx, &mut view);
				self.push(
					view.title().to_owned(),
					TabState::Live(TabContent::Terminal(Box::new(view))),
					None,
				);
			},
			Err(error) => {
				tracing::error!("启动本地终端失败：{error}");
				self.push(
					"本地终端".to_owned(),
					TabState::Failed(error.to_string()),
					None,
				);
			},
		}
	}

	/// 向当前活动标签页的终端写一串字节。返回是否写成。
	///
	/// 只给无人值守验证用（见 `main.rs` 的 `DAISY_SMOKE_TERM_INPUT`）：
	/// 合成键盘事件要辅助功能授权，脚本化不了，只能从应用自己的输入通道塞。
	pub fn write_to_active_terminal(&mut self, bytes: &[u8]) -> bool {
		match self.tabs.get_mut(self.active).map(|tab| &mut tab.state) {
			Some(TabState::Live(content)) => content.write_text(bytes),
			_ => false,
		}
	}

	/// 开一个 RDP 标签页。
	///
	/// 与 SSH 不同，**RDP 的连接过程由会话线程自己管**——`RdpView` 内部
	/// 一开始就是「连接中」状态，用不上 `request_id` 那套异步回收。
	pub fn open_rdp(
		&mut self,
		title: String,
		params: ConnectionParams,
		host_id: Option<i64>,
		display: daisy_core::model::DisplaySettings,
		expected_cert: Option<String>,
	) {
		let view = RdpView::new(
			title.clone(),
			params,
			host_id,
			display,
			expected_cert,
		);
		self.push(title, TabState::Live(TabContent::Rdp(Box::new(view))), None);
	}

	/// 开一个 SSH 标签页（先显示「连接中」，结果经 [`SessionManager::poll`] 回收）。
	pub fn open_ssh(&mut self, params: SshParams, title: String, endpoint: (String, u16)) {
		let request_id = self.next_request_id;
		self.next_request_id = self.next_request_id.wrapping_add(1);

		// 首帧尺寸先按经验值给，界面量出真实区域后会 resize 校正
		self.connector.request(request_id, params, 100, 30);
		self.push(title, TabState::Connecting { request_id }, Some(endpoint));
	}

	/// 开一个经跳板机的 SSH 标签页。
	///
	/// `jump` 是跳板机自己的连接参数；`target` / `auth` / `host_key` 属于真正要连的主机。
	pub fn open_ssh_via_jump(
		&mut self,
		spec: crate::connect::JumpSpec,
		title: String,
		endpoint: (String, u16),
	) {
		let request_id = self.next_request_id;
		self.next_request_id = self.next_request_id.wrapping_add(1);

		self.connector.request_jump(request_id, spec, 100, 30);
		self.push(title, TabState::Connecting { request_id }, Some(endpoint));
	}

	/// 关闭指定标签页。
	pub fn close(&mut self, index: usize) {
		if index >= self.tabs.len() {
			return;
		}

		// 先显式收链路，再 drop——让对端收到关闭而不是只看到连接断了
		if let Some(tab) = self.tabs.get_mut(index)
			&& let TabState::Live(content) = &mut tab.state
			&& let Some(error) = content.close()
		{
			tracing::debug!("结束会话时出错：{error}");
		}

		self.tabs.remove(index);
		// 选中项要跟着**会话**走，不能跟着下标走：关掉前面的标签后，
		// 原来选中的那个会话整体左移了一格，下标不动就等于选到了别人。
		if self.tabs.is_empty() {
			self.active = 0;
		} else if index < self.active {
			self.active -= 1;
		} else if self.active >= self.tabs.len() {
			self.active = self.tabs.len() - 1;
		}
		// 被关掉的那一页带着焦点一起没了，交给接替的那一页
		self.focus_active();
	}

	/// 关闭当前标签页。
	pub fn close_active(&mut self) {
		self.close(self.active);
	}

	/// 选中标签页。
	///
	/// 顺便把键盘焦点交过去：点标签栏这个动作在 egui 里等于「点了别处」，
	/// 焦点会被交还，不补这一下的话切过去还得再点一回画面才能打字。
	pub fn select(&mut self, index: usize) {
		if index < self.tabs.len() {
			self.active = index;
			self.focus_active();
		}
	}

	/// 把键盘焦点交给当前活动标签页的内容。
	fn focus_active(&mut self) {
		if let Some(TabState::Live(content)) =
			self.tabs.get_mut(self.active).map(|tab| &mut tab.state)
		{
			content.request_focus();
		}
	}

	/// 收割异步连接结果，并把首连的主机密钥记入 known_hosts。
	///
	/// `store` 只在这里用一次（记密钥），平时不参与每帧逻辑。
	///
	/// 结果先收进 `Vec` 再处理——`TryIter` 借着 `connector`，
	/// 边迭代边调 `self` 的方法会撞借用检查。
	pub fn poll(&mut self, ctx: &egui::Context, store: &Store) {
		let outcomes: Vec<ConnectOutcome> = self.connector.poll().collect();
		for outcome in outcomes {
			match outcome {
				ConnectOutcome::Connected { request_id, conn } => {
					self.on_connected(request_id, conn, ctx, store);
				},
				ConnectOutcome::Failed { request_id, error } => {
					// 同 RDP：失败原因只弹浮层的话，事后无从排查
					let title = self
						.tab_by_request(request_id)
						.map_or_else(|| "（已关闭）".to_string(), |tab| tab.title.clone());
					tracing::warn!("SSH 连接「{title}」失败：{error}");
					if let Some(tab) = self.tab_by_request(request_id) {
						tab.state = TabState::Failed(error);
					}
				},
			}
		}
	}

	/// 画标签页栏（放顶部面板）。末尾带「+ 本地终端」。
	///
	/// 标签可拖拽重排：把 A 拖到 B 的左右半边即插到对应位置。
	pub fn ui_tabs(&mut self, ui: &mut egui::Ui) {
		// 标签之间贴得近一些：间距大了会散成一个个独立按钮，
		// 而标签页在语义上是「同一排里的一格」
		ui.spacing_mut().item_spacing.x = 2.0;
		ui.horizontal_wrapped(|ui| {
			let mut to_close: Option<usize> = None;
			let mut to_select: Option<usize> = None;
			let mut to_move: Option<(usize, usize)> = None;

			for index in 0..self.tabs.len() {
				let selected = index == self.active;
				let label = {
					let tab = &self.tabs[index];
					match &tab.state {
						TabState::Connecting { .. } => format!("{} …", tab.title),
						// 用汉字而不是 `✕`：CJK 字体缺这个字形时会渲染成方框
						// （见 docs/UI-POLISH.md 的约定）
						TabState::Failed(_) => format!("{} · 失败", tab.title),
						TabState::Live(content) => content.title().to_owned(),
					}
				};

				let hit = tab_widget(ui, &label, selected, index);

				if hit.open_clicked {
					to_select = Some(index);
				}
				if hit.close_clicked {
					to_close = Some(index);
				}

				// 拖拽悬停在本标签上：按指针在左/右半边决定插前还是插后
				if let Some(payload) = hit.response.dnd_hover_payload::<usize>() {
					let from = *payload;
					if from != index {
						let after = ui
							.ctx()
							.input(|input| input.pointer.hover_pos())
							.is_some_and(|pos| pos.x > hit.response.rect.center().x);
						paint_insert_marker(ui, &hit.response.rect, after);
						to_move = Some((from, if after { index + 1 } else { index }));
					}
				}
			}

			ui.add_space(6.0);
			if design::ghost_button(ui, "+ 新建终端").clicked() {
				self.open_local(ui.ctx());
			}

			if let Some(index) = to_close {
				self.close(index);
			}
			if let Some(index) = to_select {
				self.select(index);
			}
			// 顺序要紧：先关闭再重排，否则索引已经变了
			if let Some((from, to)) = to_move {
				self.move_tab(from, to);
			}
		});
	}

	/// 把 `from` 处的标签移到 `to` 位置（`to` 是**移动前**的目标索引）。
	///
	/// 索引运算抽在 [`plan_move`] 里单测；这里只管落地。
	pub fn move_tab(&mut self, from: usize, to: usize) {
		let Some((insert_at, active)) = plan_move(self.tabs.len(), self.active, from, to) else {
			return;
		};
		let tab = self.tabs.remove(from);
		self.tabs.insert(insert_at, tab);
		self.active = active;
		// 拖着标签走的这一下同样算「点了别处」，焦点会被 egui 交还
		self.focus_active();
	}

	/// 取走某个标签页攒下的提示（剪贴板不可用之类），交给应用层弹 toast。
	///
	/// 视图层够不到 `DaisyApp` 的 toast 队列，只能往上递；每帧收一条，
	/// 堆积的会在后续帧里陆续弹出。
	pub fn take_notice(&mut self) -> Option<String> {
		self.tabs.iter_mut().find_map(|tab| match &mut tab.state {
			TabState::Live(TabContent::Terminal(view)) => view.take_notice(),
			TabState::Live(TabContent::Rdp(_))
			| TabState::Connecting { .. }
			| TabState::Failed(_) => None,
		})
	}

	/// 每帧渲染所有「弹出去」的 RDP 桌面。
	///
	/// **不能挂在 `ui_active` 上**：那里只跑当前激活的标签页，用户切走之后
	/// 弹出去的窗口会因为「本帧没画到」被 egui 关掉。
	pub fn detached_viewports(&mut self, ctx: &egui::Context) {
		for tab in &mut self.tabs {
			if let TabState::Live(TabContent::Rdp(view)) = &mut tab.state {
				view.detached_pass(ctx);
			}
		}
	}

	/// 画活动标签页的内容（放中央面板）。
	pub fn ui_active(&mut self, ui: &mut egui::Ui) -> Option<RdpAction> {
		if self.tabs.is_empty() {
			ui.vertical_centered(|ui| {
				ui.add_space(80.0);
				ui.heading("没有会话");
				ui.add_space(8.0);
				ui.label("双击左侧主机列表即可连接；也可开一个本地终端。");
			});
			return None;
		}
		let mut action = None;

		if let Some(tab) = self.tabs.get_mut(self.active) {
			// 关闭点击先记下来，等 `tab` 的借用结束再动手——
			// 在 match 臂里直接调 self.close_active() 会撞借用检查
			let mut close_clicked = false;

			match &mut tab.state {
				TabState::Connecting { .. } => {
					ui.vertical_centered(|ui| {
						ui.add_space(60.0);
						ui.spinner();
						ui.add_space(8.0);
						ui.label(format!("正在连接 {} …", tab.title));
					});
					// 连接中也要持续重绘，否则结果到了也不刷新
					ui.ctx()
						.request_repaint_after(std::time::Duration::from_millis(100));
				},
				TabState::Failed(error) => {
					let error = error.clone();
					ui.vertical_centered(|ui| {
						ui.add_space(60.0);
						ui.heading("连接失败");
						ui.add_space(8.0);
						ui.label(&error);
						ui.add_space(12.0);
						close_clicked = ui.button("关闭标签页").clicked();
					});
				},
				TabState::Live(content) => action = content.ui(ui),
			}

			if close_clicked {
				self.close_active();
			}
		}

		action
	}

	/// 连接成功的收尾：建视图、记主机密钥。
	fn on_connected(
		&mut self,
		request_id: u64,
		conn: Box<SshConnection>,
		ctx: &egui::Context,
		store: &Store,
	) {
		let Some(endpoint) = self
			.tab_by_request(request_id)
			.and_then(|tab| tab.endpoint.clone())
		else {
			// 标签页已被用户关掉——会话没人要了，直接丢弃（drop 即断连）
			tracing::debug!("连接结果到达时标签页已关闭，丢弃会话");
			return;
		};

		// 首连接受的主机密钥在此落库。之后同一 host:port 再连就会核对，
		// 指纹变化直接拒连（S6 的指纹锁在这里闭合）。
		match conn.host_key_verdict() {
			HostKeyVerdict::Unknown => {
				let info = conn.host_key();
				match store.remember_host_key(&endpoint.0, endpoint.1, &info.fingerprint) {
					Ok(()) => tracing::info!(
						"首次连接 {}:{}，已记住主机密钥 {}",
						endpoint.0,
						endpoint.1,
						info.fingerprint
					),
					Err(error) => tracing::warn!("记住主机密钥失败：{error}"),
				}
			},
			HostKeyVerdict::Match => {
				tracing::debug!("主机密钥与 known_hosts 记录一致");
			},
		}

		// 外观值先取到局部——`tab_by_request()` 借的是整个 self，
		// 调用期间不能再读 self 的字段
		let palette = self.palette.clone();
		let font_size = self.font_size;

		let Some(tab) = self.tab_by_request(request_id) else {
			return;
		};

		match TerminalView::attach(ctx, Box::new(conn.shell), &tab.title) {
			Ok(mut view) => {
				view.apply_appearance(ctx, palette, font_size);
				tab.title = view.title().to_owned();
				tab.state = TabState::Live(TabContent::Terminal(Box::new(view)));
			},
			Err(error) => {
				tracing::error!("会话已连上但终端初始化失败：{error}");
				tab.state = TabState::Failed(format!("连接成功但终端初始化失败：{error}"));
			},
		}
	}

	/// 按请求号找标签页。
	fn tab_by_request(&mut self, request_id: u64) -> Option<&mut SessionTab> {
		self.tabs.iter_mut().find(|tab| match tab.state {
			TabState::Connecting { request_id: id } => id == request_id,
			_ => false,
		})
	}

	/// 追加标签页并选中它。
	fn push(&mut self, title: String, state: TabState, endpoint: Option<(String, u16)>) {
		self.tabs.push(SessionTab {
			title,
			state,
			endpoint,
		});
		self.active = self.tabs.len() - 1;
	}
}

/// 计算拖拽重排的落地位置。
///
/// 返回 `(插入位置, 新的选中索引)`；`None` 表示这次拖动等于没动
/// （原地、或语义上等价于原地），调用方应直接跳过。
///
/// 注意 `to` 是**移动前**的坐标系里的目标索引：把标签拖到 `to` 处
/// 意味着它要占据 `to` 之前的位置。移除 `from` 之后，大于 `from`
/// 的目标位置都要左移一格——这个偏移很容易漏，单独抽出来测。
fn plan_move(len: usize, active: usize, from: usize, to: usize) -> Option<(usize, usize)> {
	if from >= len || to > len {
		return None;
	}
	// 原地：拖到自己左侧或紧邻自己右侧，结果不变
	if from == to || from + 1 == to {
		return None;
	}

	let insert_at = if to > from { to - 1 } else { to };

	// 选中索引跟着被移动的标签走；否则按「区间整体平移一格」调整
	let new_active = if active == from {
		insert_at
	} else if from < active && insert_at >= active {
		active - 1
	} else if from > active && insert_at <= active {
		active + 1
	} else {
		active
	};

	Some((insert_at, new_active))
}

/// 标签的左内边距（文字起点）。
const TAB_PAD_LEFT: f32 = 12.0;
/// 文字末尾到 × 图形的间隙。
const TAB_CLOSE_GAP: f32 = 8.0;
/// × 十字的边长。
///
/// 对齐的是「+ 新建终端」左边那个加号：13pt 字体里加号的墨迹约 7.5pt，
/// 而十字的墨迹 ≈ 边长 + 描边，所以边长取 6.2 才和它一样大。
/// **不是**按中文字高算——中文字形 12~13pt，十字照那个尺寸会显得又大又重。
const TAB_CLOSE_GLYPH: f32 = 6.2;
/// 标签的右内边距。
const TAB_PAD_RIGHT: f32 = 10.0;
/// × 的点击区。比图形大得多——图形 6pt 不代表用户能点准 6pt。
/// 但**不能大到压住文字**：压住了就是「点最后一个字会关标签」。
const TAB_CLOSE_HIT: f32 = 18.0;

/// 一个标签的交互结果。
struct TabHit {
	/// 拖拽源与标签合并后的响应：判拖拽插入位置用它的 `rect`。
	response: egui::Response,
	/// 点到了标签正文（应切换过去）。
	open_clicked: bool,
	/// 点到了 ×（应关闭）。
	close_clicked: bool,
}

/// 画一个标签并处理它的交互。
///
/// **不用 `dnd_drag_source`**，原因有两条，都是实测出来的：
///
/// 1. 它在标签矩形上再盖一层 `Sense::drag()` 的控件。只感应拖动的东西
///    在**按下的那一刻**就算「开始拖」（egui 的 `interaction.rs`：不是
///    click+drag 的控件直接 `is_dragged = senses_drag()`），于是点一下标签
///    等于把它拎起来——而且盖在上面的这一层会让比它早注册的点击区
///    收不到点击，表现就是「点标签切不过去、点 × 关不掉」，还不报错。
/// 2. 拖动中的标签被画进 Tooltip 图层，和普通图层的 `Response` 做 `union`
///    会直接 panic（"makes no sense to combine Responses from two different
///    layers"）。
///
/// 改成自己管：`click_and_drag` 让 egui 等指针真的移动了再判定拖动，
/// 拖动开始时把索引塞进拖拽载荷，落点由调用方画插入指示线。
fn tab_widget(ui: &mut egui::Ui, label: &str, selected: bool, index: usize) -> TabHit {
	let (rect, close_rect) = tab_item(ui, label, selected);

	let response = ui.interact(
		rect,
		ui.id().with(("tab", index)),
		egui::Sense::click_and_drag(),
	);
	// 关闭键注册在后面，所以它压在标签之上：点 × 拿到的是它
	let close_response = ui.interact(
		close_rect,
		ui.id().with(("tab_close", index)),
		egui::Sense::click(),
	);

	response.dnd_set_drag_payload(index);
	if response.dragged() {
		// 拖起来了才换抓手；平时保持箭头（用户明确要求不要手型）
		ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
	}

	TabHit {
		// 先判点击再取 rect——`union` 之类的操作会消费响应
		open_clicked: response.clicked() && !close_response.clicked(),
		close_clicked: close_response.clicked(),
		response,
	}
}

/// 画一个标签页，返回（标签矩形，关闭键的点击矩形）。
///
/// **这里一个交互控件都不注册。** `dnd_drag_source` 会在标签矩形上再盖一层
/// `Sense::drag()` 的控件，任何比它早注册的点击区都收不到点击——实测标签
/// 和 × 全都点不动。所以绘制与交互分开：这里只画，点击由调用方在拖拽源
/// **之后**注册。
///
/// 视觉上**不给选中项填整块底色**，只在底部画一条强调线。
/// 原来用 `selectable_label` 时每个选中项都是一整块蓝，
/// 一排标签里几块蓝比终端内容还抢眼。
fn tab_item(ui: &mut egui::Ui, label: &str, selected: bool) -> (egui::Rect, egui::Rect) {
	let tokens = design::tokens(ui.ctx());
	let pointer = ui.ctx().pointer_interact_pos();
	let font = egui::FontId::proportional(design::FONT_BASE);
	let color = if selected {
		tokens.text
	} else {
		tokens.text_muted
	};
	let galley = ui.painter().layout_no_wrap(label.to_owned(), font, color);
	let text_width = galley.size().x;

	// 关闭键的位置**始终预留**：只在悬停时才展开宽度的话，
	// 鼠标扫过整排标签时它们会左右跳动。
	let width = TAB_PAD_LEFT + text_width + TAB_CLOSE_GAP + TAB_CLOSE_GLYPH + TAB_PAD_RIGHT;
	let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 30.0), egui::Sense::hover());

	// 悬停用几何判定（指针在不在矩形里）：交互控件注册在拖拽源之后，
	// 这里拿不到 Response，而绘制必须发生在交互注册之前。
	let hovering = pointer.is_some_and(|p| rect.contains(p));
	let painter = ui.painter();
	if hovering {
		painter.rect_filled(rect, design::RADIUS_MD, tokens.bg_hover);
	}

	let text_pos = egui::pos2(rect.left() + TAB_PAD_LEFT, rect.center().y);
	painter.galley(
		text_pos - egui::vec2(0.0, galley.size().y * 0.5),
		galley.clone(),
		color,
	);

	// **垂直对齐按文字的实际墨迹算，不按行框算。**
	// 行框（`galley.size()`）含上下留白，中文字形的墨迹偏上，
	// 照行框居中就会让 × 比文字低一截（实测差 1.25pt）。
	let ink = galley.mesh_bounds;
	let ink_center_y = text_pos.y - galley.size().y * 0.5 + ink.center().y;

	let close_center = egui::pos2(
		rect.left() + TAB_PAD_LEFT + text_width + TAB_CLOSE_GAP + TAB_CLOSE_GLYPH * 0.5,
		ink_center_y,
	);
	let close_rect = egui::Rect::from_center_size(close_center, egui::Vec2::splat(TAB_CLOSE_HIT));
	let hovering_close = pointer.is_some_and(|p| close_rect.contains(p));
	if hovering_close {
		painter.rect_filled(close_rect, design::RADIUS_SM, tokens.bg_raised);
	}

	design::icon_close(
		painter,
		egui::Rect::from_center_size(close_center, egui::Vec2::splat(TAB_CLOSE_GLYPH)),
		// **和标签文字同色**，只有悬停才提到最亮。
		// 用 `text_faint` 时，同一个标签里文字是「灰」，叉是「更灰」——
		// 尺寸怎么调都像是从别处贴过来的（实测灰阶 150 vs 95）。
		if hovering_close { tokens.text } else { color },
	);

	if selected {
		let y = rect.bottom() - 1.0;
		painter.line_segment(
			[
				egui::pos2(rect.left() + 6.0, y),
				egui::pos2(rect.right() - 6.0, y),
			],
			egui::Stroke::new(2.0, tokens.accent),
		);
	}

	(rect, close_rect)
}

/// 在标签的左/右侧画一条插入指示线。
fn paint_insert_marker(ui: &egui::Ui, rect: &egui::Rect, after: bool) {
	let x = if after {
		rect.right() + 1.0
	} else {
		rect.left() - 1.0
	};
	let color = design::tokens(ui.ctx()).accent;
	ui.painter()
		.vline(x, rect.y_range(), egui::Stroke::new(2.0, color));
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 造一份只含给定事件的输入。
	fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
		egui::RawInput {
			screen_rect: Some(egui::Rect::from_min_size(
				egui::Pos2::ZERO,
				egui::vec2(800.0, 600.0),
			)),
			events,
			..Default::default()
		}
	}

	fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
		egui::Event::PointerButton {
			pos,
			button: egui::PointerButton::Primary,
			pressed,
			modifiers: egui::Modifiers::default(),
		}
	}

	/// 跑一帧标签栏，返回（标签矩形，点正文，点 ×）。
	fn tab_frame(ctx: &egui::Context, events: Vec<egui::Event>) -> (egui::Rect, bool, bool) {
		let mut rect = egui::Rect::ZERO;
		let mut open = false;
		let mut close = false;
		let mut output = ctx.run_ui(raw_input(events), |ui| {
			egui::Panel::top("tabs").show(ui, |ui| {
				let hit = tab_widget(ui, "本地终端", false, 0);
				rect = hit.response.rect;
				open = hit.open_clicked;
				close = hit.close_clicked;
			});
		});
		// 纹理增量没人消费会在 drop 时 panic
		output.textures_delta.clear();
		(rect, open, close)
	}

	/// 点一下标签（按下与抬起分两帧，和真实输入一致），返回（点正文，点 ×）。
	///
	/// 这两件事同时坏过一次：点击区被 `dnd_drag_source` 那层 `Sense::drag()`
	/// 挡住，切换和关闭全都不响应、也没有任何报错——只能靠人点才发现。
	fn click_tab(pos: impl Fn(egui::Rect) -> egui::Pos2) -> (bool, bool) {
		let ctx = egui::Context::default();
		let (rect, _, _) = tab_frame(&ctx, Vec::new());
		assert!(rect.width() > 0.0, "标签没被布局出来");

		let target = pos(rect);
		tab_frame(
			&ctx,
			vec![
				egui::Event::PointerMoved(target),
				pointer_button(target, true),
			],
		);
		let (_, open, close) = tab_frame(&ctx, vec![pointer_button(target, false)]);
		(open, close)
	}

	#[test]
	fn clicking_tab_body_switches() {
		let (open, close) = click_tab(|rect| egui::pos2(rect.left() + 20.0, rect.center().y));
		assert!(open, "点标签正文没有触发切换");
		assert!(!close, "点正文不该关标签");
	}

	#[test]
	fn clicking_close_button_closes() {
		// 关闭键中心：右内边距 + 半个十字
		let (open, close) = click_tab(|rect| {
			egui::pos2(
				rect.right() - TAB_PAD_RIGHT - TAB_CLOSE_GLYPH * 0.5,
				rect.center().y,
			)
		});
		assert!(close, "点 × 没有触发关闭");
		assert!(!open, "点 × 不该同时切换标签");
	}

	/// 拖动标签：**按下那一刻不能算拖**，要等指针真的移动。
	///
	/// 只感应拖动的控件（`Sense::drag()`）在按下瞬间就进入拖动状态，
	/// 于是「点一下标签」变成「把标签拎起来」，重排也会被误触发。
	#[test]
	fn drag_payload_only_after_pointer_moves() {
		let ctx = egui::Context::default();
		let (rect, _, _) = tab_frame(&ctx, Vec::new());
		let start = rect.center();

		tab_frame(
			&ctx,
			vec![
				egui::Event::PointerMoved(start),
				pointer_button(start, true),
			],
		);
		assert!(
			!egui::DragAndDrop::has_any_payload(&ctx),
			"刚按下就设置载荷 = 单击也会被当成拖动"
		);

		tab_frame(
			&ctx,
			vec![egui::Event::PointerMoved(start + egui::vec2(40.0, 0.0))],
		);
		assert!(
			egui::DragAndDrop::has_payload_of_type::<usize>(&ctx),
			"指针移动后应进入拖动并带上索引载荷"
		);
	}

	/// 关闭键的点击区不能压住标签文字。
	///
	/// 压住了就变成「点最后一个字 → 标签关了」，而这种误触用户
	/// 根本不会想到是关闭键干的。结论与文字长度无关，取任意值即可。
	#[test]
	fn close_hit_area_does_not_cover_the_label() {
		let text = 60.0_f32;
		let text_end = TAB_PAD_LEFT + text;
		let center = text_end + TAB_CLOSE_GAP + TAB_CLOSE_GLYPH * 0.5;
		let width = text_end + TAB_CLOSE_GAP + TAB_CLOSE_GLYPH + TAB_PAD_RIGHT;

		assert!(
			center - TAB_CLOSE_HIT * 0.5 >= text_end,
			"点击区左缘压到文字上了"
		);
		assert!(
			center + TAB_CLOSE_HIT * 0.5 <= width,
			"点击区右缘超出标签了"
		);
		// 图标不该大于字号：一旦超过，视觉上就变成「一个图标旁边配了行小字」。
		// 写成 const 块：这是常量之间的关系，编译期就该挡下。
		const {
			assert!(TAB_CLOSE_GLYPH <= design::FONT_BASE);
			assert!(TAB_CLOSE_GLYPH > 0.0);
		}
	}

	#[test]
	fn moving_forward_shifts_insert_left() {
		// [A,B,C] 把 A(0) 移到 C 之后(to=3)：原来的 to 要减 1
		assert_eq!(plan_move(3, 0, 0, 3), Some((2, 2)));
	}

	#[test]
	fn moving_backward_uses_target_as_is() {
		// [A,B,C] 把 C(2) 移到最前(to=0)
		assert_eq!(plan_move(3, 0, 2, 0), Some((0, 1)));
	}

	#[test]
	fn dragging_onto_own_edges_is_a_no_op() {
		let len = 4;
		assert_eq!(plan_move(len, 0, 2, 2), None, "拖到自己左侧 = 不动");
		assert_eq!(plan_move(len, 0, 2, 3), None, "拖到自己右侧 = 不动");
	}

	#[test]
	fn out_of_range_is_rejected() {
		assert_eq!(plan_move(3, 0, 5, 0), None, "来源越界");
		assert_eq!(plan_move(3, 0, 0, 9), None, "目标越界");
	}

	#[test]
	fn active_follows_the_moved_tab() {
		// 选中 B(1)，把 B 拖到最前
		let (insert_at, active) = plan_move(3, 1, 1, 0).expect("应可移动");
		assert_eq!((insert_at, active), (0, 0), "选中应跟着 B 走");
	}

	#[test]
	fn active_shifts_when_an_earlier_tab_moves_behind_it() {
		// [A,B,C] 选中 B(1)；把 A(0) 拖到 B 之后(to=2)
		// 结果 [B,A,C]：B 从 1 变成 0
		let (insert_at, active) = plan_move(3, 1, 0, 2).expect("应可移动");
		assert_eq!(insert_at, 1);
		assert_eq!(active, 0);
	}

	#[test]
	fn active_shifts_when_a_later_tab_moves_before_it() {
		// [A,B,C] 选中 B(1)；把 C(2) 拖到最前(to=0)
		// 结果 [C,A,B]：B 从 1 变成 2
		let (insert_at, active) = plan_move(3, 1, 2, 0).expect("应可移动");
		assert_eq!(insert_at, 0);
		assert_eq!(active, 2);
	}

	#[test]
	fn active_untouched_when_move_happens_after_it() {
		// [A,B,C,D] 选中 A(0)；把 C(2) 拖到末尾(to=4)
		// 结果 [A,B,D,C]：A 仍在 0
		let (insert_at, active) = plan_move(4, 0, 2, 4).expect("应可移动");
		assert_eq!(insert_at, 3);
		assert_eq!(active, 0);
	}

	#[test]
	fn active_untouched_when_move_happens_before_it() {
		// [A,B,C,D] 选中 D(3)；把 B(1) 拖到最前(to=0)
		// 结果 [B,A,C,D]：D 仍在 3
		let (insert_at, active) = plan_move(4, 3, 1, 0).expect("应可移动");
		assert_eq!(insert_at, 0);
		assert_eq!(active, 3);
	}

	/// 跑一帧「活动标签页内容」。
	#[cfg(unix)]
	fn content_frame(ctx: &egui::Context, mgr: &mut SessionManager) {
		let mut output = ctx.run_ui(raw_input(Vec::new()), |ui| {
			mgr.ui_active(ui);
		});
		output.textures_delta.clear();
	}

	/// 切标签页之后键盘焦点要跟着走。
	///
	/// 点标签栏在 egui 里等于「点了别处」，焦点会被交还；不补一次请求，
	/// 用户切过去还得再点一下画面才能打字。
	#[cfg(unix)]
	#[test]
	fn switching_tabs_takes_the_keyboard_back() {
		let ctx = egui::Context::default();
		let mut warmup = ctx.run_ui(raw_input(Vec::new()), |_ui| {});
		warmup.textures_delta.clear();

		let mut mgr = SessionManager::new();
		mgr.open_local(&ctx);
		mgr.open_local(&ctx);
		content_frame(&ctx, &mut mgr);

		// 模拟用户点了标签栏：焦点被交给标签控件
		let tab_bar = egui::Id::new("daisy_tab_bar_probe");
		ctx.memory_mut(|memory| memory.request_focus(tab_bar));

		mgr.select(0);
		content_frame(&ctx, &mut mgr);

		let focused = ctx.memory(|memory| memory.focused());
		assert!(focused.is_some(), "切过去之后应当有控件持有焦点");
		assert_ne!(focused, Some(tab_bar), "焦点应回到终端，而不是停在标签栏上");
	}

	/// 关掉前面的标签页，选中的**会话**不能跟着下标跑。
	#[cfg(unix)]
	#[test]
	fn closing_an_earlier_tab_keeps_the_same_session_selected() {
		let ctx = egui::Context::default();
		// 本地终端建视图时要量字体，必须先跑一帧把字体装进去
		let mut warmup = ctx.run_ui(raw_input(Vec::new()), |_ui| {});
		warmup.textures_delta.clear();

		let mut mgr = SessionManager::new();
		mgr.open_local(&ctx);
		mgr.open_local(&ctx);
		mgr.open_local(&ctx);

		mgr.select(1);
		mgr.close(0);

		assert_eq!(mgr.active, 0, "原本选中的会话左移了一格，选中项要跟着它走");
	}
}
