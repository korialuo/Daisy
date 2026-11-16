//! 侧边栏：主机列表（分组树）。
//!
//! 只负责「展示 + 收集点击」，所有状态变更经 [`HostListAction`] 回传给应用层——
//! 视图不直接碰 store，保持可测。树的构建是纯函数 [`build_tree`]，单测覆盖。
//!
//! 分组只支持**两层**（与 `daisy_store::group` 的约束一致）：顶层分组下可挂
//! 子分组；子分组下只能挂主机，不再嵌套。

use std::collections::{BTreeSet, HashMap};

use daisy_core::model::{Group, Host, Protocol};

use crate::design;

/// 侧边栏触发的动作。
pub enum HostListAction {
	/// 连接主机（双击或右键菜单）。
	Connect(Host),
	/// 新建主机。
	NewHost,
	/// 编辑主机。
	Edit(Host),
	/// 打开主机的文件管理（SFTP，仅 SSH 主机）。
	OpenFiles(Host),
	/// 删除主机（主键）。
	Delete(i64),
	/// 新建分组。`parent` 为 `None` 建顶层，`Some(id)` 在该分组下建子分组。
	NewGroup {
		/// 上级分组主键。
		parent: Option<i64>,
	},
	/// 请求重命名分组——名字输入框由应用层弹。
	RequestRenameGroup {
		/// 目标分组主键。
		id: i64,
		/// 当前名称，作为输入框初值。
		current: String,
	},
	/// 请求删除分组——确认框由应用层弹。
	RequestDeleteGroup {
		/// 目标分组主键。
		id: i64,
	},
	/// 把主机移动到分组。`group` 为 `None` 表示移出到「未分组」。
	MoveHostToGroup {
		/// 目标主机主键。
		host_id: i64,
		/// 目标分组主键。
		group: Option<i64>,
	},
}

/// 侧边栏状态。
#[derive(Default)]
pub struct HostListState {
	/// 搜索关键词。匹配名称与地址，大小写不敏感。
	search: String,
	/// 当前选中的主机主键。
	selected: Option<i64>,
	/// 已折叠的分组主键。纯视图状态，不上报应用层。
	collapsed: BTreeSet<i64>,
}

impl HostListState {
	/// 选中一台主机。
	///
	/// 目前只有无人值守截图钩子用它——选中态（左侧强调条 + 底色）
	/// 是侧边栏最要紧的一处视觉反馈，不能只靠「点一下看看」验。
	pub fn select(&mut self, id: Option<i64>) {
		self.selected = id;
	}
}

/// 分组树的一个节点。
pub struct GroupNode<'a> {
	/// 对应的分组；`None` 表示「未分组」虚拟节点。
	group: Option<&'a Group>,
	/// 子分组（只有顶层节点的 `children` 可能非空）。
	children: Vec<GroupNode<'a>>,
	/// 直接挂在该分组下的主机，已按名称排序。
	hosts: Vec<&'a Host>,
}

impl GroupNode<'_> {
	/// 该节点连同子分组的主机总数。
	pub fn total(&self) -> usize {
		self.hosts.len() + self.children.iter().map(GroupNode::total).sum::<usize>()
	}
}

