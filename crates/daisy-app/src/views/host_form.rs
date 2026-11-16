//! 新建 / 编辑主机表单。
//!
//! 凭据（密码、私钥口令）**不进 [`Host`] 模型**——模型里只留 `has_passphrase`
//! 这类布尔标记。表单把用户输入的原样带回，由应用层决定「留空即保持原值」
//! 的语义并写入加密库。
//!
//! 连通性测试只置一个标志，真正的 TCP 探测由应用层跑（要 tokio），
//! 结果经 [`HostFormState::set_probe_result`] 回填——视图不碰异步。

use daisy_core::addr::HostAddr;
use daisy_core::model::{AuthMethod, Group, Host, Protocol, Proxy, PublicKeyAuth};

use crate::design;

/// 表单回传的原始输入。凭据是否落库由应用层结合既有数据决定。
pub struct HostFormInput {
	/// 填好的主机（不含任何凭据）。
	pub host: Host,
	/// 密码输入框内容。编辑时留空表示「未改动」。
	pub password: String,
	/// 私钥口令输入框内容。编辑时留空表示「未改动」。
	pub passphrase: String,
}

/// 表单本帧的结局。
///
/// `Save` 装箱：表单输入带一个完整的 `Host`，另外两个分支是空枚举。
pub enum HostFormOutcome {
	/// 仍在编辑。
	Open,
	/// 用户取消或关闭了窗口。
	Cancelled,
	/// 校验通过，可以落库。
	Save(Box<HostFormInput>),
}

/// 连通性测试的状态。
#[derive(Debug, Clone, Default)]
pub enum ProbeStatus {
	/// 没测过。
	#[default]
	Idle,
	/// 测试中。
	Testing,
	/// 测完了：`reachable` 决定绿字还是红字，`detail` 是失败原因。
	Done {
		/// 端口是否可达。
		reachable: bool,
		/// 失败原因（成功时为空）。
		detail: String,
	},
}

/// 表单状态。
pub struct HostFormState {
	/// 正在编辑的主机。`id == None` 表示新建。
	host: Host,
	/// 用户名输入框。空表示不填（RDP 常见）。
	username: String,
	/// 密码输入框内容。
	password: String,
	/// 私钥口令输入框内容。
	passphrase: String,
	/// 是否新建。决定「留空」的语义。
	is_new: bool,
	/// 上一次保存尝试的校验错误。
	error: Option<String>,
	/// 连通性测试状态。
	probe_status: ProbeStatus,
	/// 本帧是否点了「测试连接」。应用层取走后即清零。
	probe_requested: bool,
	/// 库里记录的 RDP 证书指纹（已格式化，给人看的）。
	///
	/// **由应用层填**：视图不碰数据库。新建主机或 SSH 主机恒为 `None`。
	known_cert: Option<String>,
	/// 上面那枚指纹属于哪个 `(地址, 端口)`。
	///
	/// 用户在表单里改了地址之后，这条记录就不再适用——继续显示会让人以为
	/// 「新地址也是这个指纹」。所以渲染前要先比对。
	cert_for: Option<(String, u16)>,
	/// 本帧是否点了「重新信任」。取走即清零。
	forget_cert_requested: bool,
}

impl HostFormState {
	/// 新建一台 SSH 主机（协议可改）。
	pub fn new_host() -> Self {
		Self {
			host: Host::draft(Protocol::Ssh),
			username: String::new(),
			password: String::new(),
			passphrase: String::new(),
			is_new: true,
			error: None,
			probe_status: ProbeStatus::Idle,
			probe_requested: false,
			known_cert: None,
			cert_for: None,
			forget_cert_requested: false,
		}
	}

	/// 编辑既有主机。凭据输入框留空——已保存的凭据不回显。
	pub fn edit(host: &Host) -> Self {
		Self {
			host: host.clone(),
			username: host.username.clone().unwrap_or_default(),
			password: String::new(),
			passphrase: String::new(),
			is_new: false,
			error: None,
			probe_status: ProbeStatus::Idle,
			probe_requested: false,
			known_cert: None,
			cert_for: None,
			forget_cert_requested: false,
		}
	}

	/// 用户是否在本帧点了「测试连接」。取走即清零。
	pub fn take_probe_request(&mut self) -> bool {
		let requested = self.probe_requested;
		self.probe_requested = false;
		requested
	}

