//! SFTP 文件管理对话框。
//!
//! 布局：左侧远端目录树，右侧当前目录的文件列表。
//!
//! | 位置 | 右键菜单 |
//! | --- | --- |
//! | 文件 / 目录行 | 删除、复制、下载 |
//! | 空白处 | 粘贴（剪贴板空时置灰）、上传 |
//!
//! 视图只做「画 + 收集动作」，真正的传输交给 [`crate::sftp_session::SftpSession`]，
//! 由它在后台线程里跑——界面每帧只看有没有新事件。

use std::path::{Path, PathBuf};

use daisy_ssh::SshParams;
use daisy_ssh::sftp::{DirEntry, join_path};

use crate::design;
use crate::sftp_session::{SftpEvent, SftpSession, base_name, human_size};
use crate::views;

/// 行高。
const ROW_HEIGHT: f32 = 26.0;
/// 目录树宽度。
const TREE_WIDTH: f32 = 190.0;
/// 树的一级缩进。
const TREE_INDENT: f32 = 12.0;
/// 右侧列表各列的宽度（从右往左量）。
const COL_SIZE: f32 = 80.0;
const COL_TIME: f32 = 128.0;
const COL_MODE: f32 = 86.0;

/// 对话框尺寸。
const DIALOG_SIZE: [f32; 2] = [820.0, 560.0];

/// 应用内剪贴板：复制的是远端路径，粘贴时在远端内部复制。
///
/// 不走系统剪贴板——系统剪贴板传不了「远端路径」这层语义，
/// 而跨主机粘贴需要两条连接同时在线，先不做。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Clipboard {
	/// 源路径。
	path: String,
	/// 是否目录（目录暂不支持粘贴复制）。
	is_dir: bool,
}

/// 目录树节点。子节点懒加载：展开时才去列目录。
#[derive(Debug)]
struct TreeNode {
	/// 绝对路径。
	path: String,
	/// 显示名。
	name: String,
	/// 缩进层级。
	depth: usize,
	/// 是否展开。
	expanded: bool,
	/// 子节点（只放目录）。
	children: Vec<TreeNode>,
	/// 是否已经拉取过子节点。
	loaded: bool,
}

impl TreeNode {
	fn new(path: String, name: String, depth: usize) -> Self {
		Self {
			path,
			name,
			depth,
			expanded: false,
			children: Vec::new(),
			loaded: false,
		}
	}

	/// 按路径找节点（深度优先）。
	fn find(&mut self, path: &str) -> Option<&mut TreeNode> {
		if self.path == path {
			return Some(self);
		}
		for child in &mut self.children {
			if let Some(found) = child.find(path) {
				return Some(found);
			}
		}
		None
	}
}

/// 一帧里收集到的动作，画完再统一执行（避免借用冲突）。
enum Action {
	Navigate(String),
	Delete { path: String, is_dir: bool },
	Copy(String),
	Download(String),
	Paste,
	Upload,
	Refresh,
}

/// 对话框状态。
pub struct SftpState {
	/// 标题里的主机名。
	host_name: String,
	/// 后台会话。`None` 表示已经结束。
	session: Option<SftpSession>,
	/// 当前目录。
	cwd: String,
	/// 当前目录的内容。
	entries: Vec<DirEntry>,
	/// 登录用户的家目录（树的根）。
	home: String,
	/// 目录树（根节点）。
	tree: Option<TreeNode>,
	/// 正在做什么（`None` 表示空闲）。
	busy: Option<String>,
	/// 最近一次错误，显示在底部。
	error: Option<String>,
	/// 最近一次成功提示与它的时刻——提示只留几秒，不能一直占着状态栏。
	info: Option<(String, std::time::Instant)>,
	/// 连接阶段失败的原因。
	connect_error: Option<String>,
	/// 应用内剪贴板。
	clipboard: Option<Clipboard>,
	/// 选中的路径。
	selected: Option<String>,
	/// 待确认的删除。
	pending_delete: Option<(String, bool)>,
	/// 对话框是否还开着。
	open: bool,
	/// 连接是否已建立。
	connected: bool,
	/// 无人值守验证钩子：连上后自动下载一次的远端路径（见 `DAISY_SMOKE_SFTP_DOWNLOAD`）。
	auto_download: Option<String>,
}

impl SftpState {
	/// 打开一个文件管理对话框（连接在后台进行）。
	pub fn open(host_name: impl Into<String>, params: SshParams) -> Self {
		let host_name = host_name.into();
		tracing::info!("打开 SFTP 文件管理：{host_name}");
		Self {
			host_name,
			session: Some(SftpSession::spawn(params)),
			cwd: String::from("/"),
			entries: Vec::new(),
			home: String::from("/"),
			tree: None,
			busy: Some("正在连接…".to_string()),
			error: None,
			info: None,
			connect_error: None,
			clipboard: None,
			selected: None,
			pending_delete: None,
			open: true,
			connected: false,
			auto_download: std::env::var("DAISY_SMOKE_SFTP_DOWNLOAD")
				.ok()
				.filter(|path| !path.trim().is_empty()),
		}
	}

