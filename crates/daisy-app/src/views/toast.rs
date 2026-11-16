//! 全局 toast：右下角浮层提示，自动过期并淡出。
//!
//! 取代原先「状态栏右侧一行文本」的做法——状态栏那格会被长文案挤掉，
//! 也看不清「刚刚发生了什么」。浮层可堆叠、错误有独立配色、
//! 过期后自动淡出消失，不需要调用方关心生命周期。

use std::time::{Duration, Instant};

/// 提示停留时长。
pub const TOAST_TTL: Duration = Duration::from_secs(6);

/// 停留结束后再淡出多久才移除。
pub const TOAST_FADE: Duration = Duration::from_millis(400);

/// 提示级别。决定配色——错误要比普通信息更显眼。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
	/// 普通信息（成功、状态变更）。
	Info,
	/// 出错。
	Error,
}

/// 一条提示。
struct Entry {
	/// 文案。
	message: String,
	/// 级别。
	kind: ToastKind,
	/// 开始淡出的时刻。
	until: Instant,
}

/// 提示队列。应用层只管往里塞，生命周期由这里管理。
#[derive(Default)]
pub struct ToastState {
	entries: Vec<Entry>,
}

impl ToastState {
	/// 弹一条普通提示。
	pub fn info(&mut self, message: impl Into<String>) {
		self.push(message.into(), ToastKind::Info);
	}

	/// 弹一条错误提示。
	pub fn error(&mut self, message: impl Into<String>) {
		self.push(message.into(), ToastKind::Error);
	}

	/// 是否还有提示在显示（含淡出阶段）。
	///
	/// 调用方据此决定要不要续帧——事件循环睡着的话淡出会卡住。
	pub fn is_active(&self) -> bool {
		!self.entries.is_empty()
	}

	fn push(&mut self, message: String, kind: ToastKind) {
		self.entries.push(Entry {
			message,
			kind,
			until: Instant::now() + TOAST_TTL,
		});
	}

	/// 画浮层。过期的条目在这里顺手清掉。
	pub fn ui(&mut self, ctx: &egui::Context) {
		let now = Instant::now();
		self.entries.retain(|entry| !is_expired(entry.until, now));

		if self.entries.is_empty() {
			return;
		}

		egui::Area::new(egui::Id::new("daisy_toast"))
			// 避开底部状态栏
			.anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -56.0))
			.order(egui::Order::Foreground)
			// 提示只该被看，不该抢点击——下面可能是终端
			.interactable(false)
			.show(ctx, |ui| {
				ui.set_max_width(360.0);
				for entry in &self.entries {
					paint_entry(ui, entry, fade_alpha(entry.until, now));
					ui.add_space(6.0);
				}
			});
	}
}

/// 条目是否已完全过期（淡出结束）。
fn is_expired(until: Instant, now: Instant) -> bool {
	now >= until + TOAST_FADE
}

/// 当前不透明度：停留期内恒为 1，之后线性淡出到 0。
fn fade_alpha(until: Instant, now: Instant) -> f32 {
	if now <= until {
		return 1.0;
	}
	let elapsed = now.duration_since(until).as_secs_f32();
	(1.0 - elapsed / TOAST_FADE.as_secs_f32()).clamp(0.0, 1.0)
}

/// 画一条提示的卡片。
///
/// 错误用红色文字 + 红色描边，背景仍用窗口底色——这样深浅两套主题
/// 下都不必单独调对比度。
fn paint_entry(ui: &mut egui::Ui, entry: &Entry, alpha: f32) {
	let visuals = ui.visuals();
	let (stroke_color, text_color) = match entry.kind {
		ToastKind::Info => (visuals.window_stroke.color, visuals.text_color()),
		ToastKind::Error => (visuals.error_fg_color, visuals.error_fg_color),
	};

	egui::Frame::popup(ui.style())
		.fill(visuals.window_fill.gamma_multiply(alpha))
		.stroke(egui::Stroke::new(1.0, stroke_color.gamma_multiply(alpha)))
		.show(ui, |ui| {
			ui.set_max_width(340.0);
			ui.colored_label(text_color.gamma_multiply(alpha), &entry.message);
		});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn alpha_is_opaque_while_held() {
		let now = Instant::now();
		let until = now + Duration::from_secs(3);
		assert_eq!(fade_alpha(until, now), 1.0);
	}

	#[test]
	fn alpha_is_opaque_at_the_boundary() {
		let now = Instant::now();
		assert_eq!(fade_alpha(now, now), 1.0, "刚到点时还应该是全不透明");
	}

	#[test]
	fn alpha_fades_linearly() {
		let now = Instant::now();
		let until = now
			.checked_sub(Duration::from_millis(200))
			.expect("时间可回溯");
		// 淡出 400ms 走了 200ms → 剩一半
		assert!((fade_alpha(until, now) - 0.5).abs() < 0.01);
	}

	#[test]
	fn alpha_reaches_zero_after_fade() {
		let now = Instant::now();
		let until = now
			.checked_sub(TOAST_FADE + Duration::from_millis(1))
			.expect("时间可回溯");
		assert_eq!(fade_alpha(until, now), 0.0);
	}

	#[test]
	fn entry_expires_only_after_fade() {
		let now = Instant::now();
		let until = now + TOAST_TTL;
		assert!(!is_expired(until, now), "停留期内不该过期");
		assert!(
			!is_expired(until, until + Duration::from_millis(100)),
			"淡出中不该移除"
		);
		assert!(
			is_expired(until, until + TOAST_FADE + Duration::from_millis(1)),
			"淡出结束应移除"
		);
	}

	#[test]
	fn state_reports_activity() {
		let mut state = ToastState::default();
		assert!(!state.is_active());
		state.info("已保存");
		assert!(state.is_active());
	}
}