	/// 标记测试中。
	pub fn set_probe_testing(&mut self) {
		self.probe_status = ProbeStatus::Testing;
	}

	/// 代为发起一次测试连接（无人值守验证用）。
	///
	/// 正常路径是用户点按钮置位 [`Self::probe_requested`]；截图核对绿字/红字时
	/// 点不到按钮（辅助功能权限拦着），由应用自己置位。
	pub fn request_probe(&mut self) {
		self.probe_requested = true;
	}

	/// 预填密码（无人值守验证用）。
	///
	/// 掩码点的密度只有填了字才看得见，而脚本敲不了键。
	pub fn set_password(&mut self, value: &str) {
		self.password = value.to_string();
	}

	/// 回填测试结论。
	pub fn set_probe_result(&mut self, reachable: bool, detail: impl Into<String>) {
		self.probe_status = ProbeStatus::Done {
			reachable,
			detail: detail.into(),
		};
	}

	/// 告知这台机器已记录的证书指纹（`shown` 已格式化）。
	///
	/// `host`/`port` 是**库里那条记录对应的**地址与端口，而不是表单当前值——
	/// 用户一改地址，界面就该把旧指纹收起来（见 [`cert_applies`]）。
	pub fn set_known_cert(&mut self, shown: Option<String>, host: &str, port: u16) {
		self.known_cert = shown;
		self.cert_for = Some((host.to_owned(), port));
	}

	/// 取走「重新信任」点击，返回要清除记录的 `(地址, 端口)`。
	///
	/// 取走时先把界面上的指纹抹掉：按钮点下去就该立刻有反馈，
	/// 而删库失败时应用层会用 [`Self::set_known_cert`] 填回去。
	pub fn take_forget_cert_request(&mut self) -> Option<(String, u16)> {
		if !self.forget_cert_requested {
			return None;
		}
		self.forget_cert_requested = false;
		self.known_cert = None;
		self.cert_for.clone()
	}

	/// 当前填写的地址与端口——连通性测试的目标（不一定是已保存的值）。
	///
	/// 地址在这里**去掉 IPv6 的方括号**：`TcpStream::connect` 收的是裸地址，
	/// `[fe80::1]` 会被当成域名去解析，测试必失败而地址其实没错。
	#[must_use]
	pub fn probe_target(&self) -> (String, u16) {
		let raw = self.host.host.trim();
		let host = HostAddr::parse(raw)
			.map(|addr| addr.host().to_owned())
			.unwrap_or_else(|_| raw.to_owned());
		(host, self.host.port)
	}
}