	/// 是否还开着（应用层据此丢弃状态）。
	#[must_use]
	pub fn is_open(&self) -> bool {
		self.open
	}

	/// 造一个不连服务器的状态（只在测试里用）：只把目录内容填好。
	/// 使用方（空白区回归）是 `#[cfg(unix)]`，Windows 测试构建里这里会死代码。
	#[cfg(all(test, unix))]
	fn stub(entries: Vec<DirEntry>) -> Self {
		Self {
			host_name: "测试主机".to_string(),
			session: None,
			cwd: "/root/cert".to_string(),
			entries,
			home: "/root".to_string(),
			tree: None,
			busy: None,
			error: None,
			info: None,
			connect_error: None,
			clipboard: None,
			selected: None,
			pending_delete: None,
			open: true,
			connected: true,
			auto_download: None,
		}
	}

	/// 对话框标题。
	fn title(&self) -> String {
		format!("文件 · {}", self.host_name)
	}

	/// 画对话框。返回本帧是否要关闭。
	pub fn ui(&mut self, ctx: &egui::Context) {
		self.drain_events();

		// 传输期间要让界面持续刷新，否则进度与结果都停在上一帧
		if self.busy.is_some() {
			ctx.request_repaint_after(std::time::Duration::from_millis(80));
		}

		let mut open = self.open;
		views::dialog(ctx, &self.title(), DIALOG_SIZE, &mut open, |ui| {
			self.body(ui);
		});
		self.open = open;
		if !self.open {
			self.close();
		}
	}

	/// 收后台事件。
	fn drain_events(&mut self) {
		let Some(session) = self.session.as_mut() else {
			return;
		};
		let events = session.drain();
		for event in events {
			match event {
				SftpEvent::Connected { home } => {
					self.connected = true;
					self.connect_error = None;
					self.home = home.clone();
					self.cwd = home.clone();
					self.tree = Some(TreeNode::new(home.clone(), home_name(&home), 0));
					if let Some(root) = self.tree.as_mut() {
						root.expanded = true;
						root.loaded = true;
					}
				},
				SftpEvent::Listed { path, entries } => {
					if path == self.cwd {
						self.entries = entries.clone();
						// 钩子只触发一次：拿第一个列出来的目录做目标
						if let Some(remote) = self.auto_download.take() {
							tracing::info!("[SFTP] 无人值守钩子：下载 {remote}");
							self.download(&remote);
						}
					}
					// 目录树只关心子目录
					if let Some(node) = self.tree.as_mut().and_then(|root| root.find(&path)) {
						node.loaded = true;
						node.children = entries
							.iter()
							.filter(|entry| entry.is_dir)
							.map(|entry| {
								TreeNode::new(
									entry.path.clone(),
									entry.name.clone(),
									node.depth + 1,
								)
							})
							.collect();
					}
				},
				SftpEvent::Busy(state) => self.busy = state,
				SftpEvent::Info(text) => {
					self.error = None;
					self.info = Some((text.clone(), std::time::Instant::now()));
					tracing::info!("[SFTP] {text}");
				},
				SftpEvent::Error(text) => {
					tracing::warn!("[SFTP] {text}");
					self.error = Some(text);
				},
				SftpEvent::Closed(reason) => {
					tracing::info!("[SFTP] 会话结束：{reason}");
					if !self.connected {
						self.connect_error = Some(reason);
					}
					self.session = None;
					self.busy = None;
				},
			}
		}
	}

	fn close(&mut self) {
		if let Some(session) = self.session.as_mut() {
			session.close();
		}
		self.session = None;
	}

