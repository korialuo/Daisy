//! 设置窗口。
//!
//! 窗口只改 [`AppConfig`] 的值，**不自己落盘也不自己生效**——
//! 应用层每帧比对配置是否变化，变了就保存 + 应用。这样「改动即时生效」
//! 只有一条路径，不会出现窗口里改了、运行时没跟上的分叉。

use std::path::Path;

use daisy_core::config::{AppConfig, Theme};

/// 设置页控件列的宽度。
///
/// 下拉框按内容自适应，会出现 60 / 90 / 80 三种宽度，一列看过去参差不齐；
/// 统一宽度之后右边缘对齐，顺带把「数字框贴着左边、右边空一大片」也治了。
const CONTROL_WIDTH: f32 = 240.0;

/// 终端字体下拉里「不指定」那一项的文案。
const DEFAULT_FONT_LABEL: &str = "默认（Hack）";

/// 字体下拉弹层的高度（逻辑点）。
///
/// 本机三百多个字体，列表必须能滚；弹层高度定死，滚动条才落在弹层里而不是
/// 把整个窗口顶长。
const FONT_MENU_HEIGHT: f32 = 320.0;

/// 字体下拉里的一行：名字 + 「等宽 / 非等宽」徽标，**整行都能点**。
///
/// 不用 `selectable_value`：它的响应只包住文字，点徽标或行尾没反应；而且
/// 徽标得跟名字排在一行，纯文本的选中项塞不进第二段样式。
fn font_row(ui: &mut egui::Ui, label: &str, monospaced: bool, selected: bool) -> bool {
	let tokens = crate::design::tokens(ui.ctx());
	let height = 22.0;
	// 下拉里的 `ScrollArea` 在某些布局下会把可用宽度报成无穷大，
	// 直接拿去 allocate 会让弹层宽度炸开
	let width = ui.available_width().clamp(120.0, 640.0);
	let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());

	if selected {
		ui.painter().rect_filled(rect, 4, tokens.bg_active);
	} else if response.hovered() {
		ui.painter().rect_filled(rect, 4, tokens.bg_hover);
	}

	let color = if selected {
		tokens.text
	} else {
		tokens.text_dim
	};
	let name_left = rect.left() + 8.0;
	let galley =
		ui.painter()
			.layout_no_wrap(label.to_owned(), egui::FontId::proportional(13.0), color);
	ui.painter().galley(
		egui::pos2(name_left, rect.center().y - galley.size().y * 0.5),
		galley.clone(),
		color,
	);

	// 与主机列表的 SSH / RDP 徽标同一套度量：等宽用成功色（和 SSH 一个色系），
	// 非等宽用弱色——列表里绝大多数都是它，染成告警色会吵得看不出重点
	let (fg, bg, tag) = if monospaced {
		(tokens.success_text, tokens.success_fill, "等宽")
	} else {
		(tokens.text_muted, tokens.bg_raised, "非等宽")
	};
	super::paint_badge(
		ui.painter(),
		name_left + galley.size().x + 6.0,
		rect.center().y,
		tag,
		fg,
		bg,
	);

	// 名字长了会在弹层边缘被裁：悬停给全名
	response.on_hover_text(label).clicked()
}

/// 设置窗口本帧的结局。
pub enum SettingsOutcome {
	/// 仍在编辑。
	Open,
	/// 用户关闭了窗口。
	Closed,
	/// 请求在系统文件管理器里打开日志目录。
	///
	/// 做成动作而不是直接 `Command::spawn`：视图不碰外部程序，
	/// 失败提示也归应用层统一弹。
	OpenLogDir,
}

