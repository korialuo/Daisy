//! 键盘输入编码。
//!
//! 把 egui 的键盘事件翻译成终端能理解的字节序列。
//!
//! ## 为什么必须区分「文本」与「按键」两条路
//!
//! - `Event::Text` 携带的是**已经过键盘布局与输入法处理**的字符。普通输入
//!   （含中文、`Shift` 后的符号）必须走这条路，否则非拉丁布局会全乱。
//! - `Event::Key` 用来识别**控制键**。方向键、`Ctrl+字母`、功能键都不产生文本，
//!   只能在这里处理。
//!
//! 两边都要处理，且要避免重复——例如 `Ctrl+C` 在部分平台既发 `Key` 也发 `Text`。
//! 因此规则是：**按下 Ctrl 时丢弃 `Text`**，交由 `Key` 分支产生控制码。
//!
//! ## macOS 的 Command 键
//!
//! `⌘` 组合属于应用级快捷键（复制、粘贴、切换标签），**绝不发给终端**。
//! egui 会把它报成 `Copy` / `Paste` 等事件，`Key` 分支里直接忽略。
//!
//! ## 中文输入法走的是第三条路
//!
//! 输入法开着时，字符既不是 `Event::Text` 也不是 `Event::Key`，而是
//! `Event::Ime`：敲拼音的中间过程是 `Preedit`，选定之后才是一个 `Commit`。
//! 只处理前两条路的结果就是「切了输入法也打不出中文」。
//!
//! 组合期间的按键（退格改拼音、空格选词）属于输入法，必须挡住——
//! 否则用户每删一个拼音字母，远端 shell 也跟着删一个字。

use alacritty_terminal::term::TermMode;
use egui::{Event, ImeEvent, Key, Modifiers};

/// 方向键等「应用模式」下的前缀。终端切到该模式后方向键要换一套编码。
const SS3: u8 = b'O';

/// IME 组合状态。
///
/// 交互式输入要**跨帧**记住「现在是不是在打拼音」，所以不能是无状态的
/// 函数：`Preedit` 与随后的 `Key` / `Commit` 往往落在相邻两帧里。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImeState {
	composing: bool,
}

impl ImeState {
	/// 当前是否处于输入法组合中。
	#[must_use]
	pub fn composing(&self) -> bool {
		self.composing
	}

	/// 把一帧内的事件翻译成要写入 PTY 的字节，顺便更新组合状态。
	#[must_use]
	pub fn encode(&mut self, events: &[Event], mode: TermMode) -> Vec<u8> {
		let mut out = Vec::new();
		let ctrl_down = ctrl_is_down(events);

		for event in events {
			match event {
				Event::Ime(ImeEvent::Preedit { text, .. }) => {
					// 空 preedit 表示输入法收起了组合（用户取消或已选定）
					self.composing = !text.is_empty();
				},
				Event::Ime(ImeEvent::Commit(text)) => {
					self.composing = false;
					out.extend_from_slice(text.as_bytes());
				},
				// 组合期间的按键归输入法：拼音打错按退格是改拼音，空格/回车是选词。
				// 这些一个都不能进终端，否则会一边删拼音一边删远端字符。
				_ if self.composing => {},
				Event::Text(text) => {
					// Ctrl 组合由 Key 分支产生控制码，这里丢弃以免重复
					if !ctrl_down {
						out.extend_from_slice(text.as_bytes());
					}
				},
				Event::Key {
					key,
					pressed: true,
					modifiers,
					..
				} => {
					if let Some(bytes) = encode_key(*key, modifiers, mode) {
						out.extend_from_slice(&bytes);
					}
				},
				Event::Paste(text) => {
					if mode.contains(TermMode::BRACKETED_PASTE) {
						// 括号粘贴：让 shell 知道这是粘贴内容，不会被当作逐字输入
						out.extend_from_slice(b"\x1b[200~");
						out.extend_from_slice(text.as_bytes());
						out.extend_from_slice(b"\x1b[201~");
					} else {
						out.extend_from_slice(text.as_bytes());
					}
				},
				// 复制/剪切由界面层处理选区，不经终端
				Event::Copy | Event::Cut => {},
				_ => {},
			}
		}

		out
	}
}