	/// 主体：工具栏 + 左树 + 右列表 + 状态条。
	fn body(&mut self, ui: &mut egui::Ui) {
		let height = ui.available_height();
		let footer = 26.0;

		self.path_bar(ui);
		ui.add_space(design::GAP_SM);

		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = design::GAP_MD;
			// 左：目录树
			ui.allocate_ui_with_layout(
				egui::vec2(TREE_WIDTH, height - footer - 34.0),
				egui::Layout::top_down(egui::Align::Min),
				|ui| self.tree_panel(ui),
			);
			// 右：文件列表
			ui.allocate_ui_with_layout(
				egui::vec2(ui.available_width(), height - footer - 34.0),
				egui::Layout::top_down(egui::Align::Min),
				|ui| self.list_panel(ui),
			);
		});

		ui.add_space(design::GAP_XS);
		self.status_bar(ui);
		self.delete_confirm(ui.ctx());
	}

	/// 顶部：家目录 / 上一级 / 当前路径。
	fn path_bar(&mut self, ui: &mut egui::Ui) {
		let tokens = design::tokens(ui.ctx());
		let mut action = None;
		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = design::GAP_XS;
			if design::ghost_button(ui, "家目录").clicked() {
				action = Some(Action::Navigate(self.home.clone()));
			}
			if let Some(parent) = daisy_ssh::parent_of(&self.cwd)
				&& design::ghost_button(ui, "上一级").clicked()
			{
				action = Some(Action::Navigate(parent));
			}
			ui.add_space(design::GAP_XS);
			// 路径只读展示，太长就截断——对话框宽度有限，路径是给眼睛定位用的
			ui.add(
				egui::Label::new(
					egui::RichText::new(&self.cwd)
						.size(design::FONT_BASE)
						.color(tokens.text_dim),
				)
				.truncate()
				.selectable(true),
			);
		});
		if let Some(Action::Navigate(path)) = action {
			self.navigate(path);
		}
	}

	/// 左侧目录树。
	fn tree_panel(&mut self, ui: &mut egui::Ui) {
		let tokens = design::tokens(ui.ctx());
		egui::Frame::NONE
			.fill(tokens.bg_deep)
			.corner_radius(design::RADIUS_MD)
			.inner_margin(egui::Margin::symmetric(6, 6))
			.show(ui, |ui| {
				ui.set_min_size(egui::vec2(TREE_WIDTH - 12.0, ui.available_height()));
				egui::ScrollArea::both()
					// 同一帧里两个 ScrollArea：不给盐会撞同一个自动 ID，
					// egui 会直接弹「First use of ScrollArea ID」的红框
					.id_salt("sftp-tree")
					.auto_shrink([false, false])
					.show(ui, |ui| {
						let mut action = None;
						// 先克隆结构再画：画的时候要 &mut self 处理动作
						let nodes = self.tree.as_ref().map(flatten_tree).unwrap_or_default();
						for (node_path, node_name, depth, expanded, has_children, _visible) in nodes {
							let (_, response) = tree_row(
								ui,
								&node_name,
								depth,
								expanded,
								has_children,
								self.cwd == node_path,
								tokens,
							);
							// 单击就进去：树上的目录名是导航入口，
							// 「点一下只展开、右边不动」会让人以为点空了
							if response.clicked() || response.double_clicked() {
								action = Some(Action::Navigate(node_path.clone()));
							}
						}
						if let Some(Action::Navigate(path)) = action {
							self.navigate(path);
						}
					});
			});
	}

	/// 右侧文件列表。
	///
	/// 用 `show_rows` **只画可见行**：`/usr/bin` 这类目录上千项，
	/// 每帧都给每一项分配矩形 + 排版文字会明显卡顿。
	fn list_panel(&mut self, ui: &mut egui::Ui) {
		let tokens = design::tokens(ui.ctx());
		egui::Frame::NONE
			.fill(tokens.bg_deep)
			.corner_radius(design::RADIUS_MD)
			.inner_margin(egui::Margin::symmetric(6, 6))
			.show(ui, |ui| {
				let width = ui.available_width();
				header_row(ui, width, tokens);

				let area = ui.available_rect_before_wrap();
				let total = self.entries.len();
				let mut actions: Vec<Action> = Vec::new();
				// 最后画出来那一行的矩形：空白区必须从它下面开始，
				// 否则会盖住行的下半截（测试拿它当判据）
				let mut last_row = egui::Rect::NOTHING;

				let output = egui::ScrollArea::vertical()
					.id_salt("sftp-list")
					.auto_shrink([false, false])
					.show_rows(ui, ROW_HEIGHT, total, |ui, range| {
						ui.set_min_width(width);
						for index in range {
							let entry = self.entries[index].clone();
							let selected = self.selected.as_deref() == Some(entry.path.as_str());
							let (rect, response) = file_row(ui, &entry, width, selected, tokens);
							last_row = rect;
							if response.clicked() {
								self.selected = Some(entry.path.clone());
							}
							if response.double_clicked() {
								if entry.is_dir {
									actions.push(Action::Navigate(entry.path.clone()));
								} else {
									actions.push(Action::Download(entry.path.clone()));
								}
							}
							response.context_menu(|ui| {
								views::menu(ui, |ui| {
									if ui.button("下载").clicked() {
										actions.push(Action::Download(entry.path.clone()));
										ui.close();
									}
									if ui.button("复制").clicked() {
										actions.push(Action::Copy(entry.path.clone()));
										ui.close();
									}
									ui.separator();
									if ui.button("删除").clicked() {
										actions.push(Action::Delete {
											path: entry.path.clone(),
											is_dir: entry.is_dir,
										});
										ui.close();
									}
								});
							});
						}
					});

				// 空白区域：列表比可视区短时补满，右键落在哪都算空白。
				// 行自己已经挂过菜单，这里从内容底部往下算，不会抢行的点击。
				//
				// **必须用 `content_size`，不能拿「行数 × 行高」算**：行与行之间还有
				// `item_spacing.y`（8pt），漏掉之后空白区整体上抬，正好压住最后一行
				// 的下半截——单击与右键都被空白区抢走，表现就是「最后一行选不中」。
				// 3 个文件时抬高了 16pt，一行才 26 高，等于只剩上沿能点。
				let content_bottom = area.top() + output.content_size.y;
				let blank_top = content_bottom.min(area.bottom());
				let blank = egui::Rect::from_min_max(
					egui::pos2(area.left(), blank_top),
					area.right_bottom(),
				);
				#[cfg(test)]
				record_list_geometry(last_row, blank);
				if blank.height() > 1.0 {
					let response =
						ui.interact(blank, ui.id().with("sftp-blank"), egui::Sense::click());
					response.context_menu(|ui| {
						blank_menu(ui, &mut actions, self.clipboard.is_some());
					});
					if total == 0 {
						ui.painter().text(
							blank.center(),
							egui::Align2::CENTER_CENTER,
							"这个目录是空的",
							egui::FontId::proportional(design::FONT_XS),
							tokens.text_muted,
						);
					}
				}

				for action in actions {
					self.apply(action);
				}
			});
	}

	/// 底部状态：忙碌 / 错误 / 连接失败。
	fn status_bar(&mut self, ui: &mut egui::Ui) {
		let tokens = design::tokens(ui.ctx());
		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = design::GAP_SM;
			if let Some(busy) = &self.busy {
				ui.add(egui::Spinner::new().size(14.0));
				ui.label(
					egui::RichText::new(busy)
						.size(design::FONT_XS)
						.color(tokens.text_dim),
				);
				return;
			}
			if let Some(error) = &self.connect_error {
				ui.label(
					egui::RichText::new(format!("连接失败：{error}"))
						.size(design::FONT_XS)
						.color(tokens.danger),
				);
				return;
			}
			if let Some(error) = &self.error {
				ui.label(
					egui::RichText::new(error.clone())
						.size(design::FONT_XS)
						.color(tokens.danger),
				);
				return;
			}
			// 成功提示只留 5 秒：下载到哪儿这种事看一眼就够，
			// 一直挂着反而把「还有几项」这类常驻信息挤掉
			if let Some((text, at)) = &self.info
				&& at.elapsed() < std::time::Duration::from_secs(5)
			{
				ui.label(
					egui::RichText::new(text.clone())
						.size(design::FONT_XS)
						.color(tokens.success),
				);
				ui.ctx()
					.request_repaint_after(std::time::Duration::from_millis(200));
				return;
			}
			let count = self.entries.len();
			ui.label(
				egui::RichText::new(format!("{count} 项"))
					.size(design::FONT_XS)
					.color(tokens.text_muted),
			);
			if let Some(clip) = &self.clipboard {
				ui.label(
					egui::RichText::new(format!("已复制：{}", base_name(&clip.path)))
						.size(design::FONT_XS)
						.color(tokens.text_muted),
				);
			}
		});
	}

	/// 删除确认。
	fn delete_confirm(&mut self, ctx: &egui::Context) {
		let Some((path, is_dir)) = self.pending_delete.clone() else {
			return;
		};
		let tokens = design::tokens(ctx);
		let mut confirmed = false;
		let mut cancelled = false;
		egui::Modal::new(egui::Id::new("sftp-delete")).show(ctx, |ui| {
			ui.set_width(320.0);
			ui.label(
				egui::RichText::new(format!("删除 {}", base_name(&path)))
					.size(design::FONT_LG)
					.color(tokens.text),
			);
			ui.add_space(design::GAP_SM);
			ui.label(
				egui::RichText::new(if is_dir {
					"目录必须为空才能删除。删除后无法恢复。"
				} else {
					"删除后无法恢复。"
				})
				.size(design::FONT_BASE)
				.color(tokens.text_dim),
			);
			ui.add_space(design::GAP_MD);
			ui.horizontal(|ui| {
				confirmed = design::danger_button(ui, "删除").clicked();
				cancelled = design::ghost_button(ui, "取消").clicked();
			});
		});
		if confirmed {
			if let Some(session) = &self.session {
				session.delete(&path, is_dir);
			}
			self.pending_delete = None;
			self.selected = None;
			self.refresh();
		} else if cancelled {
			self.pending_delete = None;
		}
	}

	// ---- 动作 ----

	fn apply(&mut self, action: Action) {
		match action {
			Action::Navigate(path) => self.navigate(path),
			Action::Delete { path, is_dir } => self.pending_delete = Some((path, is_dir)),
			Action::Copy(path) => {
				let is_dir = self
					.entries
					.iter()
					.find(|entry| entry.path == path)
					.is_some_and(|entry| entry.is_dir);
				self.clipboard = Some(Clipboard { path, is_dir });
			},
			Action::Download(path) => self.download(&path),
			Action::Paste => self.paste(),
			Action::Upload => self.pick_upload(),
			Action::Refresh => self.refresh(),
		}
	}

	fn navigate(&mut self, path: String) {
		// 树上对应节点自动展开：`Listed` 事件回来时会用它填子目录
		if let Some(node) = self.tree.as_mut().and_then(|root| root.find(&path)) {
			node.expanded = true;
			node.loaded = true;
		}
		if let Some(session) = &self.session {
			session.list(path.clone());
		}
		self.cwd = path;
		self.error = None;
	}

	fn refresh(&mut self) {
		if let Some(session) = &self.session {
			session.list(self.cwd.clone());
		}
	}

	fn download(&mut self, remote: &str) {
		let Some(session) = &self.session else {
			return;
		};
		let dir = download_dir();
		let name = base_name(remote).to_string();
		if name.is_empty() {
			return;
		}
		let local = unique_path(&dir, &name);
		session.download(remote.to_string(), local);
	}

	fn paste(&mut self) {
		let Some(clip) = self.clipboard.clone() else {
			return;
		};
		if clip.is_dir {
			self.error = Some("暂不支持粘贴目录".to_string());
			return;
		}
		let Some(session) = &self.session else {
			return;
		};
		let target = unique_remote(&self.entries, &self.cwd, base_name(&clip.path));
		session.copy(&clip.path, &target);
	}

	fn pick_upload(&mut self) {
		// **必须在主线程调用**：macOS 的 `NSOpenPanel` 是 AppKit 的东西，
		// 从后台线程开面板属于未定义行为（实测会直接崩）。
		// 代价是选择期间界面停住——系统模态窗口本来就是这个语义。
		let Some(path) = rfd::FileDialog::new()
			.set_title("选择要上传的文件")
			.pick_file()
		else {
			return;
		};
		let name = path
			.file_name()
			.map(|name| name.to_string_lossy().into_owned())
			.unwrap_or_else(|| "file".to_string());
		let remote = join_path(&self.cwd, &name);
		if let Some(session) = &self.session {
			session.upload(&path, &remote);
		}
	}
}

