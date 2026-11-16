//! 键鼠输入注入：把界面的按键与指针动作翻译成 FreeRDP 的输入事件。
//!
//! 本模块**不依赖任何 GUI**：只认「物理键」与「桌面像素坐标」。
//! `egui::Key` → [`Key`] 的桥接在 app 层（`views/rdp_view.rs`），
//! 于是协议常量（扫描码、指针标志位）全部集中在这里，可以单测。
//!
//! ## 扫描码标识「位置」，不是字符
//!
//! RDP 传的是 PC 键盘扫描码——它标识键在键盘上的**位置**，由远端按自己的
//! 键盘布局翻译成字符。所以这里绝不能按字符映射：`A` 与 `a` 是同一个扫描码，
//! 大小写由 Shift 的按下状态决定。同理 [`Key::Semicolon`] 在 US 布局下是 `;`，
//! 在别的布局下是别的字符——这正是我们要的行为。
//!
//! ## 手写常量必须对着头文件核
//!
//! FreeRDP 把这些都定义成**宏**，bindgen 导不出来。手写常量一旦写错，
//! 编译期不会报错，运行期会以毫不相干的表象炸出来——`PIXEL_FORMAT_XRGB32`
//! 那次就是这么被藏的（见 `freerdp.rs` 顶部）。所以下面每个常量都标了出处，
//! 并且有单测钉住数值。

/// `KBDEXT`：扫描码第 9 位，标识扩展键。
///
/// 出处：`winpr3/winpr/input.h`（`#define KBDEXT 0x0100u`）。
///
/// 它不是"修饰键"，而是扫描码的一部分：右 Ctrl、方向键、小键盘除号 /
/// 回车等键靠它区分。FreeRDP 的 `MAKE_RDP_SCANCODE(code, extended)` 就是
/// 把它或进低 8 位。
const KBDEXT: u16 = 0x0100;

/// 一个 RDP 键盘扫描码。
///
/// 低 8 位是码，[`KBDEXT`] 位表示扩展键。用 newtype 而不是裸 `u16`，
/// 免得和指针标志位、坐标之类的整数混着传。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scancode(u16);

impl Scancode {
	/// 由 FreeRDP `RDP_SCANCODE_*` 宏的两个参数构造。
	pub const fn new(code: u8, extended: bool) -> Self {
		Self(code as u16 | if extended { KBDEXT } else { 0 })
	}

	/// 低 8 位的键码。
	pub const fn code(self) -> u8 {
		self.0 as u8
	}

	/// 是否扩展键。
	pub const fn is_extended(self) -> bool {
		self.0 & KBDEXT != 0
	}

	/// 作为 `freerdp_input_send_keyboard_event_ex` 的 `rdp_scancode` 参数。
	pub const fn as_u32(self) -> u32 {
		self.0 as u32
	}
}

/// 键盘事件标志（`KBD_FLAGS_*`，出处 `freerdp/input.h`）。
///
/// 我们走 `freerdp_input_send_keyboard_event_ex`，标志位由 FreeRDP 自己算，
/// 所以这里主要供测试与文档引用。
pub mod kbd_flags {
	/// 同 [`super::KBDEXT`]——扩展键位。
	pub const EXTENDED: u16 = 0x0100;
	/// 该键此前已经按下（长按重复）。
	pub const DOWN: u16 = 0x4000;
}

/// 指针事件标志（`PTR_FLAGS_*`，出处 `freerdp/input.h`）。
pub mod ptr_flags {
	/// 水平滚轮。
	pub const HWHEEL: u16 = 0x0400;
	/// 垂直滚轮。
	pub const WHEEL: u16 = 0x0200;
	/// 滚轮方向为负（配合低 9 位的补码，见 [`super::wheel_flags`]）。
	pub const WHEEL_NEGATIVE: u16 = 0x0100;
	/// 指针移动。
	pub const MOVE: u16 = 0x0800;
	/// 按键按下（抬起时不带此位）。
	pub const DOWN: u16 = 0x8000;
	/// 左键。
	pub const BUTTON1: u16 = 0x1000;
	/// 右键。
	pub const BUTTON2: u16 = 0x2000;
	/// 中键。
	pub const BUTTON3: u16 = 0x4000;
	/// 滚轮量的掩码——低 9 位放补码。
	pub const WHEEL_ROTATION_MASK: u16 = 0x01FF;
}

