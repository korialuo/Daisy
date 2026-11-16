//! Daisy 终端层。
//!
//! - [`link`] —— 字节链路抽象。本地 PTY 与远程 SSH 通道实现同一 trait，
//!   网格与渲染层因此完全复用
//! - [`pty`] —— 走 `portable-pty` 统一 ConPTY（Windows）与 forkpty（Unix），
//!   不直接调用 POSIX API
//! - [`term`] —— 基于 `alacritty_terminal` 维护单元格网格，并导出与解析器无关的快照
//! - [`palette`] —— 把语义颜色解析成实际颜色，索引色走 xterm 标准表
//! - [`input`] —— 把 egui 的键盘事件编码成终端字节序列
//! - [`render`] —— egui 绘制后端，按同样式连续段合并，避免逐格布局
//! - [`session`] —— 把链路、网格、输入编码串成一个可直接用的会话

pub mod error;
pub mod input;
pub mod link;
pub mod palette;
pub mod pty;
pub mod render;
pub mod session;
pub mod term;

pub use error::{Result, TermError};
pub use link::{LinkEvent, SessionLink, Waker, WakerSlot};
pub use palette::{Palette, xterm_256};
pub use pty::{PtyCommand, PtySession, TERM_VALUE};
pub use render::{CellMetrics, TerminalRenderer};
pub use session::{TerminalSession, TerminalUpdate};
pub use term::{
	CellFlags, CellSnapshot, ClipboardResponder, CursorShape, CursorState, DEFAULT_SCROLLBACK,
	GridSnapshot, SelectionBounds, TermColor, TermEvent, TerminalGrid,
};