/// 空白区域的右键菜单。
fn blank_menu(ui: &mut egui::Ui, actions: &mut Vec<Action>, has_clipboard: bool) {
	views::menu(ui, |ui| {
		// 「粘贴」置灰而不是隐藏：用户得看得出「这里能粘贴，只是剪贴板空着」
		if ui
			.add_enabled(has_clipboard, egui::Button::new("粘贴"))
			.clicked()
		{
			actions.push(Action::Paste);
			ui.close();
		}
		if ui.button("上传").clicked() {
			actions.push(Action::Upload);
			ui.close();
		}
		ui.separator();
		if ui.button("刷新").clicked() {
			actions.push(Action::Refresh);
			ui.close();
		}
	});
}

// 列表面板的几何：最后一行与空白区（只在测试构建里编译）。
//
// 界面里没有别的办法把这两个矩形取出来——`Response.rect` 拿不到，
// 而「空白区压住最后一行」正是这一处的经典回归。
#[cfg(test)]
thread_local! {
	static LIST_GEOMETRY: std::cell::RefCell<Option<(egui::Rect, egui::Rect)>> =
		const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn record_list_geometry(last_row: egui::Rect, blank: egui::Rect) {
	LIST_GEOMETRY.with(|slot| *slot.borrow_mut() = Some((last_row, blank)));
}

#[cfg(all(test, unix))]
fn list_geometry() -> Option<(egui::Rect, egui::Rect)> {
	LIST_GEOMETRY.with(|slot| *slot.borrow())
}

/// 把树摊平成「画一行需要的全部信息」。
///
/// 返回 `(path, name, depth, expanded, has_children, visible)`。
fn flatten_tree(root: &TreeNode) -> Vec<(String, String, usize, bool, bool, bool)> {
	let mut out = Vec::new();
	fn walk(
		node: &TreeNode,
		out: &mut Vec<(String, String, usize, bool, bool, bool)>,
		visible: bool,
	) {
		// 树里只放**目录**，所以每个节点都可以展开：箭头表示「能点开」，
		// 而不是「一定有子项」（懒加载下展开前根本不知道有没有）
		out.push((
			node.path.clone(),
			node.name.clone(),
			node.depth,
			node.expanded,
			true,
			visible,
		));
		if node.expanded {
			for child in &node.children {
				walk(child, out, visible);
			}
		}
	}
	walk(root, &mut out, true);
	out
}

/// 一行树节点。返回 `(rect, response)`。
fn tree_row(
	ui: &mut egui::Ui,
	name: &str,
	depth: usize,
	expanded: bool,
	has_children: bool,
	selected: bool,
	tokens: design::Tokens,
) -> (egui::Rect, egui::Response) {
	let width = ui.available_width();
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::click());

	let painter = ui.painter();
	if selected {
		painter.rect_filled(rect, design::RADIUS_SM, tokens.bg_active);
	} else if response.hovered() {
		painter.rect_filled(rect, design::RADIUS_SM, tokens.bg_hover);
	}

	let left = rect.left() + 4.0 + depth as f32 * TREE_INDENT;
	let color = if selected {
		tokens.text
	} else {
		tokens.text_dim
	};
	let font = egui::FontId::proportional(design::FONT_BASE);

	if has_children {
		let arrow = egui::Rect::from_center_size(
			egui::pos2(left + 4.0, rect.center().y),
			egui::vec2(9.0, 9.0),
		);
		design::icon_chevron(painter, arrow, color, expanded);
	}
	painter.text(
		egui::pos2(left + 14.0, rect.center().y),
		egui::Align2::LEFT_CENTER,
		name,
		font,
		color,
	);
	(rect, response)
}