/// 画表单窗口。
///
/// `proxies`、`ssh_hosts` 与 `groups` 只用于下拉选项，不回写。
pub fn ui(
	ctx: &egui::Context,
	state: &mut HostFormState,
	proxies: &[Proxy],
	ssh_hosts: &[Host],
	groups: &[Group],
) -> HostFormOutcome {
	let mut open = true;
	let mut outcome = HostFormOutcome::Open;

	super::dialog(
		ctx,
		if state.is_new {
			"新建主机"
		} else {
			"编辑主机"
		},
		[440.0, 760.0],
		&mut open,
		|ui| {
			super::scroll_body(ui, |ui| {
				ui.spacing_mut().item_spacing.y = super::ROW_GAP;
				super::field(ui, "名称", |ui| {
					super::text_row(ui, egui::TextEdit::singleline(&mut state.host.name))
				});

				// 协议。RDP 的连接界面尚在 S8 收尾，先允许保存。
				super::field_ui(ui, "协议", |ui| {
					egui::ComboBox::from_id_salt("protocol")
						.selected_text(state.host.protocol.as_str().to_uppercase())
						.show_ui(ui, |ui| {
							ui.selectable_value(&mut state.host.protocol, Protocol::Ssh, "SSH");
							ui.selectable_value(&mut state.host.protocol, Protocol::Rdp, "RDP");
						});
				});

				super::field(ui, "地址", |ui| {
					super::text_row(
						ui,
						egui::TextEdit::singleline(&mut state.host.host)
							.hint_text("IP / 域名 / [IPv6]"),
					)
				});
				super::hint(ui, "支持 IPv4、域名与 IPv6；IPv6 可带方括号。");

				// 剪贴板映射。对应 mstsc 连接前的那个勾选项，放在主机配置里
				// 而不是每次连接都问——问一次就得记住，记住就得落库，
				// 而多数人从头到尾都是同一个答案。
				// 只对 RDP 有意义：SSH 那边的复制粘贴是纯本地的，与远程无关。
				if state.host.protocol == Protocol::Rdp {
					super::field_ui(ui, "剪贴板", |ui| {
						ui.checkbox(&mut state.host.clipboard, "映射剪贴板");
					});
					super::hint(ui, "关闭后不安装 cliprdr 通道，Cmd/Ctrl+C/V 不再跨机同步。");
				}

				super::field(ui, "端口", |ui| {
					super::number_row(
						ui,
						100.0,
						egui::DragValue::new(&mut state.host.port).range(1..=65535),
					)
				});

				// 连通性测试。目标取当前填写值，不等保存。
				//
				// 标签列留空但**必须占位**：不留的话按钮会顶到内容最左边，
				// 与上下几行的控件不在一条竖线上。
				super::field_ui(ui, "", |ui| {
					let (host, port) = state.probe_target();
					ui.add_enabled_ui(!host.is_empty(), |ui| {
						if ui.button("测试连接").clicked() {
							state.probe_requested = true;
						}
						false
					});
					let tokens = design::tokens(ui.ctx());
					match &state.probe_status {
						ProbeStatus::Idle => {
							if host.is_empty() {
								ui.weak("先填地址");
							}
						},
						ProbeStatus::Testing => {
							ui.spinner();
							ui.weak("测试中…");
						},
						// 结论就是一句话：绿字「连接正常」/ 红字「连接异常：原因」。
						// 不再把「地址 — 地址 可以连接」摆出来——地址就在上面一行，
						// 重复一遍反而把真正的结论挤到看不清。
						ProbeStatus::Done { reachable, detail } => {
							let (text, color) = if *reachable {
								("连接正常".to_string(), tokens.success)
							} else {
								(format!("连接异常：{detail}"), tokens.danger)
							};
							// 必须 `.wrap()`：横向布局里 `Label` 默认 `Extend` 不换行，
							// 一条长错误会把对话框顶宽（与说明文字同一个坑）
							ui.add(egui::Label::new(egui::RichText::new(text).color(color)).wrap())
								.on_hover_text(format!(
									"只测试 {} 的 TCP 端口是否可达，不校验账号密码。",
									daisy_core::endpoint(&host, port)
								));
						},
					}
				});

				// 证书指纹（只有编辑已保存的 RDP 主机、且**已经记过**才看得到）。
				//
				// 放在「测试连接」之后：它属于连接配置，但不是每次都要看的字段，
				// 夹在端口和测试按钮之间会把这两件事拆开。
				//
				// 这里是「指纹变更」那条错误文案的落点：文案让用户「先删除该主机的
				// 证书记录再连」，那就必须有个地方能删——否则等于让人去找数据库。
				//
				// 空态（从没连过）整块不画：那句「尚未记录」是占位不是信息，
				// 却要占掉「标题 + 说明 + 一行正文」三行的位置，把表单最后的
				// 代理 / 跳板机挤到需要滚动才看得见。首次连接记下指纹后这一节
				// 自然出现，那时才有指纹可核对、也有「重新信任」可点。
				if state.host.protocol == Protocol::Rdp
					&& !state.is_new
					&& state.known_cert.is_some()
				{
					cert_section(ui, state);
				}

				super::field(ui, "用户名", |ui| {
					super::text_row(
						ui,
						egui::TextEdit::singleline(&mut state.username).hint_text("登录用户"),
					)
				});

				super::divider(ui);

				// 认证方式。只在**真正切换**时重建 auth——
				// 重复点击当前选项会清掉已经填好的私钥路径
				super::field_ui(ui, "认证", |ui| {
					let mut use_key = state.host.auth.is_public_key();
					let before = use_key;
					let switched = ui.radio_value(&mut use_key, false, "密码").clicked()
						|| ui.radio_value(&mut use_key, true, "公钥").clicked();
					if switched && use_key != before {
						state.host.auth = if use_key {
							AuthMethod::PublicKey(PublicKeyAuth {
								key_path: String::new(),
								has_passphrase: false,
							})
						} else {
							AuthMethod::Password
						};
					}
				});

				match &mut state.host.auth {
					AuthMethod::Password => {
						super::field(ui, "密码", |ui| {
							super::password_row(
								ui,
								egui::TextEdit::singleline(&mut state.password).password(true),
							)
						});
						if !state.is_new {
							super::hint(ui, "留空表示保持已保存的密码不变。");
						}
					},
					AuthMethod::PublicKey(pk) => {
						super::field(ui, "私钥路径", |ui| {
							super::text_row(
								ui,
								egui::TextEdit::singleline(&mut pk.key_path)
									.hint_text("~/.ssh/id_rsa"),
							)
						});

						super::field_ui(ui, "口令", |ui| {
							ui.checkbox(&mut pk.has_passphrase, "私钥有口令");
						});

						if pk.has_passphrase {
							super::field(ui, "私钥口令", |ui| {
								super::password_row(
									ui,
									egui::TextEdit::singleline(&mut state.passphrase)
										.password(true),
								)
							});
							if !state.is_new {
								super::hint(ui, "留空表示保持已保存的口令不变。");
							}
						}
					},
				}

				super::divider(ui);

				// 分组。纯组织属性，不影响连接
				group_picker(
					ui,
					"group_pick",
					"分组",
					state.host.group_id,
					groups,
					|picked| state.host.group_id = picked,
				);
				if groups.is_empty() {
					super::hint(ui, "还没有分组。可在侧边栏右键「新建分组」。");
				}

				// 代理。SSH 与 RDP 都能走；留空 = 直连
				proxy_picker(
					ui,
					"proxy_pick",
					"代理",
					state.host.proxy_id,
					proxies,
					|picked| state.host.proxy_id = picked,
				);
				if proxies.is_empty() {
					super::hint(ui, "还没有代理。可在侧边栏「代理」页新建。");
				}

				// 跳板机只有 SSH 有意义；候选排除自己，否则成环
				if state.host.protocol == Protocol::Ssh {
					let candidates: Vec<&Host> = ssh_hosts
						.iter()
						.filter(|candidate| candidate.id != state.host.id)
						.collect();
					jump_picker(
						ui,
						"jump_pick",
						"跳板机",
						state.host.jump_host_id,
						&candidates,
						|picked| state.host.jump_host_id = picked,
					);
					if candidates.is_empty() {
						super::hint(ui, "没有可选的跳板机（需要另一台 SSH 主机）。");
					}
				}

				if let Some(error) = &state.error {
					ui.add_space(6.0);
					ui.colored_label(ui.visuals().error_fg_color, error);
				}
			});

			ui.separator();
			ui.horizontal(|ui| {
				ui.spacing_mut().item_spacing.x = 2.0;
				if design::filled_button(ui, "保存").clicked() {
					match build_input(state) {
						Ok(input) => outcome = HostFormOutcome::Save(Box::new(input)),
						Err(error) => state.error = Some(error),
					}
				}
				if design::ghost_button(ui, "取消").clicked() {
					outcome = HostFormOutcome::Cancelled;
				}
			});
		},
	);

	// 窗口被右上角关掉同样算取消
	if !open {
		return HostFormOutcome::Cancelled;
	}
	outcome
}

