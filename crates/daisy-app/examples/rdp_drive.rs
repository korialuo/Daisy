//! 无人值守驱动一台 RDP 主机：按键脚本、画面快照、剪贴板双向。
//!
//! 为什么需要它：AppleScript 点不了 Daisy 的窗口（-10004），合成 HID 也不被
//! 允许，所以「在这台 Mac 上操作另一台 Windows」只能靠**应用自己**的输入注入。
//! 这个示例把「连上 → 按脚本敲键 → 截图核对 → 读剪贴板」做成分步脚本，
//! 让装环境、改设置、跑一段命令这类重复劳动可以无人值守完成。
//!
//! ```sh
//! DAISY_DATA_DIR=/tmp/daisy-remote DAISY_SEED_PASSWORD='...' \
//!   cargo run -p daisy-app --example rdp_drive -- <主机名> \
//!   'wait:6000; key:win+r; wait:800; type:cmd; enter; wait:3000; shot:/tmp/d1.bmp'
//! ```
//!
//! 步骤用 `;` 分隔：
//!
//! | 步骤 | 含义 |
//! | --- | --- |
//! | `wait:800` | 等 800 ms（期间照常 pump，画面与剪贴板都在更新） |
//! | `key:win+r` / `key:ctrl+v` / `enter` / `esc` / `tab` | 按键，`+` 表示组合（先按住、抬起按逆序） |
//! | `type:Hello 123` | 敲一串 ASCII（大写与上档符号自动加 Shift，`\n` 为回车） |
//! | `shot:/tmp/a.bmp` | 把当前画面存成 BMP（24 位，ViewImage 能直接看） |
//! | `sendclip:/tmp/script.ps1` | 把文件内容放进远端剪贴板，等它被确认（最多 5 s） |
//! | `paste` | 注入 Ctrl+V（**必须先 `sendclip`**，否则远端粘的是旧内容） |
//! | `readclip` | 等远端剪贴板有新文本（最多 8 s）并打印出来，前后各有一行标记 |
//!
//! 主机、账号、密码沿用库里那台（与界面走同一条 store 路径），先跑
//! `examples/seed_host.rs` 灌进去。

use std::path::Path;
use std::time::{Duration, Instant};

use daisy_core::config::AppPaths;
use daisy_core::key::EmbeddedKey;
use daisy_rdp::{ConnectionParams, InputEvent, Key, RdpClient, to_rgba};
use daisy_store::Database;

/// 一次按键组合，例如 `win+r`。
fn parse_combo(spec: &str) -> Vec<Key> {
	spec.split('+')
		.map(|part| match part.trim().to_ascii_lowercase().as_str() {
			"win" | "super" | "cmd" => Key::SuperLeft,
			"ctrl" | "control" => Key::ControlLeft,
			"shift" => Key::ShiftLeft,
			"alt" => Key::AltLeft,
			"enter" | "return" => Key::Enter,
			"esc" | "escape" => Key::Escape,
			"tab" => Key::Tab,
			"space" => Key::Space,
			"backspace" => Key::Backspace,
			"delete" | "del" => Key::Delete,
			"up" => Key::ArrowUp,
			"down" => Key::ArrowDown,
			"left" => Key::ArrowLeft,
			"right" => Key::ArrowRight,
			other => single_key(other).unwrap_or_else(|| panic!("不认识的键：{other}")),
		})
		.collect()
}

/// 单个字符 → 物理键（US 布局）。
fn single_key(text: &str) -> Option<Key> {
	let bytes = text.as_bytes();
	if bytes.len() != 1 {
		return None;
	}
	let c = bytes[0] as char;
	Some(match c {
		'a'..='z' => letter(c.to_ascii_uppercase()),
		'0'..='9' => digit(c),
		' ' => Key::Space,
		'-' => Key::Minus,
		'=' => Key::Equals,
		'[' => Key::OpenBracket,
		']' => Key::CloseBracket,
		'\\' => Key::Backslash,
		';' => Key::Semicolon,
		'\'' => Key::Quote,
		',' => Key::Comma,
		'.' => Key::Period,
		'/' => Key::Slash,
		'`' => Key::Backtick,
		_ => return None,
	})
}

fn letter(c: char) -> Key {
	match c {
		'A' => Key::A,
		'B' => Key::B,
		'C' => Key::C,
		'D' => Key::D,
		'E' => Key::E,
		'F' => Key::F,
		'G' => Key::G,
		'H' => Key::H,
		'I' => Key::I,
		'J' => Key::J,
		'K' => Key::K,
		'L' => Key::L,
		'M' => Key::M,
		'N' => Key::N,
		'O' => Key::O,
		'P' => Key::P,
		'Q' => Key::Q,
		'R' => Key::R,
		'S' => Key::S,
		'T' => Key::T,
		'U' => Key::U,
		'V' => Key::V,
		'W' => Key::W,
		'X' => Key::X,
		'Y' => Key::Y,
		'Z' => Key::Z,
		_ => unreachable!("非字母"),
	}
}