/// 画侧边栏。本帧有用户动作时返回它。
pub fn ui(
	ui: &mut egui::Ui,
	state: &mut HostListState,
	hosts: &[Host],
	groups: &[Group],
) -> Option<HostListAction> {
	let mut action = None;

	// 搜索框占满整行。这里原来在右侧挂着一枚**裸加号**当「新建分组」入口，
	// 但一枚没有文案的 `+` 紧贴搜索框，读起来是「搜索选项」而不是新建分组——
	// 现在挪到上方与「新建主机」并排，两个入口都写全文字（见应用层）。
	design::search_field(ui, &mut state.search, "搜索名称或地址");

	// 搜索框和分组树之间留一点呼吸：不留的话搜索框像分组的一部分
	ui.add_space(design::GAP_SM);

	// 底部操作条只在选中时出现。**先把它的高度扣掉**再给列表——
	// `ScrollArea` 会吃光可用高度，剩下的按钮就被顶出可视区
	// （曾因此让「编辑 / 删除」整行消失，用户点不到）。
	let selected_host = hosts.iter().find(|host| host.id == state.selected);
	let footer = if selected_host.is_some() {
		design::GAP_XS + 26.0
	} else {
		0.0
	};
	let list_height = (ui.available_height() - footer).max(80.0);

	egui::ScrollArea::vertical()
		.max_height(list_height)
		.show(ui, |ui| {
			let keyword = state.search.trim().to_lowercase();
			let tree = build_tree(hosts, groups, &keyword);

			if tree.is_empty() {
				empty_state(ui, keyword.is_empty());
				return;
			}

			for node in &tree {
				node_ui(ui, state, node, groups, 0, &mut action);
			}
		});

	// 选中一台主机时才露出编辑与删除。常驻的话，
	// 未选中时它们只能是灰的，一排灰按钮纯属噪音。
	if let Some(host) = selected_host {
		// 把操作条推到底部：`ScrollArea` 只占内容高度，列表短的时候
		// 按钮会紧贴在最后一行下面，看起来像列表里的一项。
		let slack = (ui.available_height() - footer).max(0.0);
		ui.add_space(slack + design::GAP_XS);
		ui.horizontal(|ui| {
			ui.spacing_mut().item_spacing.x = 2.0;
			// 只有 SSH 主机有 SFTP；RDP 侧没有这条通道
			if host.protocol == daisy_core::model::Protocol::Ssh
				&& design::ghost_button(ui, "文件").clicked()
			{
				action = Some(HostListAction::OpenFiles(host.clone()));
			}
			if design::ghost_button(ui, "编辑").clicked() {
				action = Some(HostListAction::Edit(host.clone()));
			}
			if design::danger_button(ui, "删除").clicked()
				&& let Some(id) = host.id
			{
				action = Some(HostListAction::Delete(id));
			}
		});
	}

	action
}

/// 列表空的时候给一句话，而不是一片空白。
fn empty_state(ui: &mut egui::Ui, no_hosts: bool) {
	let tokens = design::tokens(ui.ctx());
	ui.add_space(28.0);
	ui.vertical_centered(|ui| {
		ui.label(
			egui::RichText::new(if no_hosts {
				"还没有主机"
			} else {
				"没有匹配的主机"
			})
			.size(design::FONT_BASE)
			.color(tokens.text_muted),
		);
		if no_hosts {
			ui.add_space(2.0);
			ui.label(
				egui::RichText::new("点上方「新建主机」添加第一台")
					.size(design::FONT_XS)
					.color(tokens.text_faint),
			);
		}
	});
}

/// 画一个分组节点（先分组行，再子分组，最后主机）。
fn node_ui(
	ui: &mut egui::Ui,
	state: &mut HostListState,
	node: &GroupNode<'_>,
	groups: &[Group],
	depth: usize,
	action: &mut Option<HostListAction>,
) {
	match node.group {
		Some(group) => {
			let id = group.id.unwrap_or_default();
			let collapsed = state.collapsed.contains(&id);
			let header = group_header(ui, &group.name, node.total(), collapsed, depth);

			// 整行都可点：箭头只有 12px，逼用户瞄准它没必要
			if header.clicked() {
				toggle_collapsed(&mut state.collapsed, id);
			}

			header.context_menu(|ui| {
				super::menu(ui, |ui| {
					// 只支持两层：子分组下不再提供「新建子分组」
					if depth == 0 && ui.button("新建子分组").clicked() {
						*action = Some(HostListAction::NewGroup { parent: Some(id) });
						ui.close();
					}
					if ui.button("重命名").clicked() {
						*action = Some(HostListAction::RequestRenameGroup {
							id,
							current: group.name.clone(),
						});
						ui.close();
					}
					ui.separator();
					if ui.button("删除分组").clicked() {
						*action = Some(HostListAction::RequestDeleteGroup { id });
						ui.close();
					}
				});
			});

			if collapsed {
				return;
			}
		},
		None => {
			// 「未分组」是虚拟节点，用一个哨兵主键让它和真实分组一样可折叠
			let collapsed = state.collapsed.contains(&UNGROUPED_ID);
			let header = group_header(ui, "未分组", node.hosts.len(), collapsed, depth);
			if header.clicked() {
				toggle_collapsed(&mut state.collapsed, UNGROUPED_ID);
			}
			if collapsed {
				return;
			}
		},
	}

	for child in &node.children {
		node_ui(ui, state, child, groups, depth + 1, action);
	}

	for host in &node.hosts {
		host_row_ui(ui, state, host, groups, depth + 1, action);
	}
}