/// 无状态编码：不跟踪输入法组合。
///
/// 只适合「一次性把一批事件翻成字节」的场合。交互式输入必须用
/// [`ImeState::encode`]——每帧新建一个状态会把组合上下文丢掉。
#[must_use]
pub fn encode(events: &[Event], mode: TermMode) -> Vec<u8> {
	ImeState::default().encode(events, mode)
}

/// 判断这一批事件里是否有 Ctrl 处于按下状态。
///
/// egui 的 `Event::Text` 不携带修饰键，只能从同一帧的 `Key` 事件里推断。
fn ctrl_is_down(events: &[Event]) -> bool {
	events
		.iter()
		.any(|event| matches!(event, Event::Key { pressed: true, modifiers, .. } if modifiers.ctrl))
}

/// 编码单个按键。返回 `None` 表示这个键不进终端。
#[must_use]
pub fn encode_key(key: Key, modifiers: &Modifiers, mode: TermMode) -> Option<Vec<u8>> {
	// ⌘ 组合是应用快捷键，不外发
	if modifiers.mac_cmd {
		return None;
	}

	// Ctrl + 字母 → 控制码（Ctrl+A = 0x01 … Ctrl+Z = 0x1A）。
	// 注意要走同一条 Alt 前缀路径，否则 Ctrl+Alt+字母 会丢掉 Meta 前缀。
	let body = if modifiers.ctrl
		&& let Some(letter) = letter_index(key)
	{
		Some(vec![letter + 1])
	} else {
		// 其余含 Ctrl 的组合终端一般也认（Ctrl+[ 之类），在下面按符号处理
		match key {
			Key::Enter => Some(b"\r".to_vec()),
			// 多数终端把退格发成 DEL，读作 0x7F
			Key::Backspace => Some(vec![0x7F]),
			Key::Tab if modifiers.shift => Some(b"\x1b[Z".to_vec()),
			Key::Tab => Some(b"\t".to_vec()),
			Key::Escape => Some(vec![0x1B]),

			Key::ArrowUp => Some(cursor_key(b'A', mode)),
			Key::ArrowDown => Some(cursor_key(b'B', mode)),
			Key::ArrowRight => Some(cursor_key(b'C', mode)),
			Key::ArrowLeft => Some(cursor_key(b'D', mode)),
			Key::Home => Some(cursor_key(b'H', mode)),
			Key::End => Some(cursor_key(b'F', mode)),

			Key::Insert => Some(b"\x1b[2~".to_vec()),
			Key::Delete => Some(b"\x1b[3~".to_vec()),
			Key::PageUp => Some(b"\x1b[5~".to_vec()),
			Key::PageDown => Some(b"\x1b[6~".to_vec()),

			Key::F1 => Some(b"\x1bOP".to_vec()),
			Key::F2 => Some(b"\x1bOQ".to_vec()),
			Key::F3 => Some(b"\x1bOR".to_vec()),
			Key::F4 => Some(b"\x1bOS".to_vec()),
			Key::F5 => Some(b"\x1b[15~".to_vec()),
			Key::F6 => Some(b"\x1b[17~".to_vec()),
			Key::F7 => Some(b"\x1b[18~".to_vec()),
			Key::F8 => Some(b"\x1b[19~".to_vec()),
			Key::F9 => Some(b"\x1b[20~".to_vec()),
			Key::F10 => Some(b"\x1b[21~".to_vec()),
			Key::F11 => Some(b"\x1b[23~".to_vec()),
			Key::F12 => Some(b"\x1b[24~".to_vec()),

			// Ctrl + 符号的控制码。这些键不产生文本，必须在这里处理。
			Key::OpenBracket if modifiers.ctrl => Some(vec![0x1B]),
			Key::Backslash if modifiers.ctrl => Some(vec![0x1C]),
			Key::CloseBracket if modifiers.ctrl => Some(vec![0x1D]),
			Key::Space if modifiers.ctrl => Some(vec![0x00]),

			// 可打印字符由 Event::Text 负责，这里不再重复产生
			_ => None,
		}
	};

	// Option / Alt 作为 Meta 前缀。terminal 惯例是前面加一个 ESC。
	match (body, modifiers.alt) {
		(Some(mut bytes), true) => {
			bytes.insert(0, 0x1B);
			Some(bytes)
		},
		(other, _) => other,
	}
}

