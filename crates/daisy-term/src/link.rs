//! 会话字节链路的抽象。
//!
//! [`TerminalSession`](crate::session::TerminalSession) 不关心字节从哪来：
//! 本地 PTY 与远程 SSH 通道实现同一个 trait，网格、渲染、输入编码全部复用。
//!
//! ```text
//!   PtySession ─────────┐
//!                       ├─▶ dyn SessionLink ─▶ TerminalGrid ─▶ 渲染层
//!   RemoteShell（SSH）──┘
//! ```
//!
//! ## 为什么 trait 里是同步接口
//!
//! 界面每帧拉一次数据（`try_recv`），不能在这里 await。远程链路的异步性由
//! 实现方在内部消化——`daisy-ssh` 用一个后台任务把异步通道喂进同步队列。

use std::sync::Arc;

use crate::error::Result;

/// 链路上产生的异步事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
	/// 对端产生了一段输出。
	///
	/// **不保证按字符边界切分**，使用方需要交给 ANSI 解析器去拼。
	Output(Vec<u8>),
	/// 会话结束，附带退出码。
	Exited(i32),
}

/// 「有新数据了，醒一下」的唤醒回调。
///
/// 由链路自己持有的后台线程（PTY 读取线程、SSH 搬运任务）在送出一批数据后调用。
/// 界面层把它接到 `egui::Context::request_repaint` 上，输出就能**立刻**上屏，
/// 而不是等下一次周期性重绘。
///
/// 实现约束：**会被高频调用**（远端刷屏时一秒上千次），必须廉价且线程安全。
/// 去重是调用方的责任（见 `daisy-app` 里的 `pending` 标志）。
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// 唤醒回调的共享槽。
///
/// 会话总是「先建链路、后装回调」（界面层要等视图建好才拿到 `egui::Context`），
/// 所以带后台线程的链路先带着一个空槽跑起来，之后由 `set_waker` 填进去。
///
/// 把「取出再调用」收在这里是为了守住锁纪律：锁只用于克隆 `Arc`，
/// **绝不在锁里跑回调**——回调会去叫醒事件循环，万一重入就会自锁。
#[derive(Clone, Default)]
pub struct WakerSlot(Arc<std::sync::Mutex<Option<Waker>>>);

impl WakerSlot {
	/// 装入（或替换）回调。
	pub fn set(&self, waker: Waker) {
		*self
			.0
			.lock()
			.unwrap_or_else(std::sync::PoisonError::into_inner) = Some(waker);
	}

	/// 叫醒一次。槽里没有回调时是空操作。
	pub fn wake(&self) {
		let waker = match self.0.lock() {
			Ok(guard) => guard.clone(),
			Err(poisoned) => poisoned.into_inner().clone(),
		};
		if let Some(waker) = waker {
			waker();
		}
	}
}

impl std::fmt::Debug for WakerSlot {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let installed = self.0.lock().is_ok_and(|slot| slot.is_some());
		f.debug_struct("WakerSlot")
			.field("installed", &installed)
			.finish()
	}
}

/// 一个终端会话的字节链路。
///
/// 实现方需保证 `try_recv` 非阻塞：没有事件就返回 `None`。
pub trait SessionLink: Send {
	/// 取一个待处理事件。没有就绪事件时返回 `None`。
	fn try_recv(&mut self) -> Option<LinkEvent>;

	/// 写入字节（键盘输入、粘贴、终端应答）。
	fn write(&mut self, data: &[u8]) -> Result<()>;

	/// 通知对端尺寸变化。
	fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;

	/// 主动结束会话。会话本已结束时返回 `Ok(false)`。
	fn shutdown(&mut self) -> Result<bool>;

	/// 注册「有新数据」的唤醒回调。
	///
	/// 默认空实现：批处理、测试等不关心重绘的调用方不必理会。
	/// 有后台线程的链路（PTY / SSH）**应当**实现它——否则界面只能靠
	/// 其它输入事件把帧叫醒，远端输出会明显滞后。
	fn set_waker(&mut self, _waker: Waker) {}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::error::TermError;

	/// 最小实现，用来验证 trait 可以装箱、可以被测试替身满足。
	struct FakeLink {
		pending: Vec<LinkEvent>,
		written: Vec<u8>,
		size: (u16, u16),
		closed: bool,
	}

	impl SessionLink for FakeLink {
		fn try_recv(&mut self) -> Option<LinkEvent> {
			if self.pending.is_empty() {
				None
			} else {
				Some(self.pending.remove(0))
			}
		}

		fn write(&mut self, data: &[u8]) -> Result<()> {
			self.written.extend_from_slice(data);
			Ok(())
		}

		fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
			if cols == 0 || rows == 0 {
				return Err(TermError::InvalidSize("尺寸不能为 0".to_string()));
			}
			self.size = (cols, rows);
			Ok(())
		}

		fn shutdown(&mut self) -> Result<bool> {
			let was_open = !self.closed;
			self.closed = true;
			Ok(was_open)
		}
	}

	#[test]
	fn link_is_object_safe() {
		let mut link: Box<dyn SessionLink> = Box::new(FakeLink {
			pending: vec![LinkEvent::Output(b"hello".to_vec()), LinkEvent::Exited(0)],
			written: Vec::new(),
			size: (80, 24),
			closed: false,
		});

		assert_eq!(link.try_recv(), Some(LinkEvent::Output(b"hello".to_vec())));
		assert_eq!(link.try_recv(), Some(LinkEvent::Exited(0)));
		assert_eq!(link.try_recv(), None, "取空后应返回 None 而不是阻塞");

		link.write(b"x").expect("写入失败");
		link.resize(120, 40).expect("resize 失败");
		assert!(link.resize(0, 40).is_err(), "0 列应被拒绝");

		assert!(link.shutdown().expect("关闭失败"));
		assert!(
			!link.shutdown().expect("重复关闭不应报错"),
			"第二次应为 false"
		);
	}

	#[test]
	fn fake_link_records_writes() {
		let mut link = FakeLink {
			pending: Vec::new(),
			written: Vec::new(),
			size: (80, 24),
			closed: false,
		};

		link.write(b"abc").expect("写入失败");
		link.write(b"def").expect("写入失败");
		assert_eq!(link.written, b"abcdef");
		assert_eq!(link.size, (80, 24), "未 resize 时尺寸不应变化");
	}
}