/// 分组下拉。`None` 一项表示未分组。
fn group_picker(
	ui: &mut egui::Ui,
	id_salt: &str,
	label: &str,
	selected: Option<i64>,
	groups: &[Group],
	mut on_pick: impl FnMut(Option<i64>),
) {
	super::field_ui(ui, label, |ui| {
		let summary = group_summary(selected, groups);
		egui::ComboBox::from_id_salt(id_salt)
			.selected_text(summary)
			.show_ui(ui, |ui| {
				if ui.selectable_label(selected.is_none(), "未分组").clicked() {
					on_pick(None);
				}
				for group in groups {
					let is_selected = selected == group.id;
					if ui
						.selectable_label(is_selected, super::group_display_name(group, groups))
						.clicked()
					{
						on_pick(group.id);
					}
				}
			});
	});
}

/// 选中分组的展示文案。分组被删掉时给可读提示，而不是空白。
fn group_summary(selected: Option<i64>, groups: &[Group]) -> String {
	let Some(id) = selected else {
		return "未分组".to_string();
	};
	groups
		.iter()
		.find(|group| group.id == Some(id))
		.map_or_else(
			|| "（分组已删除）".to_string(),
			|group| super::group_display_name(group, groups),
		)
}

/// 代理下拉。`None` 一项表示直连。
fn proxy_picker(
	ui: &mut egui::Ui,
	id_salt: &str,
	label: &str,
	selected: Option<i64>,
	proxies: &[Proxy],
	mut on_pick: impl FnMut(Option<i64>),
) {
	super::field_ui(ui, label, |ui| {
		let summary = match selected {
			None => "无代理（直连）".to_string(),
			Some(id) => proxies
				.iter()
				.find(|proxy| proxy.id == Some(id))
				.map(|proxy| {
					format!(
						"{} ({} {})",
						proxy.name,
						proxy.kind,
						daisy_core::endpoint(&proxy.host, proxy.port)
					)
				})
				.unwrap_or_else(|| "（代理已删除）".to_string()),
		};

		egui::ComboBox::from_id_salt(id_salt)
			.selected_text(summary)
			.show_ui(ui, |ui| {
				if ui
					.selectable_label(selected.is_none(), "无代理（直连）")
					.clicked()
				{
					on_pick(None);
				}
				for proxy in proxies {
					let is_selected = selected == proxy.id;
					let text = format!(
						"{} ({} {})",
						proxy.name,
						proxy.kind,
						daisy_core::endpoint(&proxy.host, proxy.port)
					);
					if ui.selectable_label(is_selected, text).clicked() {
						on_pick(proxy.id);
					}
				}
			});
	});
}