/// 切换一个分组（或「未分组」哨兵）的折叠状态。
fn toggle_collapsed(collapsed: &mut BTreeSet<i64>, id: i64) {
	if !collapsed.remove(&id) {
		collapsed.insert(id);
	}
}

/// 一行分组标题：折叠箭头 + 名称 + 数量，整行可点。
///
/// 箭头用 `Painter` 画而不是打 `▸`/`▾`——CJK 字体（Hiragino Sans GB）
/// 没有这两个字形，打出来是方框（实测）。
fn group_header(
	ui: &mut egui::Ui,
	name: &str,
	count: usize,
	collapsed: bool,
	depth: usize,
) -> egui::Response {
	let tokens = design::tokens(ui.ctx());
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::click());
	let painter = ui.painter();
	if response.hovered() {
		painter.rect_filled(rect, design::RADIUS_SM, tokens.bg_hover);
	}

	let left = rect.left() + 4.0 + depth as f32 * 12.0;
	let chevron = egui::Rect::from_center_size(
		egui::pos2(left + 7.0, rect.center().y),
		egui::Vec2::splat(12.0),
	);
	design::icon_chevron(painter, chevron, tokens.text_faint, !collapsed);

	// 裁到行内：分组名过长时不能溢出到隔壁面板
	let clip = painter.with_clip_rect(rect);
	let name_rect = clip.text(
		egui::pos2(left + 19.0, rect.center().y),
		egui::Align2::LEFT_CENTER,
		name,
		egui::FontId::proportional(design::FONT_XS + 1.0),
		tokens.text_muted,
	);
	clip.text(
		egui::pos2(name_rect.right() + 5.0, rect.center().y),
		egui::Align2::LEFT_CENTER,
		count.to_string(),
		egui::FontId::proportional(design::FONT_XS),
		tokens.text_faint,
	);
	response
}

/// 「未分组」虚拟节点的折叠状态主键。
///
/// 真实主键是非负自增 id，取 `i64::MIN` 不可能撞上。
const UNGROUPED_ID: i64 = i64::MIN;