/// 表头。
fn header_row(ui: &mut egui::Ui, width: f32, tokens: design::Tokens) {
	let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
	let painter = ui.painter();
	let font = egui::FontId::proportional(design::FONT_XS);
	let y = rect.center().y;
	painter.text(
		egui::pos2(rect.left() + 10.0, y),
		egui::Align2::LEFT_CENTER,
		"名称",
		font.clone(),
		tokens.text_muted,
	);
	painter.text(
		egui::pos2(rect.right() - COL_TIME - COL_MODE - 6.0, y),
		egui::Align2::RIGHT_CENTER,
		"大小",
		font.clone(),
		tokens.text_muted,
	);
	painter.text(
		egui::pos2(rect.right() - COL_MODE - 6.0, y),
		egui::Align2::RIGHT_CENTER,
		"修改时间",
		font.clone(),
		tokens.text_muted,
	);
	painter.text(
		egui::pos2(rect.right() - 6.0, y),
		egui::Align2::RIGHT_CENTER,
		"权限",
		font,
		tokens.text_muted,
	);
}

/// 一行文件。返回 `(rect, response)`。
fn file_row(
	ui: &mut egui::Ui,
	entry: &DirEntry,
	width: f32,
	selected: bool,
	tokens: design::Tokens,
) -> (egui::Rect, egui::Response) {
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(width, ROW_HEIGHT), egui::Sense::click());

	let painter = ui.painter();
	if selected {
		painter.rect_filled(rect, design::RADIUS_SM, tokens.bg_active);
	} else if response.hovered() {
		painter.rect_filled(rect, design::RADIUS_SM, tokens.bg_hover);
	}

	let name_color = if entry.is_dir {
		tokens.accent_text
	} else {
		tokens.text
	};
	let clip = painter.with_clip_rect(rect);
	// 名称列的可视宽度：减去右边三列与两个间隙
	let name_right = rect.right() - COL_TIME - COL_MODE - COL_SIZE - 12.0;
	let name_rect = egui::Rect::from_min_max(
		egui::pos2(rect.left() + 10.0, rect.top()),
		egui::pos2(name_right, rect.bottom()),
	);
	clip.text(
		egui::pos2(name_rect.left(), rect.center().y),
		egui::Align2::LEFT_CENTER,
		&entry.name,
		egui::FontId::proportional(design::FONT_BASE),
		name_color,
	);

	let small = egui::FontId::proportional(design::FONT_XS);
	let size_text = if entry.is_dir {
		"—".to_string()
	} else {
		human_size(entry.size)
	};
	painter.text(
		egui::pos2(rect.right() - COL_TIME - COL_MODE - 6.0, rect.center().y),
		egui::Align2::RIGHT_CENTER,
		size_text,
		small.clone(),
		tokens.text_muted,
	);
	painter.text(
		egui::pos2(rect.right() - COL_MODE - 6.0, rect.center().y),
		egui::Align2::RIGHT_CENTER,
		format_time(entry.mtime),
		small.clone(),
		tokens.text_muted,
	);
	painter.text(
		egui::pos2(rect.right() - 6.0, rect.center().y),
		egui::Align2::RIGHT_CENTER,
		entry.mode_text(),
		small,
		tokens.text_muted,
	);
	(rect, response)
}

