//! S8 步骤 62 验收：向真实 RDP 主机注入键鼠输入，抓注入前后的画面比对。
//!
//! `capture.rs` 验证「能连上并出图」，本工具验证**输入真的到了远端**——
//! 做法是发一组**结果可辨认**的输入，再抓画面：
//!
//! ```bash
//! DAISY_RDP_PASSWORD='<密码>' \
//!   cargo run -p daisy-rdp --example input_probe 192.168.10.20 /tmp/probe keys
//! ```
//!
//! 动作（第三个参数）：
//!
//! | 动作 | 含义 | 为什么能作证 |
//! | --- | --- | --- |
//! | `keys` | 发 `Win+R` | 「运行」对话框只有收到 Win 键（扩展键）+ R 才出现，一张图证明修饰键、扩展位、字母三者都对 |
//! | `move:<x>,<y>` | 只把指针移到绝对坐标，不点 | 看画面里有没有多出 Windows 光标 |
//! | `click:<x>,<y>` | 在绝对坐标点一下左键 | 靶子自选——点「取消」按钮能验证坐标换算精确 |
//! | `drag:<x1>,<y1>,<x2>,<y2>` | 按下 → 移动 → 抬起 | 验证按下 / 移动 / 抬起整条链路 |
//! | `keys+click:<x>,<y>` | 先 `Win+R`，抓一张，再点击，再抓一张 | 一次跑完键鼠两项 |
//!
//! 输出 `<前缀>-before.rgba`（必要时还有 `-mid.rgba` 与 `-after.rgba`），
//! 紧凑 RGBA，尺寸打到标准输出。转 PNG 用外部工具即可——不在这里再抄一份
//! `capture.rs` 的 PNG 编码器。
//!
//! 密码只从环境变量读。

use std::time::{Duration, Instant};

use daisy_rdp::{ConnectionParams, InputEvent, Key, PointerButton, RdpClient, to_rgba};

/// 注入之后等画面稳定的时长。
///
/// 「运行」对话框与开始菜单都有展开动画，等太短会抓到一半的状态。
const AFTER_SETTLE_MS: u64 = 1500;

fn env_required(key: &str) -> String {
	std::env::var(key).unwrap_or_else(|_| {
		eprintln!("缺少环境变量 {key}");
		std::process::exit(2);
	})
}

fn main() {
	let args: Vec<String> = std::env::args().collect();
	let host = args.get(1).map(String::as_str).unwrap_or("192.168.10.20");
	let prefix = args.get(2).map(String::as_str).unwrap_or("/tmp/rdp_probe");
	let phase = args.get(3).map(String::as_str).unwrap_or("keys");

	let password = env_required("DAISY_RDP_PASSWORD");
	let settle_ms: u64 = std::env::var("DAISY_RDP_SETTLE_MS")
		.ok()
		.and_then(|value| value.parse().ok())
		.unwrap_or(3000);

	let params = ConnectionParams::new(host, "Administrator", &password).with_resolution(1024, 768);
	println!("目标 {host}，分辨率 1024x768，动作「{phase}」");

	let mut client = match RdpClient::new() {
		Ok(client) => client,
		Err(error) => {
			eprintln!("创建实例失败：{error}");
			std::process::exit(1);
		},
	};
	// 启用剪贴板通道（必须在 connect 之前）
	client.enable_clipboard();
	if let Err(error) = client.connect(&params) {
		eprintln!("连接失败：{error}");
		std::process::exit(1);
	}

	// 等首帧 + 让桌面画完（首帧常常是全黑）
	pump_until_frame(&client, settle_ms);
	pump_for(&client, Duration::from_millis(settle_ms));
	// 光标走独立消息（不合成进画面帧），连上后单独问一次
	report_cursor(&client);
	dump_latest(&client, &format!("{prefix}-before.rgba"));

	// 先按下 Win+R 的动作统一在这里发；`keys` 与 `keys+click` 都走它
	if phase.starts_with("keys") {
		run(&client, &win_r());
		pump_for(&client, Duration::from_millis(AFTER_SETTLE_MS));
	}

	if phase == "keys" {
		dump_latest(&client, &format!("{prefix}-after.rgba"));
		return;
	}

	// `keys+click` 的中间态单独留一张——它是「键盘已生效」的证据
	if let Some(rest) = phase.strip_prefix("keys+") {
		dump_latest(&client, &format!("{prefix}-mid.rgba"));
		run_parsed(&client, rest);
	} else {
		run_parsed(&client, phase);
	}

	pump_for(&client, Duration::from_millis(AFTER_SETTLE_MS));
	dump_latest(&client, &format!("{prefix}-after.rgba"));

	// 光标走独立消息，最后再看一次
	report_cursor(&client);
}