fn digit(c: char) -> Key {
	match c {
		'1' => Key::Digit1,
		'2' => Key::Digit2,
		'3' => Key::Digit3,
		'4' => Key::Digit4,
		'5' => Key::Digit5,
		'6' => Key::Digit6,
		'7' => Key::Digit7,
		'8' => Key::Digit8,
		'9' => Key::Digit9,
		'0' => Key::Digit0,
		_ => unreachable!("非数字"),
	}
}

/// 字符是否需要 Shift（ASCII 上档符号）。
fn shift_of(c: char) -> Option<Key> {
	Some(match c {
		'!' => Key::Digit1,
		'@' => Key::Digit2,
		'#' => Key::Digit3,
		'$' => Key::Digit4,
		'%' => Key::Digit5,
		'^' => Key::Digit6,
		'&' => Key::Digit7,
		'*' => Key::Digit8,
		'(' => Key::Digit9,
		')' => Key::Digit0,
		'_' => Key::Minus,
		'+' => Key::Equals,
		'{' => Key::OpenBracket,
		'}' => Key::CloseBracket,
		'|' => Key::Backslash,
		':' => Key::Semicolon,
		'"' => Key::Quote,
		'<' => Key::Comma,
		'>' => Key::Period,
		'?' => Key::Slash,
		'~' => Key::Backtick,
		_ => return None,
	})
}

fn press(client: &RdpClient, key: Key, down: bool) {
	let _ = client.send_input(InputEvent::Key {
		key,
		down,
		repeat: false,
	});
}

fn tap(client: &RdpClient, key: Key) {
	press(client, key, true);
	client.input_gap(30);
	press(client, key, false);
	client.input_gap(40);
}

/// 敲一个字符。
fn type_char(client: &RdpClient, c: char) {
	if let Some(key) = shift_of(c) {
		press(client, Key::ShiftLeft, true);
		client.input_gap(20);
		tap(client, key);
		press(client, Key::ShiftLeft, false);
		client.input_gap(30);
		return;
	}
	if c.is_ascii_uppercase() {
		press(client, Key::ShiftLeft, true);
		client.input_gap(20);
		tap(client, letter(c));
		press(client, Key::ShiftLeft, false);
		client.input_gap(30);
		return;
	}
	match single_key(&c.to_string()) {
		Some(key) => tap(client, key),
		None => tracing::warn!("跳过无法映射的字符：{c:?}"),
	}
}

/// 把一帧存成 24 位 BMP（无第三方依赖，看图工具直接可读）。
fn save_bmp(path: &str, frame: &daisy_rdp::RgbaFrame) -> std::io::Result<()> {
	let width = frame.width as usize;
	let height = frame.height as usize;
	let row_bytes = width * 3;
	let padding = (4 - row_bytes % 4) % 4;
	let stride = row_bytes + padding;
	let pixel_bytes = stride * height;
	let file_size = 54 + pixel_bytes;

	let mut out = Vec::with_capacity(file_size);
	out.extend_from_slice(b"BM");
	out.extend_from_slice(&(file_size as u32).to_le_bytes());
	out.extend_from_slice(&0u32.to_le_bytes());
	out.extend_from_slice(&54u32.to_le_bytes());
	out.extend_from_slice(&40u32.to_le_bytes());
	out.extend_from_slice(&(width as i32).to_le_bytes());
	out.extend_from_slice(&(height as i32).to_le_bytes());
	out.extend_from_slice(&1u16.to_le_bytes());
	out.extend_from_slice(&24u16.to_le_bytes());
	out.extend_from_slice(&0u32.to_le_bytes());
	out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
	out.extend_from_slice(&2835i32.to_le_bytes());
	out.extend_from_slice(&2835i32.to_le_bytes());
	out.extend_from_slice(&0u32.to_le_bytes());
	out.extend_from_slice(&0u32.to_le_bytes());

	// BMP 自下而上，且是 BGR 顺序
	for y in (0..height).rev() {
		let row = y * width * 4;
		for x in 0..width {
			let i = row + x * 4;
			out.push(frame.pixels[i + 2]);
			out.push(frame.pixels[i + 1]);
			out.push(frame.pixels[i]);
		}
		out.extend(std::iter::repeat_n(0u8, padding));
	}
	std::fs::write(path, out)
}

fn shot(client: &RdpClient, path: &str) {
	match client
		.latest_frame()
		.and_then(|(_, buffer)| to_rgba(&buffer))
	{
		Some(frame) => match save_bmp(path, &frame) {
			Ok(()) => println!("    截图 {}（{}×{}）", path, frame.width, frame.height),
			Err(error) => println!("    截图失败：{error}"),
		},
		None => println!("    还没有画面，截图跳过"),
	}
}