/// 跳板机下拉。候选由调用方保证不含自己。
fn jump_picker(
	ui: &mut egui::Ui,
	id_salt: &str,
	label: &str,
	selected: Option<i64>,
	candidates: &[&Host],
	mut on_pick: impl FnMut(Option<i64>),
) {
	super::field_ui(ui, label, |ui| {
		let summary = match selected {
			None => "无跳板机".to_string(),
			Some(id) => candidates
				.iter()
				.find(|host| host.id == Some(id))
				.map(|host| {
					format!(
						"{} ({})",
						host.name,
						daisy_core::endpoint(&host.host, host.port)
					)
				})
				.unwrap_or_else(|| "（跳板机已删除）".to_string()),
		};

		egui::ComboBox::from_id_salt(id_salt)
			.selected_text(summary)
			.show_ui(ui, |ui| {
				if ui
					.selectable_label(selected.is_none(), "无跳板机")
					.clicked()
				{
					on_pick(None);
				}
				for host in candidates {
					let is_selected = selected == host.id;
					let text = format!(
						"{} ({})",
						host.name,
						daisy_core::endpoint(&host.host, host.port)
					);
					if ui.selectable_label(is_selected, text).clicked() {
						on_pick(host.id);
					}
				}
			});
	});
}

/// 库里那枚指纹是否仍然适用于表单当前填写的地址。
///
/// 用户改了地址 / 端口之后，旧指纹就不属于新目标了——继续显示会让人
/// 以为「新地址也是这个指纹」，而实际连接时会被判成首连。
fn cert_applies(cert_for: Option<&(String, u16)>, host: &str, port: u16) -> bool {
	matches!(cert_for, Some((recorded_host, recorded_port))
		if recorded_host == host && *recorded_port == port)
}

/// 把 `aa:bb:…` 按每行 8 个字节断开。
///
/// 直接交给 `ui.label` 会在任意位置折行——正好可能把 `aa:bb` 劈成 `a` / `a:bb`，
/// 看一眼还以为指纹本身不对。按字节边界断既整齐，也便于逐段核对。
fn fingerprint_lines(shown: &str) -> String {
	/// 每行显示几个字节（= 几个 `xx` 段）。
	const PER_LINE: usize = 8;
	let parts: Vec<&str> = shown.split(':').collect();
	if parts.len() <= PER_LINE {
		return shown.to_string();
	}
	parts
		.chunks(PER_LINE)
		.map(|chunk| chunk.join(":"))
		.collect::<Vec<_>>()
		.join("\n")
}