/// 画设置窗口。
pub fn ui(ctx: &egui::Context, config: &mut AppConfig, log_dir: &Path) -> SettingsOutcome {
	let mut open = true;
	let mut open_log_dir = false;
	super::dialog(ctx, "设置", [420.0, 745.0], &mut open, |ui| {
		egui::ScrollArea::vertical().show(ui, |ui| {
			ui.spacing_mut().item_spacing.y = super::ROW_GAP;
			super::section(ui, "外观");

			super::field_ui(ui, "主题", |ui| {
				egui::ComboBox::from_id_salt("theme")
					.width(CONTROL_WIDTH)
					.selected_text(theme_label(config.theme))
					.show_ui(ui, |ui| {
						ui.selectable_value(
							&mut config.theme,
							Theme::System,
							theme_label(Theme::System),
						);
						ui.selectable_value(
							&mut config.theme,
							Theme::Light,
							theme_label(Theme::Light),
						);
						ui.selectable_value(
							&mut config.theme,
							Theme::Dark,
							theme_label(Theme::Dark),
						);
					});
			});
			super::hint(ui, "「跟随系统」在每次应用设置时重新探测。");

			super::field(ui, "界面缩放", |ui| {
				super::number_row(
					ui,
					CONTROL_WIDTH,
					egui::DragValue::new(&mut config.ui_scale)
						.range(0.5..=3.0)
						.speed(0.01)
						.fixed_decimals(2),
				)
			});
			super::hint(ui, "相对系统 DPI 的倍率，1.00 为默认。");

			super::section(ui, "终端");

			super::field_ui(ui, "字体", |ui| {
				let selected = config
					.terminal
					.font_family
					.clone()
					.unwrap_or_else(|| DEFAULT_FONT_LABEL.to_string());

				egui::ComboBox::from_id_salt("terminal_font")
					.width(CONTROL_WIDTH)
					// 高度交给 ComboBox 自己：在 `show_ui` 里 `set_max_height`
					// 会跟它的滚动区打架，行被压扁、文字叠在一起
					.height(FONT_MENU_HEIGHT)
					.selected_text(selected)
					.show_ui(ui, |ui| {
						ui.set_min_width(CONTROL_WIDTH);
						ui.spacing_mut().item_spacing.y = 2.0;

						if font_row(ui, DEFAULT_FONT_LABEL, true, config.terminal.font_family.is_none())
						{
							config.terminal.font_family = None;
							ui.close();
						}
						for font in crate::font_catalog::all() {
							let selected =
								config.terminal.font_family.as_deref() == Some(font.family.as_str());
							if font_row(ui, &font.family, font.monospaced, selected) {
								config.terminal.font_family = Some(font.family.clone());
								ui.close();
							}
						}
					});
			});
			super::hint(ui, "本机已安装的字体，等宽在前；对所有终端生效。");

			super::field(ui, "字号", |ui| {
				super::number_row(
					ui,
					CONTROL_WIDTH,
					egui::DragValue::new(&mut config.terminal.font_size)
						.range(6.0..=48.0)
						.speed(0.1)
						.fixed_decimals(1),
				)
			});

			super::field(ui, "回滚行数", |ui| {
				super::number_row(
					ui,
					CONTROL_WIDTH,
					egui::DragValue::new(&mut config.terminal.scrollback)
						.range(100..=1_000_000)
						.speed(50),
				)
			});

			super::field_ui(ui, "光标", |ui| {
				egui::ComboBox::from_id_salt("cursor_style")
					.width(CONTROL_WIDTH)
					.selected_text(config.terminal.cursor_style.clone())
					.show_ui(ui, |ui| {
						for style in ["block", "bar", "underline"] {
							ui.selectable_value(
								&mut config.terminal.cursor_style,
								style.to_string(),
								style,
							);
						}
					});
			});

			super::section(ui, "日志");

			super::field_ui(ui, "级别", |ui| {
				egui::ComboBox::from_id_salt("log_level")
					.width(CONTROL_WIDTH)
					.selected_text(config.log_level.clone())
					.show_ui(ui, |ui| {
						for level in ["trace", "debug", "info", "warn", "error"] {
							ui.selectable_value(&mut config.log_level, level.to_string(), level);
						}
					});
			});
			super::hint(ui, "改动立即生效，并写入 config.toml。");

			super::field_ui(ui, "目录", |ui| {
				if ui.button("打开").clicked() {
					open_log_dir = true;
				}
			});
			super::hint(ui, "每日滚动，保留最近 7 天。");
			super::hint(ui, &log_dir.display().to_string());
		});
	});

	if open_log_dir {
		return SettingsOutcome::OpenLogDir;
	}
	if open {
		SettingsOutcome::Open
	} else {
		SettingsOutcome::Closed
	}
}

/// 主题偏好展示名。
fn theme_label(theme: Theme) -> &'static str {
	match theme {
		Theme::System => "跟随系统",
		Theme::Light => "浅色",
		Theme::Dark => "深色",
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 字体列表里的行不许被压扁或互相叠上。
	///
	/// 起因：曾经在 `ComboBox::show_ui` 里调 `ui.set_max_height(360)`，把
	/// 下拉内部 `ScrollArea` 的高度预算搞乱，最后几行挤在一起、文字压在下一行上。
	/// 现在高度只交给 `ComboBox::height()`，这个用例守住「行与行不重叠」。
	#[test]
	fn font_rows_keep_their_height() {
		let ctx = egui::Context::default();
		crate::fonts::install_fonts(&ctx, None);

		let mut rows: Vec<(f32, f32)> = Vec::new();
		let mut output = ctx.run_ui(
			egui::RawInput {
				screen_rect: Some(egui::Rect::from_min_size(
					egui::Pos2::ZERO,
					egui::vec2(240.0, 400.0),
				)),
				..Default::default()
			},
			|ui| {
				ui.spacing_mut().item_spacing.y = 2.0;
				for index in 0..6 {
					let before = ui.cursor().min.y;
					let clicked = font_row(ui, "Menlo", index % 2 == 0, index == 0);
					rows.push((before, ui.cursor().min.y));
					// 没点击就不该返回 true（否则下拉会莫名其妙自己关掉）
					assert!(!clicked);
				}
			},
		);
		output.textures_delta.clear();

		for (before, after) in &rows {
			assert!(after - before >= 21.5, "行高被压扁了：{before} → {after}");
		}
		for pair in rows.windows(2) {
			assert!(
				pair[1].0 >= pair[0].1 - 0.01,
				"下一行压在了上一行上：{:?} 之后接 {:?}",
				pair[0],
				pair[1]
			);
		}
	}
}
