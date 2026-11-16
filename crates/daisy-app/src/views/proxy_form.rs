//! 新建 / 编辑代理表单。
//!
//! 代理密码同样不进 [`Proxy`] 模型——只以密文落在 `proxies.secret_enc`。
//! 「留空即保持原值」由应用层兑现（存储层的 update 是整列覆盖）。

use daisy_core::model::{Proxy, ProxyKind};

use crate::design;

/// 表单回传的原始输入。
pub struct ProxyFormInput {
	/// 填好的代理（不含密码）。
	pub proxy: Proxy,
	/// 密码输入框内容。编辑时留空表示「未改动」。
	pub password: String,
}

/// 表单本帧的结局。
pub enum ProxyFormOutcome {
	/// 仍在编辑。
	Open,
	/// 用户取消或关闭了窗口。
	Cancelled,
	/// 校验通过，可以落库。
	Save(Box<ProxyFormInput>),
}

/// 表单状态。
pub struct ProxyFormState {
	/// 正在编辑的代理。`id == None` 表示新建。
	proxy: Proxy,
	/// 用户名输入框。空表示不填。
	username: String,
	/// 密码输入框内容。
	password: String,
	/// 是否新建。
	is_new: bool,
	/// 上一次保存尝试的校验错误。
	error: Option<String>,
}

impl ProxyFormState {
	/// 新建一个 SOCKS5 代理（类型可改）。
	pub fn new_proxy() -> Self {
		Self {
			proxy: Proxy {
				id: None,
				name: String::new(),
				kind: ProxyKind::Socks5,
				host: String::new(),
				port: 1080,
				username: None,
			},
			username: String::new(),
			password: String::new(),
			is_new: true,
			error: None,
		}
	}

	/// 编辑既有代理。密码不回显。
	pub fn edit(proxy: &Proxy) -> Self {
		Self {
			proxy: proxy.clone(),
			username: proxy.username.clone().unwrap_or_default(),
			password: String::new(),
			is_new: false,
			error: None,
		}
	}
}

/// 画表单窗口。
pub fn ui(ctx: &egui::Context, state: &mut ProxyFormState) -> ProxyFormOutcome {
	let mut open = true;
	let mut outcome = ProxyFormOutcome::Open;

	super::dialog(
		ctx,
		if state.is_new {
			"新建代理"
		} else {
			"编辑代理"
		},
		[420.0, 372.0],
		&mut open,
		|ui| {
			super::scroll_body(ui, |ui| {
				ui.spacing_mut().item_spacing.y = super::ROW_GAP;
				super::field(ui, "名称", |ui| {
					super::text_row(ui, egui::TextEdit::singleline(&mut state.proxy.name))
				});

				super::field_ui(ui, "类型", |ui| {
					egui::ComboBox::from_id_salt("proxy_kind")
						.selected_text(match state.proxy.kind {
							ProxyKind::Socks5 => "SOCKS5",
							ProxyKind::Http => "HTTP CONNECT",
						})
						.show_ui(ui, |ui| {
							ui.selectable_value(&mut state.proxy.kind, ProxyKind::Socks5, "SOCKS5");
							ui.selectable_value(
								&mut state.proxy.kind,
								ProxyKind::Http,
								"HTTP CONNECT",
							);
						});
				});

				super::field(ui, "地址", |ui| {
					super::text_row(
						ui,
						egui::TextEdit::singleline(&mut state.proxy.host)
							.hint_text("代理服务器 IP / 域名"),
					)
				});

				super::field(ui, "端口", |ui| {
					super::number_row(
						ui,
						100.0,
						egui::DragValue::new(&mut state.proxy.port).range(1..=65535),
					)
				});

				super::field(ui, "用户名", |ui| {
					super::text_row(
						ui,
						egui::TextEdit::singleline(&mut state.username).hint_text("可选"),
					)
				});

				super::field(ui, "密码", |ui| {
					super::password_row(
						ui,
						egui::TextEdit::singleline(&mut state.password).password(true),
					)
				});
				if !state.is_new {
					super::hint(ui, "留空表示保持已保存的密码不变。");
				}

				if let Some(error) = &state.error {
					ui.add_space(6.0);
					ui.colored_label(ui.visuals().error_fg_color, error);
				}
			});

			super::divider(ui);
			ui.horizontal(|ui| {
				ui.spacing_mut().item_spacing.x = 2.0;
				if design::filled_button(ui, "保存").clicked() {
					match build_input(state) {
						Ok(input) => outcome = ProxyFormOutcome::Save(Box::new(input)),
						Err(error) => state.error = Some(error),
					}
				}
				if design::ghost_button(ui, "取消").clicked() {
					outcome = ProxyFormOutcome::Cancelled;
				}
			});
		},
	);

	if !open {
		return ProxyFormOutcome::Cancelled;
	}
	outcome
}

/// 收拢输入并校验。
fn build_input(state: &ProxyFormState) -> Result<ProxyFormInput, String> {
	state.proxy.validate().map_err(|error| error.to_string())?;

	let mut proxy = state.proxy.clone();
	proxy.username = if state.username.trim().is_empty() {
		None
	} else {
		Some(state.username.trim().to_owned())
	};

	Ok(ProxyFormInput {
		proxy,
		password: state.password.clone(),
	})
}