/// 画一行主机：协议徽标 + 名称 + 端点。
///
/// 拆成两行是因为原来挤成一行的 `[RDP] Administrator@192.168.10.20:3389`
/// 在窄侧边栏里会先挤掉最要紧的那半截——名称，留下的全是地址。
fn host_row_ui(
	ui: &mut egui::Ui,
	state: &mut HostListState,
	host: &Host,
	groups: &[Group],
	depth: usize,
	action: &mut Option<HostListAction>,
) {
	let tokens = design::tokens(ui.ctx());
	let selected = state.selected == host.id;
	let (rect, response) =
		ui.allocate_exact_size(egui::vec2(ui.available_width(), 42.0), egui::Sense::click());

	let painter = ui.painter();
	if selected {
		painter.rect_filled(rect, design::RADIUS_MD, tokens.bg_active);
		// 左侧强调条：小面板里只靠一层底色差异，选中与否不够分明
		painter.rect_filled(
			egui::Rect::from_min_size(
				egui::pos2(rect.left(), rect.top() + 9.0),
				egui::vec2(2.0, rect.height() - 18.0),
			),
			1,
			tokens.accent,
		);
	} else if response.hovered() {
		painter.rect_filled(rect, design::RADIUS_MD, tokens.bg_hover);
	}

	// 裁到行内：名称与地址都不能溢出到隔壁面板
	let clip = painter.with_clip_rect(rect);
	let left = rect.left() + 9.0 + depth as f32 * 12.0;

	let name_font = egui::FontId::proportional(design::FONT_BASE);
	let endpoint_font = egui::FontId::proportional(design::FONT_XS);
	let name_color = if selected {
		tokens.text
	} else {
		tokens.text_dim
	};

	// **两行文字作为一整块垂直居中**，不再各自从 `rect.top()` 加固定偏移。
	// 固定偏移在字体度量变化时会整体偏上或偏下（中文字体做过墨迹改正后就会
	// 下移 3pt），看起来像「这一行没坐正」。
	let name = clip.layout_no_wrap(host.name.clone(), name_font.clone(), name_color);
	let endpoint = clip.layout_no_wrap(endpoint_text(host), endpoint_font, tokens.text_muted);
	/// 名称与地址之间的行距。
	const LINE_GAP: f32 = 1.0;
	let block = name.size().y + LINE_GAP + endpoint.size().y;
	let block_top = rect.center().y - block * 0.5;
	clip.galley(
		egui::pos2(left + 1.0, block_top + name.size().y + LINE_GAP),
		endpoint,
		tokens.text_muted,
	);

	// 协议徽标与**名称的墨迹**同高：徽标是实心块，按行框对会显得低半格
	let (badge_fg, badge_bg) = badge_colors(host.protocol, &tokens);
	let name_center_y = block_top + name.mesh_bounds.center().y;
	let badge_width = super::paint_badge(
		&clip,
		left,
		name_center_y,
		protocol_tag(host.protocol),
		badge_fg,
		badge_bg,
	);

	clip.galley(egui::pos2(left + badge_width + 6.0, block_top), name, name_color);

	if response.clicked() {
		state.selected = host.id;
	}
	// 双击连接——终端用户对「双击打开」的预期根深蒂固
	if response.double_clicked() {
		*action = Some(HostListAction::Connect(host.clone()));
	}

	response
		// 侧边栏窄时端点会被裁掉半截，悬停给出完整内容
		.on_hover_text(format!("{}\n双击连接", endpoint_text(host)))
		.context_menu(|ui| {
			super::menu(ui, |ui| {
				if ui.button("连接").clicked() {
					*action = Some(HostListAction::Connect(host.clone()));
					ui.close();
				}
				if host.protocol == daisy_core::model::Protocol::Ssh
					&& ui.button("文件…").clicked()
				{
					*action = Some(HostListAction::OpenFiles(host.clone()));
					ui.close();
				}
				if ui.button("编辑").clicked() {
					*action = Some(HostListAction::Edit(host.clone()));
					ui.close();
				}
				ui.menu_button("移动到", |ui| {
					if ui.button("未分组").clicked() {
						*action = Some(HostListAction::MoveHostToGroup {
							host_id: host.id.unwrap_or_default(),
							group: None,
						});
						ui.close();
					}
					// 列出全部分组；子分组用「父 / 子」表示——两层结构下主机
					// 也可以直接挂在子分组里，菜单不能只给顶层入口
					for group in groups {
						if ui
							.button(super::group_display_name(group, groups))
							.clicked()
						{
							*action = Some(HostListAction::MoveHostToGroup {
								host_id: host.id.unwrap_or_default(),
								group: group.id,
							});
							ui.close();
						}
					}
				});
				ui.separator();
				if ui.button("删除").clicked() {
					*action = Some(HostListAction::Delete(host.id.unwrap_or_default()));
					ui.close();
				}
			});
		});
}

/// `用户@地址:端口`（没填用户名时省略用户段）。
fn endpoint_text(host: &Host) -> String {
	match &host.username {
		Some(user) => format!("{user}@{}", daisy_core::endpoint(&host.host, host.port)),
		None => daisy_core::endpoint(&host.host, host.port),
	}
}

/// 协议徽标上的字。
fn protocol_tag(protocol: Protocol) -> &'static str {
	match protocol {
		Protocol::Ssh => "SSH",
		Protocol::Rdp => "RDP",
	}
}