fn main() {
	let mut args = std::env::args().skip(1);
	let name = args.next().expect("用法：rdp_drive <主机名> '<步骤>'");
	let script = args.next().unwrap_or_default();

	let paths = match std::env::var_os("DAISY_DATA_DIR") {
		Some(base) if !base.is_empty() => AppPaths::from_base(base),
		_ => AppPaths::discover().expect("定位应用目录失败"),
	};
	let db = Database::open(&paths, &EmbeddedKey).expect("打开加密库失败");

	let host = db
		.hosts()
		.expect("读取主机失败")
		.into_iter()
		.find(|host| host.name == name)
		.unwrap_or_else(|| panic!("库里没有主机「{name}」"));
	let password = db
		.host_password(host.id.expect("主机缺少主键"))
		.expect("读取密码失败")
		.expect("这台主机没有存密码");

	println!(
		"目标：{} → {}:{}（用户 {}）",
		host.name,
		host.host,
		host.port,
		host.username.clone().unwrap_or_default()
	);

	let params = ConnectionParams::new(
		host.host.clone(),
		host.username.clone().unwrap_or_default(),
		password.to_string(),
	)
	.with_port(host.port)
	.with_resolution(1280, 800);

	// 自己建一份剪贴板再注入：`arm()` 与 `last_server_message()` 只在
	// `Clipboard` 上有，而这条诊断信息是判断「远端到底动静了没有」的唯一依据。
	let clipboard = std::sync::Arc::new(daisy_rdp::Clipboard::new());
	let mut client = RdpClient::new().expect("初始化 FreeRDP 失败");
	client.trust_certificate(None);
	client.use_clipboard(clipboard.clone());
	client.enable_clipboard();
	match client.connect(&params) {
		Ok(()) => println!("已连接"),
		Err(error) => {
			println!("连接失败：{error}");
			std::process::exit(1);
		},
	}

	for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
		println!("== {step}");
		let (verb, arg) = step.split_once(':').unwrap_or((step, ""));
		match verb.trim() {
			"wait" => {
				let ms: u64 = arg.trim().parse().unwrap_or(500);
				let deadline = Instant::now() + Duration::from_millis(ms);
				while Instant::now() < deadline {
					let _ = client.pump(50);
				}
			},
			"key" => {
				let combo = parse_combo(arg);
				for key in &combo {
					press(&client, *key, true);
					client.input_gap(40);
				}
				for key in combo.iter().rev() {
					press(&client, *key, false);
					client.input_gap(40);
				}
			},
			"enter" | "esc" | "tab" => {
				let key = match verb.trim() {
					"enter" => Key::Enter,
					"esc" => Key::Escape,
					_ => Key::Tab,
				};
				tap(&client, key);
			},
			"type" => {
				for c in arg.chars() {
					if c == '\n' {
						tap(&client, Key::Enter);
					} else if c.is_ascii() {
						type_char(&client, c);
					}
				}
			},
			"shot" => {
				let _ = client.pump(200);
				shot(&client, arg.trim());
			},
			"sendclip" => {
				let text = std::fs::read_to_string(Path::new(arg.trim())).expect("读不到脚本文件");
				println!("    发送 {} 字节到远端剪贴板", text.len());
				client.send_clipboard(text);
				// 服务端要确认格式列表；没确认就按键会粘到空剪贴板
				let deadline = Instant::now() + Duration::from_secs(5);
				while Instant::now() < deadline {
					if client.clipboard_format_list_acked() {
						println!("    剪贴板已被确认");
						break;
					}
					let _ = client.pump(100);
				}
			},
			"arm" => {
				// rdpclip 在客户端上报过格式列表之前不监听会话剪贴板——不先
				// 「开嗓」，远端 `clip` 拷进去的东西永远不会广播过来。
				clipboard.arm();
				let deadline = Instant::now() + Duration::from_secs(5);
				while Instant::now() < deadline {
					if client.clipboard_format_list_acked() {
						println!("    已开嗓");
						break;
					}
					let _ = client.pump(100);
				}
			},
			"paste" => {
				press(&client, Key::ControlLeft, true);
				client.input_gap(60);
				tap(&client, Key::V);
				press(&client, Key::ControlLeft, false);
				client.input_gap(120);
			},
			"readclip" => {
				let deadline = Instant::now() + Duration::from_secs(8);
				let mut got = None;
				while Instant::now() < deadline {
					let _ = client.pump(200);
					if let Some(text) = client.take_remote_clipboard() {
						got = Some(text);
						break;
					}
				}
				match got {
					Some(text) => {
						println!("----- 剪贴板开始 -----");
						println!("{text}");
						println!("----- 剪贴板结束 -----");
					},
					None => {
						// 分诊：MonitorReady 之后再没报文 = rdpclip 卡死；
						// 有「格式列表应答」= 通道活着、只是远端没广播。
						match clipboard.last_server_message() {
							Some((what, age)) => {
								println!("    剪贴板无新内容（最近报文：{what}，{age:?} 前）")
							},
							None => println!("    剪贴板无新内容（服务端一个报文都没发过）"),
						}
					},
				}
			},
			other => println!("    未知步骤：{other}"),
		}
	}

	// 收尾：多 pump 一会儿让最后的输入真的上线（send_input 返回 Ok 不代表受理）
	for _ in 0..10 {
		let _ = client.pump(50);
	}
	println!("结束");
}