/// 报告并 ASCII 渲染收到的光标位图。
///
/// 渲染出来是为了**肉眼确认形状**：光标很小，打一行「收到 32x32」看不出
/// 是箭头还是方块，画成字符画一眼就知道解码对不对。
fn report_cursor(client: &RdpClient) {
	let Some((seq, sprite)) = client.latest_cursor() else {
		println!("未收到光标位图");
		return;
	};
	println!(
		"收到光标（#{seq}）：{}x{} 热点({}, {}) RGBA {} 字节",
		sprite.width,
		sprite.height,
		sprite.hot_x,
		sprite.hot_y,
		sprite.pixels.len()
	);

	// 顺手把原始 RGBA 落盘，好在外面转成 PNG 用眼睛看
	if let Ok(dir) = std::env::var("DAISY_CURSOR_OUT") {
		let path = format!("{dir}/cursor-{seq}.rgba");
		if let Err(error) = std::fs::write(&path, &sprite.pixels) {
			eprintln!("写出 {path} 失败：{error}");
		} else {
			println!(
				"光标像素已写出 {path}（{}x{}）",
				sprite.width, sprite.height
			);
		}
	}

	let width = sprite.width as usize;
	let height = sprite.height as usize;
	let step = width.div_ceil(32).max(1);
	for y in 0..height.min(24) {
		let mut line = String::new();
		for x in (0..width).step_by(step) {
			let mut sum = 0u32;
			let mut count = 0u32;
			for yy in y..(y + 1).min(height) {
				for xx in x..(x + step).min(width) {
					let offset = (yy * width + xx) * 4;
					if sprite.pixels[offset + 3] < 16 {
						continue;
					}
					let lum = sprite.pixels[offset..offset + 3]
						.iter()
						.copied()
						.map(u32::from)
						.sum::<u32>() / 3;
					sum += lum;
					count += 1;
				}
			}
			line.push(if count == 0 {
				' '
			} else {
				match sum.checked_div(count).unwrap_or(0) {
					0..=63 => '#',
					64..=127 => '+',
					128..=191 => '.',
					_ => ',',
				}
			});
		}
		println!("  |{line}|");
	}
}

/// 解析 `click:<x>,<y>` / `drag:<x1>,<y1>,<x2>,<y2>` 并注入。
fn run_parsed(client: &RdpClient, spec: &str) {
	let (kind, rest) = spec
		.split_once(':')
		.unwrap_or_else(|| fail(&format!("动作「{spec}」缺少坐标，形如 click:300,400")));

	let numbers: Vec<u16> = rest
		.split(',')
		.map(|piece| {
			piece
				.trim()
				.parse()
				.unwrap_or_else(|_| fail(&format!("坐标「{piece}」不是 0..65535 的整数")))
		})
		.collect();

	let point = |index: usize| -> (u16, u16) {
		let (x, y) = (numbers.get(index * 2), numbers.get(index * 2 + 1));
		match (x, y) {
			(Some(&x), Some(&y)) => (x, y),
			_ => fail(&format!("动作「{spec}」的点 {index} 坐标不全")),
		}
	};

	let events = match kind {
		// 只挪不点：用来验证「服务端是否把 Windows 光标合成进了画面」——
		// 若合成，指针移动后画面上会多出一个箭头形状
		"move" => {
			let (x, y) = point(0);
			vec![InputEvent::PointerMove { x, y }]
		},
		"click" => {
			let (x, y) = point(0);
			vec![
				InputEvent::PointerMove { x, y },
				button(PointerButton::Left, true, x, y),
				button(PointerButton::Left, false, x, y),
			]
		},
		"drag" => {
			let (x1, y1) = point(0);
			let (x2, y2) = point(1);
			vec![
				InputEvent::PointerMove { x: x1, y: y1 },
				button(PointerButton::Left, true, x1, y1),
				// 中间补几个点：真实拖动也是连续的，一步到位有些窗口认不出来
				InputEvent::PointerMove {
					x: x1 + (x2 - x1) / 2,
					y: y1 + (y2 - y1) / 2,
				},
				InputEvent::PointerMove { x: x2, y: y2 },
				button(PointerButton::Left, false, x2, y2),
			]
		},
		other => fail(&format!("未知动作「{other}」，可用 move / click / drag")),
	};

	run(client, &events);
}

fn button(button: PointerButton, down: bool, x: u16, y: u16) -> InputEvent {
	InputEvent::PointerButton { button, down, x, y }
}

/// `Win+R`：Win 是扩展键（`0x5B | 0x100`），扩展位写错这里就出不来对话框。
fn win_r() -> Vec<InputEvent> {
	let press = |key, down| InputEvent::Key {
		key,
		down,
		repeat: false,
	};
	vec![
		press(Key::SuperLeft, true),
		press(Key::R, true),
		press(Key::R, false),
		press(Key::SuperLeft, false),
	]
}

fn run(client: &RdpClient, events: &[InputEvent]) {
	println!("注入 {} 条输入事件", events.len());
	for &event in events {
		if let Err(error) = client.send_input(event) {
			eprintln!("注入失败：{error}");
			std::process::exit(1);
		}
	}
}

fn fail(message: &str) -> ! {
	eprintln!("{message}");
	std::process::exit(2);
}

/// 泵事件直到拿到一帧或超时。
fn pump_until_frame(client: &RdpClient, timeout_ms: u64) {
	let deadline = Instant::now() + Duration::from_millis(timeout_ms);
	while Instant::now() < deadline {
		if client.latest_frame().is_some() {
			return;
		}
		if client.is_disconnected() {
			eprintln!("会话提前断开");
			std::process::exit(1);
		}
		let _ = client.pump(200);
	}
}

/// 实打实地泵够一段时间——注入后的画面更新要靠它才收得到。
fn pump_for(client: &RdpClient, duration: Duration) {
	let deadline = Instant::now() + duration;
	while Instant::now() < deadline && !client.is_disconnected() {
		let _ = client.pump(80);
	}
}

/// 写出最新一帧的紧凑 RGBA，并把尺寸打到标准输出。
fn dump_latest(client: &RdpClient, path: &str) {
	let Some((seq, frame)) = client.latest_frame() else {
		eprintln!("拿不到帧，无法写出 {path}");
		std::process::exit(1);
	};
	let Some(rgba) = to_rgba(&frame) else {
		eprintln!("这一帧不完整（缓冲还在重建），无法写出 {path}");
		std::process::exit(1);
	};
	println!("第 {seq} 帧 {}x{} → {path}", rgba.width, rgba.height);
	if let Err(error) = std::fs::write(path, &rgba.pixels) {
		eprintln!("写出 {path} 失败：{error}");
		std::process::exit(1);
	}
}
