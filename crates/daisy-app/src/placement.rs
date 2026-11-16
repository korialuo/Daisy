//! 窗口落位：保证启动后窗口出现在**可见**显示器内。
//!
//! 背景：`ViewportBuilder` 不指定位置时由系统摆放。macOS 多屏 +
//! 副屏未激活的实测场景里，窗口被放到副屏坐标区间（X=2290，外接屏
//! 1920–3840），"上屏窗口"列表里根本没有它——用户看不到窗口，也
//! 点不到，只能杀进程。
//!
//! 策略：首帧检查一次窗口当前位置；只要落在任一显示器内就不动
//! （尊重用户摆放），全都不在才挪到主显示器居中。挪动走
//! `ViewportCommand::OuterPosition`，egui-winit 会按
//! `zoom_factor * scale_factor` 换算成物理像素——所以这里产出的是
//! **逻辑点**（物理 / scale_factor）。

use egui::Pos2;

/// 一个显示器的矩形（全局坐标，物理像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MonitorRect {
	/// 左上角 X（全局）。
	pub x: i32,
	/// 左上角 Y（全局）。
	pub y: i32,
	/// 宽（物理像素）。
	pub w: u32,
	/// 高（物理像素）。
	pub h: u32,
}

impl MonitorRect {
	/// 点是否落在矩形内。左上闭、右下开——窗口左上角在显示器内即算可见。
	///
	/// 用 i64 做加法：`x + w` 理论上能溢出 i32（虽然现实显示器到不了）。
	pub(crate) fn contains(&self, x: i32, y: i32) -> bool {
		(x as i64) >= self.x as i64
			&& (y as i64) >= self.y as i64
			&& (x as i64) < self.x as i64 + self.w as i64
			&& (y as i64) < self.y as i64 + self.h as i64
	}

	/// 给定窗口尺寸（逻辑点），返回窗口在该显示器上的居中位置（逻辑点）。
	///
	/// 显示器原点也要一起换算——多屏下非主屏的原点不是 (0,0)。
	/// 窗口比显示器还大时 clamp 到显示器原点，不产出负坐标。
	pub(crate) fn centered(&self, scale_factor: f64, window: [f32; 2]) -> Pos2 {
		let logical_w = self.w as f64 / scale_factor;
		let logical_h = self.h as f64 / scale_factor;
		let x = ((logical_w - f64::from(window[0])) / 2.0).max(0.0);
		let y = ((logical_h - f64::from(window[1])) / 2.0).max(0.0);
		// Pos2 是 f32：居中坐标都是显示器级像素，精度无损
		Pos2::new(
			(self.x as f64 / scale_factor + x) as f32,
			(self.y as f64 / scale_factor + y) as f32,
		)
	}
}

/// 首帧落位检查：窗口不在任何可见显示器内就移到主屏居中。
///
/// 只应调用一次——重复调用会把用户手动摆好的位置也强行居中。
/// 拿不到 winit 窗口（headless 测试）或平台不支持位置查询时静默跳过。
pub(crate) fn ensure_window_visible(
	frame: &eframe::Frame,
	ctx: &egui::Context,
	window_size: [f32; 2],
) {
	let Some(window) = frame.winit_window() else {
		return;
	};
	// Wayland 等平台拿不到位置—— compositor 自己摆放，不存在我们的问题
	let Ok(position) = window.outer_position() else {
		return;
	};

	let monitors: Vec<MonitorRect> = window
		.available_monitors()
		.map(|monitor| MonitorRect {
			x: monitor.position().x,
			y: monitor.position().y,
			w: monitor.size().width,
			h: monitor.size().height,
		})
		.collect();

	if monitors
		.iter()
		.any(|rect| rect.contains(position.x, position.y))
	{
		return; // 已在可见区域
	}

	// 首选主显示器；拿不到（Wayland）就退到窗口当前所在屏
	let Some(monitor) = window
		.primary_monitor()
		.or_else(|| window.current_monitor())
	else {
		return;
	};
	let scale_factor = monitor.scale_factor();
	let rect = MonitorRect {
		x: monitor.position().x,
		y: monitor.position().y,
		w: monitor.size().width,
		h: monitor.size().height,
	};
	let target = rect.centered(scale_factor, window_size);
	tracing::info!(
		"窗口位置 ({}, {}) 不在任何可见显示器内，移至 ({:.0}, {:.0})",
		position.x,
		position.y,
		target.x,
		target.y
	);
	ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(target));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn contains_inside() {
		let rect = MonitorRect {
			x: 0,
			y: 0,
			w: 1920,
			h: 1080,
		};
		assert!(rect.contains(0, 0));
		assert!(rect.contains(960, 540));
		assert!(rect.contains(1919, 1079));
	}

	#[test]
	fn contains_outside_and_edges() {
		let rect = MonitorRect {
			x: 0,
			y: 0,
			w: 1920,
			h: 1080,
		};
		assert!(!rect.contains(1920, 0)); // 右下边界外
		assert!(!rect.contains(0, 1080));
		assert!(!rect.contains(-1, 0));
		assert!(!rect.contains(0, -1));
	}

	#[test]
	fn contains_offscreen_secondary() {
		// 复现实测 bug：副屏区间 1920..3840 未激活，窗口被放到 (2290, 125)
		let primary = MonitorRect {
			x: 0,
			y: 0,
			w: 1920,
			h: 1200,
		};
		let secondary = MonitorRect {
			x: 1920,
			y: 0,
			w: 1920,
			h: 1200,
		};
		let spot = (2290, 125);
		// 副屏在 available_monitors 里时判定为可见——不打扰用户
		assert!(secondary.contains(spot.0, spot.1));
		// 只有主屏时（副屏休眠被系统移出枚举）才判定离屏
		assert!(!primary.contains(spot.0, spot.1));
	}

	#[test]
	fn centered_on_primary() {
		let rect = MonitorRect {
			x: 0,
			y: 0,
			w: 1920,
			h: 1080,
		};
		let pos = rect.centered(1.0, [1180.0, 760.0]);
		assert_eq!(pos, Pos2::new(370.0, 160.0));
	}

	#[test]
	fn centered_offsets_from_monitor_origin() {
		// 副屏原点不是 (0,0)：居中点是 原点 + (显示器内偏移)
		let rect = MonitorRect {
			x: 1920,
			y: 0,
			w: 1920,
			h: 1080,
		};
		let pos = rect.centered(1.0, [1180.0, 760.0]);
		assert_eq!(pos, Pos2::new(2290.0, 160.0));
	}

	#[test]
	fn centered_clamps_when_window_larger_than_monitor() {
		let rect = MonitorRect {
			x: 0,
			y: 0,
			w: 800,
			h: 600,
		};
		let pos = rect.centered(1.0, [1180.0, 760.0]);
		assert_eq!(pos, Pos2::new(0.0, 0.0)); // 不产出负坐标
	}

	#[test]
	fn centered_converts_retina_scale() {
		// Retina：3840×2160 物理 / scale 2 = 1920×1080 逻辑
		let rect = MonitorRect {
			x: 0,
			y: 0,
			w: 3840,
			h: 2160,
		};
		let pos = rect.centered(2.0, [1180.0, 760.0]);
		assert_eq!(pos, Pos2::new(370.0, 160.0));
	}
}