/// 证书指纹区块：显示已有指纹 + 「重新信任」。
///
/// **整块都走表单列**：标题左起（与标签列同一条竖线），正文与按钮落在控件列。
/// 早先这里是一个裸的 `ui.horizontal`，标题与说明直接顶到内容左边缘，
/// 比上下各行整整凸出一列——扫一眼就是「没对齐」。
fn cert_section(ui: &mut egui::Ui, state: &mut HostFormState) {
	let current = (state.host.host.trim().to_owned(), state.host.port);

	// 与设置页的分节同一套几何：上方留白 + 标题 + 分隔线。
	// 说明与标题同一行（小字弱色），不额外占一行。
	super::section_with_note(ui, "证书指纹", "首次连接时记录，之后指纹一变就拒绝连接");

	match (
		&state.known_cert,
		cert_applies(state.cert_for.as_ref(), &current.0, current.1),
	) {
		// 没有记录：说明还没连过，或刚被「重新信任」清掉
		(None, _) => {
			super::hint(ui, "尚未记录。首次连接成功后会自动记住服务端证书的指纹。");
		},
		// 地址改过：旧指纹不属于新目标
		(Some(_), false) => {
			super::hint(
				ui,
				"地址或端口已修改，旧指纹不再适用；保存后按新地址重新记录。",
			);
		},
		(Some(shown), true) => {
			// 可选中：核对指纹时多半要复制出去比
			super::field_ui(ui, "", |ui| {
				ui.add(
					egui::Label::new(
						egui::RichText::new(fingerprint_lines(shown))
							.monospace()
							.size(design::FONT_XS),
					)
					.selectable(true),
				);
			});
			super::field_ui(ui, "", |ui| {
				if ui
					.button("重新信任")
					.on_hover_text("删除这条记录。下次连接会重新当作首次连接并记录新指纹。")
					.clicked()
				{
					state.forget_cert_requested = true;
				}
			});
			super::hint(
				ui,
				"机器重装过、指纹确实变了时用。若没重装过却变了，先确认网络没有被劫持。",
			);
		},
	}
}

