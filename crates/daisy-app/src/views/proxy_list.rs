//! 侧边栏「代理」页：代理列表。
//!
//! 与主机列表同构：只展示与收集点击，动作经 [`ProxyListAction`] 回传。

use daisy_core::model::Proxy;

/// 代理列表触发的动作。
pub enum ProxyListAction {
	/// 新建代理。
	NewProxy,
	/// 编辑代理。
	Edit(Proxy),
	/// 删除代理（主键）。
	Delete(i64),
}

/// 代理列表状态。
#[derive(Default)]
pub struct ProxyListState {
	/// 搜索关键词。匹配名称与地址，大小写不敏感。
	search: String,
	/// 当前选中的代理主键。
	selected: Option<i64>,
}

/// 画代理列表。本帧有用户动作时返回它。
pub fn ui(
	ui: &mut egui::Ui,
	state: &mut ProxyListState,
	proxies: &[Proxy],
) -> Option<ProxyListAction> {
	let mut action = None;

	ui.add_space(6.0);
	ui.horizontal(|ui| {
		ui.strong("代理");
		ui.weak(format!("({})", proxies.len()));
	});
	ui.add_space(4.0);
	ui.add(
		egui::TextEdit::singleline(&mut state.search)
			.hint_text("搜索名称或地址")
			.desired_width(f32::INFINITY),
	);
	ui.add_space(4.0);
	ui.separator();

	egui::ScrollArea::vertical().show(ui, |ui| {
		let keyword = state.search.trim().to_lowercase();
		let visible: Vec<&Proxy> = proxies
			.iter()
			.filter(|proxy| {
				keyword.is_empty()
					|| proxy.name.to_lowercase().contains(&keyword)
					|| proxy.host.to_lowercase().contains(&keyword)
			})
			.collect();

		if visible.is_empty() {
			ui.weak(if keyword.is_empty() {
				"还没有代理。点「新建」添加第一个。"
			} else {
				"没有匹配的代理。"
			});
			return;
		}

		for proxy in visible {
			let selected = state.selected == proxy.id;
			let text = format!(
				"{}\n{} {}:{}",
				proxy.name, proxy.kind, proxy.host, proxy.port
			);
			let response = ui.selectable_label(selected, text);
			if response.clicked() {
				state.selected = proxy.id;
			}
			if response.double_clicked() {
				action = Some(ProxyListAction::Edit(proxy.clone()));
			}
		}
	});

	ui.separator();
	ui.horizontal(|ui| {
		if ui.button("新建").clicked() {
			action = Some(ProxyListAction::NewProxy);
		}

		let selected_proxy = proxies.iter().find(|proxy| proxy.id == state.selected);
		ui.add_enabled_ui(selected_proxy.is_some(), |ui| {
			if ui.button("编辑").clicked()
				&& let Some(proxy) = selected_proxy
			{
				action = Some(ProxyListAction::Edit(proxy.clone()));
			}
			if ui.button("删除").clicked()
				&& let Some(proxy) = selected_proxy
				&& let Some(id) = proxy.id
			{
				action = Some(ProxyListAction::Delete(id));
			}
			false
		});
	});

	action
}