/// 徽标配色：RDP 走强调色、SSH 走成功色——同一列表里两种协议要一眼分得开。
fn badge_colors(protocol: Protocol, tokens: &design::Tokens) -> (egui::Color32, egui::Color32) {
	match protocol {
		Protocol::Rdp => (tokens.accent_text, tokens.accent_fill),
		Protocol::Ssh => (tokens.success_text, tokens.success_fill),
	}
}

/// 搜索匹配。空关键词匹配全部。
fn matches(host: &Host, keyword: &str) -> bool {
	if keyword.is_empty() {
		return true;
	}
	host.name.to_lowercase().contains(keyword) || host.host.to_lowercase().contains(keyword)
}

/// 组装分组树。
///
/// 规则：
/// - 顶层分组在前、子分组跟随其后，「未分组」垫底（且仅在非空时出现）；
/// - 组内主机按名称排序（不区分大小写，同名再比地址）；
/// - `group_id` 指向不存在的分组时回退到「未分组」——外键理论上挡住这种数据，
///   但真出现时**宁可多显示也不要让主机凭空消失**；
/// - 搜索关键词非空时空分组不显示（留着只是噪音）。
fn build_tree<'a>(hosts: &'a [Host], groups: &'a [Group], keyword: &str) -> Vec<GroupNode<'a>> {
	let known: BTreeSet<i64> = groups.iter().filter_map(|group| group.id).collect();

	// 主机先按分组分桶，再统一排序
	let mut buckets: HashMap<Option<i64>, Vec<&Host>> = HashMap::new();
	for host in hosts.iter().filter(|host| matches(host, keyword)) {
		let key = host.group_id.filter(|id| known.contains(id));
		buckets.entry(key).or_default().push(host);
	}
	for bucket in buckets.values_mut() {
		// 用 cached：key 是 String，只构造一次而不是每次比较都分配
		bucket.sort_by_cached_key(|host| sort_key(host));
	}

	let mut out = Vec::new();
	for group in groups
		.iter()
		.filter(|group| group.parent_id.is_none() && group.id.is_some())
	{
		let id = group.id.expect("上面已过滤 id 为空的分组");

		let mut children = Vec::new();
		for child in groups
			.iter()
			.filter(|child| child.parent_id == Some(id) && child.id.is_some())
		{
			let child_id = child.id.expect("上面已过滤 id 为空的分组");
			children.push(GroupNode {
				group: Some(child),
				children: Vec::new(),
				hosts: buckets.remove(&Some(child_id)).unwrap_or_default(),
			});
		}

		out.push(GroupNode {
			group: Some(group),
			children,
			hosts: buckets.remove(&Some(id)).unwrap_or_default(),
		});
	}

	if !keyword.is_empty() {
		out.retain(|node| node.total() > 0);
	}

	let ungrouped = buckets.remove(&None).unwrap_or_default();
	if !ungrouped.is_empty() {
		out.push(GroupNode {
			group: None,
			children: Vec::new(),
			hosts: ungrouped,
		});
	}

	out
}