/// Unix 秒 → `2026-11-12 15:04`（本地时区）。拿不到时间就显示 `—`。
fn format_time(mtime: Option<u32>) -> String {
	let Some(seconds) = mtime else {
		return "—".to_string();
	};
	match chrono::DateTime::from_timestamp(i64::from(seconds), 0) {
		Some(utc) => utc
			.with_timezone(&chrono::Local)
			.format("%Y-%m-%d %H:%M")
			.to_string(),
		None => "—".to_string(),
	}
}

/// 家目录显示名：`/root` → `root`，`/` → `/`。
fn home_name(home: &str) -> String {
	base_name(home).to_string()
}

/// 下载目录：`~/Downloads`，没有就落到临时目录。
fn download_dir() -> PathBuf {
	std::env::var_os("HOME")
		.map(|home| PathBuf::from(home).join("Downloads"))
		.filter(|dir| dir.is_dir())
		.unwrap_or_else(std::env::temp_dir)
}

/// 本地不重名的落盘路径：`a.txt` → `a (1).txt`。
fn unique_path(dir: &Path, name: &str) -> PathBuf {
	let candidate = dir.join(name);
	if !candidate.exists() {
		return candidate;
	}
	let (stem, extension) = match name.rsplit_once('.') {
		Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
		_ => (name, String::new()),
	};
	for index in 1..1000 {
		let next = dir.join(format!("{stem} ({index}){extension}"));
		if !next.exists() {
			return next;
		}
	}
	candidate
}