/// `WHEEL_DELTA`：Windows 约定一格滚轮 120（出处 `freerdp/input.h` 用法）。
pub const WHEEL_DELTA: i16 = 120;

/// 定义 [`Key`] 枚举与它到扫描码的对照表。
///
/// 用宏是为了**保证两者不会走散**：没有「加了变体却忘了填扫描码」这种状态，
/// 也不必把 120 行表格重复写两遍。同一行里同时给出枚举名、键码与扩展位。
macro_rules! key_table {
	($( $(#[$doc:meta])* $name:ident = ($code:literal, $extended:literal) ),* $(,)?) => {
		/// 一个物理按键。
		///
		/// 变体名对应 US 布局下的字符 / 位置，但语义是**位置**而不是字符
		/// （见模块文档）。
		#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
		pub enum Key {
			$( $(#[$doc])* $name, )*
		}

		impl Key {
			/// 该键的 RDP 扫描码。
			pub const fn scancode(self) -> Scancode {
				match self {
					$( Self::$name => Scancode::new($code, $extended), )*
				}
			}

			/// 全部按键。用于遍历自检（例如键码必须落在 8 位内）。
			pub const ALL: &'static [Key] = &[ $( Self::$name, )* ];
		}
	};
}

key_table! {
	// ---- 字母。出处 RDP_SCANCODE_KEY_* ----
	A = (0x1E, false),
	B = (0x30, false),
	C = (0x2E, false),
	D = (0x20, false),
	E = (0x12, false),
	F = (0x21, false),
	G = (0x22, false),
	H = (0x23, false),
	I = (0x17, false),
	J = (0x24, false),
	K = (0x25, false),
	L = (0x26, false),
	M = (0x32, false),
	N = (0x31, false),
	O = (0x18, false),
	P = (0x19, false),
	Q = (0x10, false),
	R = (0x13, false),
	S = (0x1F, false),
	T = (0x14, false),
	U = (0x16, false),
	V = (0x2F, false),
	W = (0x11, false),
	X = (0x2D, false),
	Y = (0x15, false),
	Z = (0x2C, false),

	// ---- 主键盘数字。出处 RDP_SCANCODE_KEY_1 .. KEY_0 ----
	Digit1 = (0x02, false),
	Digit2 = (0x03, false),
	Digit3 = (0x04, false),
	Digit4 = (0x05, false),
	Digit5 = (0x06, false),
	Digit6 = (0x07, false),
	Digit7 = (0x08, false),
	Digit8 = (0x09, false),
	Digit9 = (0x0A, false),
	Digit0 = (0x0B, false),

	// ---- 符号键。出处 RDP_SCANCODE_OEM_* ----
	/// `` ` ``（US）／日文全角半角。
	Backtick = (0x29, false),
	/// `-`。
	Minus = (0x0C, false),
	/// `=`。
	Equals = (0x0D, false),
	/// `[`。
	OpenBracket = (0x1A, false),
	/// `]`。
	CloseBracket = (0x1B, false),
	/// `\`（回车左侧）。
	Backslash = (0x2B, false),
	/// `;`。
	Semicolon = (0x27, false),
	/// `'`。
	Quote = (0x28, false),
	/// `,`。
	Comma = (0x33, false),
	/// `.`。
	Period = (0x34, false),
	/// `/`。
	Slash = (0x35, false),
	/// ISO 布局多出来的那颗 `\`（左 Shift 与 Z 之间）。US 键盘没有。
	IntlBackslash = (0x56, false),

	// ---- 编辑键 ----
	/// 主键盘回车（区别于小键盘回车）。
	Enter = (0x1C, false),
	Escape = (0x01, false),
	Tab = (0x0F, false),
	Backspace = (0x0E, false),
	Space = (0x39, false),

	// ---- 导航键（全部是扩展键）----
	Insert = (0x52, true),
	Delete = (0x53, true),
	Home = (0x47, true),
	End = (0x4F, true),
	PageUp = (0x49, true),
	PageDown = (0x51, true),

	// ---- 方向键（扩展）----
	ArrowUp = (0x48, true),
	ArrowDown = (0x50, true),
	ArrowLeft = (0x4B, true),
	ArrowRight = (0x4D, true),

	// ---- 功能键 ----
	F1 = (0x3B, false),
	F2 = (0x3C, false),
	F3 = (0x3D, false),
	F4 = (0x3E, false),
	F5 = (0x3F, false),
	F6 = (0x40, false),
	F7 = (0x41, false),
	F8 = (0x42, false),
	F9 = (0x43, false),
	F10 = (0x44, false),
	F11 = (0x57, false),
	F12 = (0x58, false),
	F13 = (0x64, false),
	F14 = (0x65, false),
	F15 = (0x66, false),
	F16 = (0x67, false),
	F17 = (0x68, false),
	F18 = (0x69, false),
	F19 = (0x6A, false),
	F20 = (0x6B, false),
	F21 = (0x6C, false),
	F22 = (0x6D, false),
	F23 = (0x6E, false),
	F24 = (0x6F, false),

	// ---- 修饰键 ----
	ShiftLeft = (0x2A, false),
	ShiftRight = (0x36, false),
	/// 左 Ctrl 与右 Ctrl **键码相同**，只差扩展位。
	ControlLeft = (0x1D, false),
	ControlRight = (0x1D, true),
	/// 左 Alt 与右 Alt（AltGr）同理。
	AltLeft = (0x38, false),
	AltRight = (0x38, true),
	/// 左 Win / Cmd 键（扩展）。
	SuperLeft = (0x5B, true),
	/// 右 Win / Cmd 键（扩展）。
	SuperRight = (0x5C, true),
	CapsLock = (0x3A, false),

	// ---- 锁定与系统键 ----
	NumLock = (0x45, false),
	ScrollLock = (0x46, false),
	/// PrintScreen：小键盘 `*` 的键码加扩展位。
	PrintScreen = (0x37, true),
	/// Pause：与 ScrollLock 同码，靠扩展位与特殊处理区分。
	Pause = (0x46, true),
	/// 菜单键（Application）。Windows 上等于右键菜单。
	Menu = (0x5D, true),

	// ---- 小键盘。注意 NumLock 关闭时它们输出导航键，那由远端处理 ----
	Numpad0 = (0x52, false),
	Numpad1 = (0x4F, false),
	Numpad2 = (0x50, false),
	Numpad3 = (0x51, false),
	Numpad4 = (0x4B, false),
	Numpad5 = (0x4C, false),
	Numpad6 = (0x4D, false),
	Numpad7 = (0x47, false),
	Numpad8 = (0x48, false),
	Numpad9 = (0x49, false),
	NumpadDecimal = (0x53, false),
	NumpadDivide = (0x35, true),
	/// 小键盘 `*` 与 PrintScreen 同码，扩展位不同。
	NumpadMultiply = (0x37, false),
	NumpadSubtract = (0x4A, false),
	NumpadAdd = (0x4E, false),
	/// 小键盘回车与主回车同码，扩展位不同。
	NumpadEnter = (0x1C, true),
}

/// 鼠标按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
	/// 左键。
	Left,
	/// 右键。
	Right,
	/// 中键（滚轮按下）。
	Middle,
}

impl PointerButton {
	/// 该按键对应的标志位。
	const fn flag(self) -> u16 {
		match self {
			Self::Left => ptr_flags::BUTTON1,
			Self::Right => ptr_flags::BUTTON2,
			Self::Middle => ptr_flags::BUTTON3,
		}
	}
}

/// 滚轮方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WheelAxis {
	/// 垂直（普通滚轮）。
	Vertical,
	/// 水平（侧滚 / 触控板横滑）。
	Horizontal,
}

/// 算滚轮事件的标志位。
///
/// 编码沿用 Windows 的约定：低 9 位放**补码**，为负时还要额外带上
/// `PTR_FLAGS_WHEEL_NEGATIVE`。`delta` 以 [`WHEEL_DELTA`] 为单位，
/// 向上 / 向右为正。
///
/// `delta` 为 0 时只返回轴向位（没有位移就不该发滚轮事件，调用方会跳过）；
/// 绝对值超过 9 位可表示的范围时截断——真实滚轮一格就是 120，够用。
pub fn wheel_flags(axis: WheelAxis, delta: i16) -> u16 {
	let axis_flag = match axis {
		WheelAxis::Vertical => ptr_flags::WHEEL,
		WheelAxis::Horizontal => ptr_flags::HWHEEL,
	};

	if delta == 0 {
		return axis_flag;
	}

	let magnitude = delta.unsigned_abs().min(ptr_flags::WHEEL_ROTATION_MASK);
	if delta < 0 {
		// 补码：0x200 - |delta|
		axis_flag | ptr_flags::WHEEL_NEGATIVE | (0x0200u16 - magnitude)
	} else {
		axis_flag | magnitude
	}
}

/// 一次输入动作。这是 UI 线程发给 RDP 线程的唯一载荷。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
	/// 键按下或抬起。
	Key {
		/// 哪个键。
		key: Key,
		/// 按下为 `true`，抬起为 `false`。
		down: bool,
		/// 是否是长按产生的自动重复。
		repeat: bool,
	},
	/// 指针移动到桌面坐标。
	PointerMove {
		/// 桌面 x（像素）。
		x: u16,
		/// 桌面 y（像素）。
		y: u16,
	},
	/// 鼠标按键按下或抬起。带上当时的坐标——远端据此决定点在哪里。
	PointerButton {
		/// 哪个键。
		button: PointerButton,
		/// 按下为 `true`。
		down: bool,
		/// 桌面 x（像素）。
		x: u16,
		/// 桌面 y（像素）。
		y: u16,
	},
	/// 滚轮。
	Wheel {
		/// 哪个轴。
		axis: WheelAxis,
		/// 位移格数，向上 / 向右为正。
		delta: i16,
		/// 桌面 x（像素）。
		x: u16,
		/// 桌面 y（像素）。
		y: u16,
	},
}

impl InputEvent {
	/// 组装 `freerdp_input_send_mouse_event` 的 `flags`。
	///
	/// 键盘事件不走这里（它用 `..._keyboard_event_ex`），返回 `None`。
	pub fn mouse_flags(self) -> Option<u16> {
		match self {
			Self::Key { .. } => None,
			Self::PointerMove { .. } => Some(ptr_flags::MOVE),
			Self::PointerButton { button, down, .. } => {
				let mut flags = button.flag();
				if down {
					flags |= ptr_flags::DOWN;
				}
				Some(flags)
			},
			Self::Wheel { axis, delta, .. } => Some(wheel_flags(axis, delta)),
		}
	}

	/// 事件携带的桌面坐标。键盘事件没有，返回 `None`。
	pub fn position(self) -> Option<(u16, u16)> {
		match self {
			Self::Key { .. } => None,
			Self::PointerMove { x, y }
			| Self::PointerButton { x, y, .. }
			| Self::Wheel { x, y, .. } => Some((x, y)),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// 抽查字母表里的首 / 中 / 尾三颗，确认没有整段错位。
	#[test]
	fn letter_scancodes() {
		assert_eq!(Key::A.scancode(), Scancode::new(0x1E, false));
		assert_eq!(Key::M.scancode(), Scancode::new(0x32, false));
		assert_eq!(Key::Z.scancode(), Scancode::new(0x2C, false));
	}

	/// 数字 1 在 0x02、0 在 0x0B——顺序与直觉相反（1..9 然后才是 0），
	/// 写表时最容易串行。
	#[test]
	fn digit_scancodes() {
		assert_eq!(Key::Digit1.scancode().code(), 0x02);
		assert_eq!(Key::Digit9.scancode().code(), 0x0A);
		assert_eq!(Key::Digit0.scancode().code(), 0x0B);
	}

	/// 「同码不同扩展位」是这套编码的核心，挑三组最容易混的钉住。
	#[test]
	fn extended_bit_distinguishes_same_code_keys() {
		// 左右 Ctrl
		assert_eq!(
			Key::ControlLeft.scancode().code(),
			Key::ControlRight.scancode().code()
		);
		assert!(!Key::ControlLeft.scancode().is_extended());
		assert!(Key::ControlRight.scancode().is_extended());

		// 主回车 vs 小键盘回车
		assert_eq!(
			Key::Enter.scancode().code(),
			Key::NumpadEnter.scancode().code()
		);
		assert!(!Key::Enter.scancode().is_extended());
		assert!(Key::NumpadEnter.scancode().is_extended());

		// 小键盘 * vs PrintScreen
		assert_eq!(
			Key::NumpadMultiply.scancode().code(),
			Key::PrintScreen.scancode().code()
		);
		assert!(!Key::NumpadMultiply.scancode().is_extended());
		assert!(Key::PrintScreen.scancode().is_extended());
	}

	/// 方向键 / 导航键整组都必须是扩展键——漏了扩展位远端会收到小键盘数字。
	#[test]
	fn navigation_keys_are_all_extended() {
		for key in [
			Key::ArrowUp,
			Key::ArrowDown,
			Key::ArrowLeft,
			Key::ArrowRight,
			Key::Home,
			Key::End,
			Key::PageUp,
			Key::PageDown,
			Key::Insert,
			Key::Delete,
		] {
			assert!(key.scancode().is_extended(), "{key:?} 应为扩展键");
		}
	}

	/// 键码必须落在低 8 位内，否则会污染扩展位。
	#[test]
	fn all_keycodes_fit_in_one_byte() {
		for &key in Key::ALL {
			// 反过来解一遍：只留低 8 位应当还原
			let scancode = key.scancode();
			assert_eq!(
				Scancode::new(scancode.code(), scancode.is_extended()),
				scancode,
				"{key:?} 的扫描码超出 8 位"
			);
		}
	}

	/// `rdp_scancode` 参数就是「低 8 位 | 扩展位」。
	#[test]
	fn scancode_encodes_extend_bit() {
		assert_eq!(Key::A.scancode().as_u32(), 0x1E);
		assert_eq!(Key::ArrowUp.scancode().as_u32(), 0x0148);
		assert_eq!(KBDEXT, kbd_flags::EXTENDED, "KBDEXT 就是键盘标志里的扩展位");
	}

	#[test]
	fn wheel_delta_is_windows_constant() {
		assert_eq!(WHEEL_DELTA, 120);
	}

	#[test]
	fn wheel_flags_positive_and_negative() {
		// 上滚一格
		assert_eq!(wheel_flags(WheelAxis::Vertical, 120), 0x0200 | 120);
		// 下滚一格：补码 0x200 - 120
		assert_eq!(
			wheel_flags(WheelAxis::Vertical, -120),
			0x0200 | 0x0100 | (0x0200 - 120)
		);
		// 补码低 9 位为 0x188
		assert_eq!(wheel_flags(WheelAxis::Vertical, -120) & 0x01FF, 0x188);
	}

	#[test]
	fn wheel_flags_horizontal_uses_hwheel_bit() {
		assert_eq!(wheel_flags(WheelAxis::Horizontal, 120), 0x0400 | 120);
	}

	#[test]
	fn wheel_flags_zero_has_no_rotation() {
		// 没有位移就不该带旋转量——调用方本来就该跳过这种事件
		for axis in [WheelAxis::Vertical, WheelAxis::Horizontal] {
			assert_eq!(wheel_flags(axis, 0) & 0x01FF, 0);
		}
	}

	#[test]
	fn wheel_flags_clamps_oversized_delta() {
		// 9 位放不下，截断到最大值而不是溢出到别的位
		let flags = wheel_flags(WheelAxis::Vertical, i16::MAX);
		assert_eq!(flags & 0x01FF, 0x01FF);
		// 轴向位仍然完好
		assert_eq!(flags & 0x0200, 0x0200);
	}

	#[test]
	fn mouse_flags_move_and_buttons() {
		let event = InputEvent::PointerMove { x: 1, y: 2 };
		assert_eq!(event.mouse_flags(), Some(0x0800));

		let down = InputEvent::PointerButton {
			button: PointerButton::Left,
			down: true,
			x: 1,
			y: 2,
		};
		assert_eq!(down.mouse_flags(), Some(0x1000 | 0x8000));

		let up = InputEvent::PointerButton {
			button: PointerButton::Right,
			down: false,
			x: 1,
			y: 2,
		};
		// 抬起不带 DOWN 位
		assert_eq!(up.mouse_flags(), Some(0x2000));
	}

	#[test]
	fn keyboard_event_has_no_mouse_flags_or_position() {
		let event = InputEvent::Key {
			key: Key::A,
			down: true,
			repeat: false,
		};
		assert_eq!(event.mouse_flags(), None);
		assert_eq!(event.position(), None);
	}

	/// 带坐标的三种事件都要能取出坐标——坐标算错的话点击会落在别处。
	#[test]
	fn pointer_events_carry_position() {
		let cases = [
			InputEvent::PointerMove { x: 7, y: 9 },
			InputEvent::PointerButton {
				button: PointerButton::Middle,
				down: true,
				x: 7,
				y: 9,
			},
			InputEvent::Wheel {
				axis: WheelAxis::Vertical,
				delta: 120,
				x: 7,
				y: 9,
			},
		];
		for event in cases {
			assert_eq!(event.position(), Some((7, 9)), "{event:?}");
		}
	}
}