/// 排序键：名称（不区分大小写）优先，同名再比地址。
fn sort_key(host: &Host) -> (String, String) {
	(host.name.to_lowercase(), host.host.to_lowercase())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn host(id: i64, name: &str, group: Option<i64>) -> Host {
		let mut host = Host::draft(Protocol::Ssh);
		host.id = Some(id);
		host.name = name.to_string();
		host.host = format!("10.0.0.{id}");
		host.group_id = group;
		host
	}

	fn group(id: i64, name: &str, parent: Option<i64>) -> Group {
		Group {
			id: Some(id),
			name: name.to_string(),
			parent_id: parent,
		}
	}

	fn names(nodes: &[GroupNode<'_>]) -> Vec<String> {
		nodes
			.iter()
			.map(|node| match node.group {
				Some(group) => group.name.clone(),
				None => "未分组".to_string(),
			})
			.collect()
	}

	#[test]
	fn groups_hosts_under_their_group() {
		let groups = vec![group(1, "生产", None), group(2, "测试", None)];
		let hosts = vec![host(10, "a", Some(1)), host(11, "b", Some(2))];

		let tree = build_tree(&hosts, &groups, "");
		assert_eq!(names(&tree), vec!["生产", "测试"]);

		let production = tree.iter().find(|n| n.group.map(|g| g.id) == Some(Some(1)));
		let production = production.expect("应有生产分组");
		assert_eq!(production.hosts.len(), 1);
		assert_eq!(production.hosts[0].name, "a");
		assert_eq!(production.total(), 1);
	}

	#[test]
	fn nests_children_under_parent() {
		let groups = vec![group(1, "顶层", None), group(2, "子级", Some(1))];
		let hosts = vec![
			host(10, "child-host", Some(2)),
			host(11, "top-host", Some(1)),
		];

		let tree = build_tree(&hosts, &groups, "");
		assert_eq!(names(&tree), vec!["顶层"], "子分组不应单独出现在顶层");

		let top = &tree[0];
		assert_eq!(top.hosts.len(), 1, "顶层直接挂的主机");
		assert_eq!(top.children.len(), 1, "应有一个子分组");
		assert_eq!(top.children[0].hosts.len(), 1);
		assert_eq!(top.total(), 2, "计数应含子分组");
	}

	#[test]
	fn sorts_hosts_by_name_case_insensitively() {
		let groups = vec![];
		let hosts = vec![
			host(1, "Zebra", None),
			host(2, "apple", None),
			host(3, "Banana", None),
		];

		let tree = build_tree(&hosts, &groups, "");
		let order: Vec<&str> = tree[0].hosts.iter().map(|h| h.name.as_str()).collect();
		assert_eq!(order, vec!["apple", "Banana", "Zebra"]);
	}

	#[test]
	fn ungrouped_comes_last_and_only_when_non_empty() {
		let groups = vec![group(1, "生产", None)];
		let hosts = vec![host(10, "a", Some(1)), host(11, "orphan", None)];

		let tree = build_tree(&hosts, &groups, "");
		assert_eq!(names(&tree), vec!["生产", "未分组"]);

		// 没有未分组主机时不出现该节点
		let tree = build_tree(&hosts[..1], &groups, "");
		assert_eq!(names(&tree), vec!["生产"]);
	}

	#[test]
	fn unknown_group_id_falls_back_to_ungrouped() {
		// 分组 99 不存在——主机不能因此消失
		let groups = vec![group(1, "生产", None)];
		let hosts = vec![host(10, "ghost", Some(99))];

		let tree = build_tree(&hosts, &groups, "");
		assert_eq!(names(&tree), vec!["生产", "未分组"]);
		assert_eq!(tree[1].hosts[0].name, "ghost");
	}

	#[test]
	fn search_keeps_matching_hosts_and_hides_empty_groups() {
		let groups = vec![group(1, "生产", None), group(2, "测试", None)];
		let hosts = vec![
			host(10, "web-01", Some(1)),
			host(11, "db-01", Some(1)),
			host(12, "sandbox", Some(2)),
		];

		let tree = build_tree(&hosts, &groups, "web");
		assert_eq!(names(&tree), vec!["生产"], "无命中的分组应隐藏");
		assert_eq!(tree[0].hosts.len(), 1);
		assert_eq!(tree[0].hosts[0].name, "web-01");
	}

	#[test]
	fn search_matches_address_too() {
		let hosts = vec![host(10, "web-01", None)];
		let tree = build_tree(&hosts, &[], "10.0.0.10");
		assert_eq!(tree.len(), 1);
		assert_eq!(tree[0].hosts.len(), 1);
	}

	#[test]
	fn search_keeps_parent_when_only_child_matches() {
		let groups = vec![group(1, "顶层", None), group(2, "子级", Some(1))];
		let hosts = vec![host(10, "deep-match", Some(2))];

		let tree = build_tree(&hosts, &groups, "deep");
		assert_eq!(names(&tree), vec!["顶层"], "父节点应因子的命中而保留");
		assert_eq!(tree[0].total(), 1);
	}

	#[test]
	fn empty_search_keeps_empty_groups() {
		// 不搜索时空分组要留着，否则用户没法右键管理它
		let groups = vec![group(1, "空分组", None)];
		let tree = build_tree(&[], &groups, "");
		assert_eq!(names(&tree), vec!["空分组"]);
	}
}