/// 收拢输入并做校验。错误文案直接给人看。
fn build_input(state: &HostFormState) -> Result<HostFormInput, String> {
	state.host.validate().map_err(|error| error.to_string())?;

	// 地址单独过一遍 HostAddr：模型层的 validate 只看非空，
	// 而「1.2.3.999」这类错要在这里就拦下，不等连接失败
	let addr = HostAddr::parse(&state.host.host).map_err(|error| error.to_string())?;

	let mut host = state.host.clone();
	host.host = addr.host().to_owned();
	host.username = if state.username.trim().is_empty() {
		None
	} else {
		Some(state.username.trim().to_owned())
	};

	// SSH 必须有用户名——模型的 validate 不管这事，留给这里
	if host.protocol == Protocol::Ssh && host.username.is_none() {
		return Err("SSH 需要填写登录用户名".to_string());
	}

	match &host.auth {
		AuthMethod::Password => {
			if state.password.is_empty() && state.is_new {
				return Err("请输入登录密码".to_string());
			}
		},
		AuthMethod::PublicKey(pk) => {
			if pk.has_passphrase && state.passphrase.is_empty() && state.is_new {
				return Err("私钥设置了口令，请输入口令".to_string());
			}
		},
	}

	Ok(HostFormInput {
		host,
		password: state.password.clone(),
		passphrase: state.passphrase.clone(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 整张表单只有**两条**竖线：标签共用一条，控件共用另一条。
	///
	/// 这条用例挡的是「标签列被内容收缩」——`allocate_ui_with_layout` 只把
	/// 请求尺寸当上限，实际按内容收缩，于是「名称」占 26、「剪贴板」占 39、
	/// 空标签占 0，控件左边缘在三处之间跳。每一行单看都正常，只有把十几行的
	/// 左边缘数值放在一起比才看得出来。
	#[cfg(unix)]
	#[test]
	fn every_form_row_shares_the_same_two_columns() {
		let ctx = egui::Context::default();
		// 密码框用的是 `fonts::PASSWORD_FAMILY` 这个自有字体族——不装字体的话
		// epaint 会直接 panic（族名找不到），和真实启动路径一样先装上
		crate::fonts::install_fonts(&ctx, None);
		let mut state = HostFormState::new_host();
		// RDP 才会出现的「剪贴板」那一行也要被覆盖到
		state.host.protocol = Protocol::Rdp;

		crate::views::reset_row_metrics();

		let input = egui::RawInput {
			screen_rect: Some(egui::Rect::from_min_size(
				egui::Pos2::ZERO,
				egui::vec2(900.0, 800.0),
			)),
			..Default::default()
		};
		let render_ctx = ctx.clone();
		let mut output = ctx.run_ui(input, |_ui| {
			let _ = super::ui(&render_ctx, &mut state, &[], &[], &[]);
		});
		output.textures_delta.clear();

		let rows = crate::views::row_metrics();
		assert!(
			rows.len() >= 10,
			"只记录到 {} 行：有表单行没走统一布局。{rows:?}",
			rows.len()
		);

		let (label_x, control_x) = rows[0];
		for (index, (label, control)) in rows.iter().enumerate() {
			assert!(
				(label - label_x).abs() < 0.01,
				"第 {index} 行的标签没落在同一条竖线上（{label} vs {label_x}）：{rows:?}"
			);
			assert!(
				(control - control_x).abs() < 0.01,
				"第 {index} 行的控件没落在同一条竖线上（{control} vs {control_x}）：{rows:?}"
			);
		}

		let expected = crate::views::LABEL_WIDTH + crate::views::FIELD_GAP;
		assert!(
			((control_x - label_x) - expected).abs() < 0.01,
			"标签列到控件列的距离应为 {expected}，实际 {}",
			control_x - label_x
		);
	}

	/// 测试连接的目标必须去掉 IPv6 的方括号。
	///
	/// `TcpStream::connect` 收的是裸地址：`[::1]` 会被当成域名解析，
	/// 测试必然失败而地址其实没错。
	#[test]
	fn probe_target_strips_ipv6_brackets() {
		let mut state = HostFormState::new_host();
		state.host.host = "[::1]".to_string();
		assert_eq!(state.probe_target().0, "::1");

		// 域名与 IPv4 原样
		state.host.host = " example.com ".to_string();
		assert_eq!(state.probe_target().0, "example.com");
	}

	#[test]
	fn cert_applies_only_to_the_recorded_endpoint() {
		let recorded = ("10.0.0.9".to_string(), 3389u16);
		assert!(cert_applies(Some(&recorded), "10.0.0.9", 3389));
		// 改了地址或端口，旧指纹就不属于这个目标
		assert!(!cert_applies(Some(&recorded), "10.0.0.10", 3389));
		assert!(!cert_applies(Some(&recorded), "10.0.0.9", 13389));
		// 没有记录时什么都对不上
		assert!(!cert_applies(None, "10.0.0.9", 3389));
	}

	#[test]
	fn forgetting_clears_the_shown_fingerprint_once() {
		let host = Host::draft(Protocol::Rdp);
		let mut state = HostFormState::edit(&host);
		state.set_known_cert(Some("AA:BB:CC".to_string()), "10.0.0.9", 3389);

		// 没点按钮时不该有任何请求
		assert_eq!(state.take_forget_cert_request(), None);

		state.forget_cert_requested = true;
		assert_eq!(
			state.take_forget_cert_request(),
			Some(("10.0.0.9".to_string(), 3389)),
			"应返回库里那条记录的键"
		);
		assert!(
			state.known_cert.is_none(),
			"点下去就该先从界面抹掉，等库确认"
		);
		assert_eq!(
			state.take_forget_cert_request(),
			None,
			"同一次点击只能消费一次，否则会反复删库"
		);
	}

	#[test]
	fn fingerprint_breaks_on_byte_boundaries() {
		// 32 个字节 → 每行 8 个，共 4 行，且每行都以完整的 `xx:xx` 收尾
		let shown = (0..32u8)
			.map(|byte| format!("{byte:02x}"))
			.collect::<Vec<_>>()
			.join(":");
		let lines = fingerprint_lines(&shown);
		let lines: Vec<&str> = lines.split('\n').collect();
		assert_eq!(lines.len(), 4, "32 字节应断成 4 行：{lines:?}");
		for line in &lines {
			let parts: Vec<&str> = line.split(':').collect();
			assert_eq!(parts.len(), 8, "每行必须是 8 个完整字节：{line}");
			assert!(
				parts.iter().all(|part| part.len() == 2),
				"不该把字节劈开：{line}"
			);
		}

		// 短指纹（比如测试里用的 4 字节）原样返回，不硬断
		assert_eq!(fingerprint_lines("aa:bb:cc:dd"), "aa:bb:cc:dd");
	}
}