/// 远端不重名的目标路径（粘贴用）。
fn unique_remote(entries: &[DirEntry], dir: &str, name: &str) -> String {
	if !entries.iter().any(|entry| entry.name == name) {
		return join_path(dir, name);
	}
	let (stem, extension) = match name.rsplit_once('.') {
		Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
		_ => (name, String::new()),
	};
	for index in 1..1000 {
		let candidate = format!("{stem} ({index}){extension}");
		if !entries.iter().any(|entry| entry.name == candidate) {
			return join_path(dir, &candidate);
		}
	}
	join_path(dir, name)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unique_path_appends_index_before_extension() {
		let dir = std::env::temp_dir().join("daisy-sftp-unique-test");
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).expect("建目录失败");

		let first = unique_path(&dir, "a.txt");
		assert_eq!(first.file_name().unwrap(), "a.txt");
		std::fs::write(&first, b"x").expect("写文件失败");

		let second = unique_path(&dir, "a.txt");
		assert_eq!(second.file_name().unwrap(), "a (1).txt");

		let no_ext = dir.join("README");
		std::fs::write(&no_ext, b"x").expect("写文件失败");
		assert_eq!(
			unique_path(&dir, "README").file_name().unwrap(),
			"README (1)"
		);

		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn unique_remote_respects_existing_entries() {
		let entry = DirEntry {
			name: "a.txt".into(),
			path: "/tmp/a.txt".into(),
			is_dir: false,
			size: 1,
			mtime: None,
			permissions: None,
		};
		assert_eq!(unique_remote(&[], "/tmp", "a.txt"), "/tmp/a.txt");
		assert_eq!(unique_remote(&[entry], "/tmp", "a.txt"), "/tmp/a (1).txt");
	}

	#[test]
	fn home_name_strips_path() {
		assert_eq!(home_name("/home/korialuo"), "korialuo");
		assert_eq!(home_name("/root"), "root");
		assert_eq!(home_name("/"), "/");
	}

	#[test]
	fn tree_flatten_skips_collapsed_children() {
		let mut root = TreeNode::new("/".into(), "/".into(), 0);
		root.expanded = true;
		let mut child = TreeNode::new("/etc".into(), "etc".into(), 1);
		child
			.children
			.push(TreeNode::new("/etc/x".into(), "x".into(), 2));
		root.children.push(child);

		let flat = flatten_tree(&root);
		// 根 + etc；etc 未展开 → 孙节点不出现
		assert_eq!(flat.len(), 2);
		assert_eq!(flat[1].0, "/etc");

		root.children[0].expanded = true;
		assert_eq!(flatten_tree(&root).len(), 3);
	}

	#[test]
	fn flatten_reports_visibility_as_true_for_all_rows() {
		let mut root = TreeNode::new("/".into(), "/".into(), 0);
		root.expanded = true;
		root.children
			.push(TreeNode::new("/a".into(), "a".into(), 1));
		assert!(flatten_tree(&root).iter().all(|row| row.5));
	}
	/// 三个文件（最后一个是中文名）跑一帧。
	#[cfg(unix)]
	fn render_frame(ctx: &egui::Context, state: &mut SftpState, events: Vec<egui::Event>) {
		let input = egui::RawInput {
			screen_rect: Some(egui::Rect::from_min_size(
				egui::Pos2::ZERO,
				egui::vec2(1000.0, 700.0),
			)),
			events,
			..Default::default()
		};
		let mut output = ctx.run_ui(input, |ui| {
			let ctx = ui.ctx().clone();
			state.ui(&ctx);
		});
		output.textures_delta.clear();
	}

	/// 一个文件条目。
	#[cfg(all(test, unix))]
	fn file(name: &str, size: u64) -> DirEntry {
		DirEntry {
			name: name.to_string(),
			path: format!("/root/cert/{name}"),
			is_dir: false,
			size,
			mtime: Some(1_700_000_000),
			permissions: Some(0o100_644),
		}
	}

	/// **空白热区不能压住最后一行**。
	///
	/// 回归：空白区原来按「行数 × 行高」算，漏了行与行之间的 `item_spacing.y`（8pt）。
	/// 三个文件时它整体上抬 16pt，而一行才 26 高——最后一行只剩上沿能点，
	/// 单击和右键都被空白区抢走。用户看到的是「那一行怎么点都选不中」，
	/// 还以为是文件名带中文（截图里正好是 `川江-326.pdf`）。
	#[cfg(unix)]
	#[test]
	fn the_blank_area_never_covers_the_last_row() {
		let ctx = egui::Context::default();
		crate::fonts::install_fonts(&ctx, None);
		let mut state = SftpState::stub(vec![
			file("scskj.net.key", 1_700),
			file("scskj.net_bundle.pem", 6_900),
			file("川江-326.pdf", 56_000),
		]);

		// 头几帧窗口还在自适应尺寸与位置，先跑热
		for _ in 0..3 {
			render_frame(&ctx, &mut state, Vec::new());
		}
		let (last_row, blank) = list_geometry().expect("列表应当记录几何");
		assert!(
			blank.top() >= last_row.bottom() - 0.01,
			"空白区压在最后一行上（行 {last_row:?}，空白 {blank:?}）"
		);

		// 点在最后一行正中：必须选中它（不是空白区的菜单，也不是上一行）
		let point = last_row.center();
		render_frame(
			&ctx,
			&mut state,
			vec![
				egui::Event::PointerMoved(point),
				egui::Event::PointerButton {
					pos: point,
					button: egui::PointerButton::Primary,
					pressed: true,
					modifiers: egui::Modifiers::default(),
				},
				egui::Event::PointerButton {
					pos: point,
					button: egui::PointerButton::Primary,
					pressed: false,
					modifiers: egui::Modifiers::default(),
				},
			],
		);
		let after = list_geometry().expect("第二帧也该记录");
		assert!(
			(after.0.top() - last_row.top()).abs() < 0.01,
			"窗口在两帧之间挪动了，点击坐标不能复用（{last_row:?} vs {:?}）",
			after.0
		);
		assert_eq!(
			state.selected.as_deref(),
			Some("/root/cert/川江-326.pdf"),
			"最后一行点不中"
		);
	}

	/// 右键菜单有个宽度下限，别挤成一条窄条。
	///
	/// 菜单宽度本来由最宽的那一项决定，「复制 / 下载」这种两字菜单只有 70pt，
	/// 一列项贴着边框，看着像下拉框。这条钉住 `views::menu` 真的被套上了。
	#[cfg(unix)]
	#[test]
	fn context_menus_are_not_too_narrow() {
		let ctx = egui::Context::default();
		crate::fonts::install_fonts(&ctx, None);
		let mut actions: Vec<Action> = Vec::new();
		let mut width = 0.0;

		let input = egui::RawInput {
			screen_rect: Some(egui::Rect::from_min_size(
				egui::Pos2::ZERO,
				egui::vec2(600.0, 400.0),
			)),
			..Default::default()
		};
		let mut output = ctx.run_ui(input, |ui| {
			width = egui::Area::new("menu-probe".into())
				.fixed_pos(egui::pos2(20.0, 20.0))
				.show(ui.ctx(), |ui| {
					blank_menu(ui, &mut actions, false);
					ui.min_rect().width()
				})
				.inner;
		});
		output.textures_delta.clear();

		assert!(
			width >= 148.0,
			"菜单太窄（{width}），检查 views::menu 有没有被套上"
		);
	}
}
