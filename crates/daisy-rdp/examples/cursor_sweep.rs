//! 抓取远端**每一种**光标形状的原始掩码。
//!
//! 用途：光标解码的断言必须建立在**真机数据**上。合成数据只能证明
//! 「我和我一致」（两边犯同一个错），证明不了「服务端到底给什么」。
//!
//! 做法：连上目标 → 用 `Win+R` 起一个文本编辑区（记事本）→ 让指针
//! 在文本区、窗口边缘、标题栏等位置各停一会儿→ 每收到一种新形状
//! 就把原始 XOR/AND 掩码落盘。`cursor.rs` 里的
//! `DAISY_CURSOR_RAW` 钩子负责真正写文件，这里只负责把指针摆到位。
//!
//! ```sh
//! DAISY_CURSOR_RAW=/tmp/raw DAISY_RDP_PASSWORD='<密码>' \
//!   cargo run -p daisy-rdp --example cursor_sweep 192.168.10.20
//! ```
//!
//! 密码只从环境变量读。

use std::time::Duration;

use daisy_rdp::{ConnectionParams, InputEvent, Key, PointerButton, RdpClient};

/// 每个位置停留多久（毫秒）。
///
/// 要够长让服务端把光标形状发过来。实测桌面/边缘几毫秒就够，
/// 文本区稍慢，取大一点稳妥。
const DWELL_MS: u64 = 900;

/// 指针要停过的位置（1024×768 桌面坐标）。
///
/// 覆盖：桌面空白、窗口边缘（缩放光标）、**文本区（工字形）**、标题栏。
/// 工字形是最容易漏的一档——它用「反色」画，合成数据几乎造不出来。
const STOPS: &[(u16, u16, &str)] = &[
	(500, 400, "桌面空白"),
	(512, 8, "标题栏"),
	(2, 384, "左边缘（缩放）"),
	(1021, 384, "右边缘（缩放）"),
	(512, 383, "上边缘（缩放）"),
	(200, 200, "文本区左上"),
	(400, 250, "文本区中"),
	(600, 300, "文本区右"),
	(512, 700, "任务栏"),
];

fn main() {
	let args: Vec<String> = std::env::args().collect();
	let host = args.get(1).map(String::as_str).unwrap_or("192.168.10.20");
	let password = std::env::var("DAISY_RDP_PASSWORD").unwrap_or_else(|_| {
		eprintln!("缺少环境变量 DAISY_RDP_PASSWORD");
		std::process::exit(2);
	});

	let params = ConnectionParams::new(host, "Administrator", &password).with_resolution(1024, 768);
	println!("目标 {host}");

	let mut client = match RdpClient::new() {
		Ok(client) => client,
		Err(error) => {
			eprintln!("创建实例失败：{error}");
			std::process::exit(1);
		},
	};
	if let Err(error) = client.connect(&params) {
		eprintln!("连接失败：{error}");
		std::process::exit(1);
	}
	pump_until_frame(&client, 5000);
	pump_for(&client, Duration::from_millis(2000));

	// 起记事本，拿到一个真正的文本编辑区（工字形光标就住在那里）
	println!("起记事本…");
	send(&client, &win_r());
	pump_for(&client, Duration::from_millis(1200));
	send(&client, &type_text("notepad"));
	pump_for(&client, Duration::from_millis(400));
	send(&client, &[key(Key::Enter)]);
	pump_for(&client, Duration::from_millis(2500));

	let mut seen: Vec<(u64, u16, u16, u16, u16)> = Vec::new();
	for (x, y, label) in STOPS {
		send(&client, &[InputEvent::PointerMove { x: *x, y: *y }]);
		pump_for(&client, Duration::from_millis(DWELL_MS));
		if let Some((seq, sprite)) = client.latest_cursor() {
			let key = (sprite.width, sprite.height, sprite.hot_x, sprite.hot_y);
			let fresh = !seen.iter().any(|s| (s.1, s.2, s.3, s.4) == key);
			// 指针还要落一下，否则某些形状服务端不刷新
			send(
				&client,
				&[
					InputEvent::PointerMove {
						x: x.saturating_add(1),
						y: *y,
					},
					InputEvent::PointerButton {
						button: PointerButton::Left,
						down: false,
						x: *x,
						y: *y,
					},
				],
			);
			pump_for(&client, Duration::from_millis(DWELL_MS / 2));
			println!(
				"  {label} ({x},{y}) -> #{} {}x{} 热点({}, {}){}",
				seq,
				sprite.width,
				sprite.height,
				sprite.hot_x,
				sprite.hot_y,
				if fresh { "  ★新形状" } else { "" }
			);
			if fresh {
				seen.push((seq, sprite.width, sprite.height, sprite.hot_x, sprite.hot_y));
			}
		} else {
			println!("  {label} ({x},{y}) -> 未收到光标");
		}
	}
	println!("共见到 {} 种不同形状", seen.len());
}

fn key(key: Key) -> InputEvent {
	InputEvent::Key {
		key,
		down: true,
		repeat: false,
	}
}

/// 把一段ASCII 敲进去（`Win+R` 之后用来输入程序名）。
///
/// **只用可打印 ASCII**：非 ASCII 要靠键盘布局映射，探针不值得背这个复杂度。
fn type_text(text: &str) -> Vec<InputEvent> {
	const MAP: &[(char, Key)] = &[
		('a', Key::A),
		('d', Key::D),
		('e', Key::E),
		('n', Key::N),
		('o', Key::O),
		('t', Key::T),
		('p', Key::P),
		('d', Key::D),
		('.', Key::Period),
	];
	let mut events = Vec::new();
	for ch in text.chars() {
		let Some((_, k)) = MAP.iter().find(|(c, _)| *c == ch) else {
			continue;
		};
		events.push(InputEvent::Key {
			key: *k,
			down: true,
			repeat: false,
		});
		events.push(InputEvent::Key {
			key: *k,
			down: false,
			repeat: false,
		});
	}
	events
}

fn win_r() -> Vec<InputEvent> {
	vec![
		InputEvent::Key {
			key: Key::SuperLeft,
			down: true,
			repeat: false,
		},
		InputEvent::Key {
			key: Key::R,
			down: true,
			repeat: false,
		},
		InputEvent::Key {
			key: Key::R,
			down: false,
			repeat: false,
		},
		InputEvent::Key {
			key: Key::SuperLeft,
			down: false,
			repeat: false,
		},
	]
}

fn send(client: &RdpClient, events: &[InputEvent]) {
	for &event in events {
		if let Err(error) = client.send_input(event) {
			eprintln!("注入失败：{error}");
		}
	}
}

fn pump_until_frame(client: &RdpClient, timeout_ms: u64) {
	let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
	while std::time::Instant::now() < deadline {
		let _ = client.pump(30);
		if client.frame_seq() > 0 {
			return;
		}
	}
}

fn pump_for(client: &RdpClient, duration: Duration) {
	let deadline = std::time::Instant::now() + duration;
	while std::time::Instant::now() < deadline {
		let _ = client.pump(20);
	}
}