/// 方向键 / Home / End 的编码。
///
/// 普通模式是 `CSI A`，应用模式（`APP_CURSOR`）是 `SS3 A`。
/// `vim`、`less` 会切到应用模式，编错了键就不认。
fn cursor_key(letter: u8, mode: TermMode) -> Vec<u8> {
	if mode.contains(TermMode::APP_CURSOR) {
		vec![0x1B, SS3, letter]
	} else {
		vec![0x1B, b'[', letter]
	}
}

/// `Key::A` → 0，`Key::Z` → 25。非字母返回 `None`。
fn letter_index(key: Key) -> Option<u8> {
	const LETTERS: [Key; 26] = [
		Key::A,
		Key::B,
		Key::C,
		Key::D,
		Key::E,
		Key::F,
		Key::G,
		Key::H,
		Key::I,
		Key::J,
		Key::K,
		Key::L,
		Key::M,
		Key::N,
		Key::O,
		Key::P,
		Key::Q,
		Key::R,
		Key::S,
		Key::T,
		Key::U,
		Key::V,
		Key::W,
		Key::X,
		Key::Y,
		Key::Z,
	];

	LETTERS
		.iter()
		.position(|candidate| *candidate == key)
		.map(|index| index as u8)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn plain() -> Modifiers {
		Modifiers::default()
	}

	fn ctrl() -> Modifiers {
		Modifiers {
			ctrl: true,
			..Modifiers::default()
		}
	}

	fn alt() -> Modifiers {
		Modifiers {
			alt: true,
			..Modifiers::default()
		}
	}

	fn normalize() -> TermMode {
		TermMode::empty()
	}

	fn text_of(key: Key, modifiers: &Modifiers) -> Vec<u8> {
		encode_key(key, modifiers, normalize()).unwrap_or_default()
	}

	// ---------- 基本控制键 ----------

	#[test]
	fn enter_sends_carriage_return() {
		assert_eq!(text_of(Key::Enter, &plain()), b"\r");
	}

	#[test]
	fn backspace_sends_del_not_bs() {
		// 终端惯例：退格是 0x7F，而不是 0x08
		assert_eq!(text_of(Key::Backspace, &plain()), vec![0x7F]);
	}

	#[test]
	fn tab_and_shift_tab() {
		assert_eq!(text_of(Key::Tab, &plain()), b"\t");

		let shift_tab = Modifiers {
			shift: true,
			..Modifiers::default()
		};
		assert_eq!(text_of(Key::Tab, &shift_tab), b"\x1b[Z");
	}

	#[test]
	fn escape_sends_esc() {
		assert_eq!(text_of(Key::Escape, &plain()), vec![0x1B]);
	}

	// ---------- Ctrl 组合 ----------

	#[test]
	fn ctrl_letters_map_to_control_codes() {
		assert_eq!(text_of(Key::A, &ctrl()), vec![0x01]);
		assert_eq!(text_of(Key::C, &ctrl()), vec![0x03], "Ctrl+C 应是 ETX");
		assert_eq!(text_of(Key::D, &ctrl()), vec![0x04]);
		assert_eq!(text_of(Key::Z, &ctrl()), vec![0x1A]);
	}

	#[test]
	fn ctrl_bracket_family() {
		assert_eq!(text_of(Key::OpenBracket, &ctrl()), vec![0x1B]);
		assert_eq!(text_of(Key::Backslash, &ctrl()), vec![0x1C]);
		assert_eq!(text_of(Key::CloseBracket, &ctrl()), vec![0x1D]);
	}

	#[test]
	fn ctrl_space_is_nul() {
		assert_eq!(text_of(Key::Space, &ctrl()), vec![0x00]);
	}

	#[test]
	fn ctrl_with_symbol_without_rule_is_ignored() {
		// Ctrl+1 没有通用控制码，不应产生任何字节
		assert!(encode_key(Key::Num1, &ctrl(), normalize()).is_none());
	}

	// ---------- 方向键 ----------

	#[test]
	fn arrows_in_normal_mode_use_csi() {
		assert_eq!(text_of(Key::ArrowUp, &plain()), b"\x1b[A");
		assert_eq!(text_of(Key::ArrowDown, &plain()), b"\x1b[B");
		assert_eq!(text_of(Key::ArrowRight, &plain()), b"\x1b[C");
		assert_eq!(text_of(Key::ArrowLeft, &plain()), b"\x1b[D");
	}

	#[test]
	fn arrows_in_application_mode_use_ss3() {
		let app = TermMode::APP_CURSOR;

		assert_eq!(encode_key(Key::ArrowUp, &plain(), app).unwrap(), b"\x1bOA");
		assert_eq!(
			encode_key(Key::ArrowLeft, &plain(), app).unwrap(),
			b"\x1bOD"
		);
	}

	#[test]
	fn home_end_follow_cursor_mode() {
		assert_eq!(text_of(Key::Home, &plain()), b"\x1b[H");
		assert_eq!(text_of(Key::End, &plain()), b"\x1b[F");

		let app = TermMode::APP_CURSOR;
		assert_eq!(encode_key(Key::Home, &plain(), app).unwrap(), b"\x1bOH");
		assert_eq!(encode_key(Key::End, &plain(), app).unwrap(), b"\x1bOF");
	}

	// ---------- 编辑与翻页 ----------

	#[test]
	fn navigation_keys() {
		assert_eq!(text_of(Key::Insert, &plain()), b"\x1b[2~");
		assert_eq!(text_of(Key::Delete, &plain()), b"\x1b[3~");
		assert_eq!(text_of(Key::PageUp, &plain()), b"\x1b[5~");
		assert_eq!(text_of(Key::PageDown, &plain()), b"\x1b[6~");
	}

	// ---------- 功能键 ----------

	#[test]
	fn function_keys() {
		assert_eq!(text_of(Key::F1, &plain()), b"\x1bOP");
		assert_eq!(text_of(Key::F4, &plain()), b"\x1bOS");
		assert_eq!(text_of(Key::F5, &plain()), b"\x1b[15~");
		assert_eq!(text_of(Key::F12, &plain()), b"\x1b[24~");
	}

	// ---------- Alt ----------

	#[test]
	fn alt_prefixes_escape() {
		assert_eq!(text_of(Key::Enter, &alt()), b"\x1b\r");
		assert_eq!(text_of(Key::ArrowUp, &alt()), b"\x1b\x1b[A");
	}

	#[test]
	fn alt_with_ctrl_letter_keeps_control_code() {
		let both = Modifiers {
			alt: true,
			ctrl: true,
			..Modifiers::default()
		};
		assert_eq!(text_of(Key::A, &both), vec![0x1B, 0x01]);
	}

	// ---------- Command 键不外发 ----------

	#[test]
	fn mac_command_is_not_sent_to_terminal() {
		let cmd = Modifiers {
			mac_cmd: true,
			command: true,
			..Modifiers::default()
		};
		assert!(
			encode_key(Key::C, &cmd, normalize()).is_none(),
			"⌘C 不应进终端"
		);
		assert!(
			encode_key(Key::V, &cmd, normalize()).is_none(),
			"⌘V 不应进终端"
		);
	}

	// ---------- 文本路径 ----------

	#[test]
	fn plain_text_passes_through() {
		let events = vec![Event::Text("ls -la".to_string())];
		assert_eq!(encode(&events, normalize()), b"ls -la");
	}

	#[test]
	fn unicode_text_passes_through() {
		let events = vec![Event::Text("中文".to_string())];
		assert_eq!(encode(&events, normalize()), "中文".as_bytes());
	}

	#[test]
	fn text_is_dropped_when_ctrl_is_down() {
		// 有平台在 Ctrl+C 时同时发 Key 与 Text，此时必须丢掉 Text 避免重复
		let events = vec![
			Event::Key {
				key: Key::C,
				physical_key: None,
				pressed: true,
				repeat: false,
				modifiers: ctrl(),
			},
			Event::Text("c".to_string()),
		];

		assert_eq!(
			encode(&events, normalize()),
			vec![0x03],
			"不应出现多余的 'c'"
		);
	}

	#[test]
	fn key_release_is_ignored() {
		let events = vec![Event::Key {
			key: Key::Enter,
			physical_key: None,
			pressed: false,
			repeat: false,
			modifiers: plain(),
		}];
		assert!(
			encode(&events, normalize()).is_empty(),
			"抬起事件不应产生输入"
		);
	}

	// ---------- 输入法 ----------

	fn preedit(text: &str) -> Event {
		Event::Ime(ImeEvent::Preedit {
			text: text.to_owned(),
			active_range_chars: None,
		})
	}

	fn commit(text: &str) -> Event {
		Event::Ime(ImeEvent::Commit(text.to_owned()))
	}

	fn key_event(key: Key) -> Event {
		Event::Key {
			key,
			physical_key: None,
			pressed: true,
			repeat: false,
			modifiers: plain(),
		}
	}

	#[test]
	fn ime_commit_reaches_the_terminal() {
		// 中文输入法选中之后只有一个 Commit，字符全在里面
		let mut ime = ImeState::default();
		let events = vec![preedit("zhong"), commit("中文")];

		assert_eq!(ime.encode(&events, normalize()), "中文".as_bytes());
		assert!(!ime.composing(), "Commit 之后组合已结束");
	}

	#[test]
	fn keys_during_composition_do_not_reach_the_terminal() {
		let mut ime = ImeState::default();
		// 第一帧：敲下拼音，组合开始
		assert!(ime.encode(&[preedit("ni")], normalize()).is_empty());
		assert!(ime.composing());

		// 组合中按退格是「改拼音」，不能删远端字符
		assert!(
			ime.encode(&[key_event(Key::Backspace)], normalize()).is_empty(),
			"组合中的退格被输入法吃了才对"
		);
		// 选词用的空格/回车同理
		assert!(ime.encode(&[key_event(Key::Enter)], normalize()).is_empty());
	}

	#[test]
	fn clearing_preedit_ends_composition() {
		let mut ime = ImeState::default();
		assert!(ime.encode(&[preedit("n")], normalize()).is_empty());
		assert!(ime.composing());

		// 空 preedit = 输入法收起组合（用户取消）
		assert!(ime.encode(&[preedit("")], normalize()).is_empty());
		assert!(!ime.composing());

		// 之后退格恢复正常
		assert_eq!(ime.encode(&[key_event(Key::Backspace)], normalize()), vec![0x7F]);
	}

	#[test]
	fn composition_does_not_swallow_commit() {
		// 同一帧里先 Preedit 再 Commit：Commit 必须照常送出
		let mut ime = ImeState::default();
		assert!(ime.encode(&[preedit("ni")], normalize()).is_empty());
		assert_eq!(
			ime.encode(&[preedit("ni"), commit("你")], normalize()),
			"你".as_bytes()
		);
	}

	// ---------- 粘贴 ----------

	#[test]
	fn paste_without_bracketed_mode_is_raw() {
		let events = vec![Event::Paste("pasted\ntext".to_string())];
		assert_eq!(encode(&events, normalize()), b"pasted\ntext");
	}

	#[test]
	fn paste_with_bracketed_mode_is_wrapped() {
		let events = vec![Event::Paste("hello".to_string())];
		let encoded = encode(&events, TermMode::BRACKETED_PASTE);

		assert!(encoded.starts_with(b"\x1b[200~"), "缺少起始标记");
		assert!(encoded.ends_with(b"\x1b[201~"), "缺少结束标记");
		assert!(encoded.windows(5).any(|window| window == b"hello"));
	}

	#[test]
	fn copy_and_cut_do_not_reach_terminal() {
		let events = vec![Event::Copy, Event::Cut];
		assert!(encode(&events, normalize()).is_empty());
	}

	// ---------- 组合场景 ----------

	#[test]
	fn mixed_events_in_one_frame() {
		let events = vec![
			Event::Text("ec".to_string()),
			Event::Key {
				key: Key::H,
				physical_key: None,
				pressed: true,
				repeat: false,
				modifiers: ctrl(),
			},
		];

		// Ctrl 按下时文本被丢弃，只留下控制码
		assert_eq!(encode(&events, normalize()), vec![0x08]);
	}

	#[test]
	fn letter_index_covers_all_letters() {
		assert_eq!(letter_index(Key::A), Some(0));
		assert_eq!(letter_index(Key::Z), Some(25));
		assert_eq!(letter_index(Key::Enter), None);
		assert_eq!(letter_index(Key::Num0), None);
	}
}
