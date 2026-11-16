//! 终端会话：把字节链路、网格、输入编码串成一个整体。
//!
//! ```text
//!   SessionLink ──输出──▶ TerminalGrid ──快照──▶ 渲染层
//!        ▲                    │
//!        └──── 输入编码 ◀──── 键盘事件（egui）
//! ```
//!
//! 链路可以是本地 PTY（[`TerminalSession::local_shell`]）或远程 SSH 通道
//! （[`TerminalSession::attach`]），网格与渲染层完全复用。
//!
//! 界面层只需要持有一个 [`TerminalSession`]：每帧调一次 [`TerminalSession::pump`]，
//! 再调 [`TerminalSession::snapshot`] 拿内容去画。

use alacritty_terminal::term::TermMode;
use egui::Event;

use crate::error::Result;
use crate::input;
use crate::link::{LinkEvent, SessionLink, Waker};
use crate::pty::{PtyCommand, PtySession};
use crate::term::{ClipboardResponder, DEFAULT_SCROLLBACK, GridSnapshot, TermEvent, TerminalGrid};

/// 一次 [`TerminalSession::pump`] 的结果。
#[derive(Debug, Default)]
pub struct TerminalUpdate {
	/// 终端内容是否有变化。为 `false` 时界面层可以不重建快照。
	pub changed: bool,
	/// 窗口标题变化。
	pub title: Option<String>,
	/// 子进程退出码。
	pub exited: Option<i32>,
	/// 收到响铃。
	pub bell: bool,
	/// 终端要求把文本写入系统剪贴板（`OSC 52`）。
	pub clipboard_store: Option<String>,
	/// 终端要求读取系统剪贴板。调用方需用 [`ClipboardResponder`] 回填。
	pub clipboard_load: Option<ClipboardResponder>,
}

impl TerminalUpdate {
	/// 是否产生了需要上层立刻处理的事件（不含内容变化）。
	#[must_use]
	pub fn has_notification(&self) -> bool {
		self.title.is_some()
			|| self.exited.is_some()
			|| self.bell
			|| self.clipboard_store.is_some()
			|| self.clipboard_load.is_some()
	}
}

/// 一个终端会话。
pub struct TerminalSession {
	link: Box<dyn SessionLink>,
	grid: TerminalGrid,
	title: Option<String>,
	exit_code: Option<i32>,
	exited_announced: bool,
	/// 输入法组合状态，跨帧保留（见 [`input::ImeState`]）。
	ime: input::ImeState,
}

impl TerminalSession {
	/// 拉起一个终端会话。
	pub fn spawn(command: &PtyCommand) -> Result<Self> {
		Self::spawn_with_scrollback(command, DEFAULT_SCROLLBACK)
	}

	/// 拉起会话并指定回滚行数。
	pub fn spawn_with_scrollback(command: &PtyCommand, scrollback: usize) -> Result<Self> {
		let pty = PtySession::spawn(command)?;
		Self::attach(Box::new(pty), command.cols, command.rows, scrollback)
	}

	/// 挂到一个既有的字节链路上。
	///
	/// SSH 远端 shell 走这条路：链路由 `daisy-ssh` 提供，
	/// 网格、渲染、输入编码全部复用本地终端的实现。
	pub fn attach(
		link: Box<dyn SessionLink>,
		cols: u16,
		rows: u16,
		scrollback: usize,
	) -> Result<Self> {
		let grid = TerminalGrid::new(cols, rows, scrollback)?;

		Ok(Self {
			link,
			grid,
			title: None,
			exit_code: None,
			exited_announced: false,
			ime: input::ImeState::default(),
		})
	}

	/// 拉出一个执行本地 shell 的会话。
	pub fn local_shell(cols: u16, rows: u16) -> Result<Self> {
		Self::spawn(&PtyCommand::local_shell().size(cols, rows))
	}

	/// 注册「有新数据」的唤醒回调，转交给链路。
	///
	/// 界面层建好会话后调用一次即可：远端输出到达时链路会直接叫醒界面，
	/// 不必等下一次周期性重绘。
	pub fn set_waker(&mut self, waker: Waker) {
		self.link.set_waker(waker);
	}

	/// 每帧调用一次：抽取链路输出喂给网格，并处理网格回传的事件。
	pub fn pump(&mut self) -> TerminalUpdate {
		let mut update = TerminalUpdate::default();

		// 1) 链路 → 网格
		while let Some(event) = self.link.try_recv() {
			match event {
				LinkEvent::Output(chunk) => {
					self.grid.feed(&chunk);
					update.changed = true;
				},
				LinkEvent::Exited(code) => {
					self.exit_code = Some(code);
					// 退出通知只报一次，避免界面层反复弹提示
					if !self.exited_announced {
						self.exited_announced = true;
						update.exited = Some(code);
					}
					update.changed = true;
				},
			}
		}

		// 2) 网格 → 上层
		for event in self.grid.drain_events() {
			match event {
				TermEvent::Title(title) => {
					self.title = Some(title.clone());
					update.title = Some(title);
				},
				TermEvent::ResetTitle => {
					self.title = None;
				},
				TermEvent::Bell => update.bell = true,
				TermEvent::PtyWrite(bytes) => {
					// 终端对查询的应答必须写回去，否则 TUI 会一直等
					if let Err(error) = self.link.write(&bytes) {
						tracing::warn!("终端应答写回失败：{error}");
					}
				},
				TermEvent::ClipboardStore(text) => update.clipboard_store = Some(text),
				TermEvent::ClipboardLoad(responder) => update.clipboard_load = Some(responder),
				// 光标闪烁与子进程退出由 pump 自己处理，这里不需要动作
				TermEvent::CursorBlinkingChange | TermEvent::ChildExit(_) => {},
			}
		}

		update
	}

	/// 把 egui 的键盘事件编码后写进 PTY。
	///
	/// 返回实际写入的字节数，便于界面层判断是否要消费这些事件。
	pub fn handle_key_events(&mut self, events: &[Event]) -> Result<usize> {
		let bytes = self.ime.encode(events, self.mode());
		if bytes.is_empty() {
			return Ok(0);
		}
		self.write(&bytes)?;
		Ok(bytes.len())
	}

	/// 直接写入字节。
	pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
		self.link.write(bytes)
	}

	/// 粘贴文本。终端处于括号粘贴模式时会自动加标记。
	pub fn paste(&mut self, text: &str) -> Result<()> {
		let events = [Event::Paste(text.to_owned())];
		let bytes = input::encode(&events, self.mode());
		self.write(&bytes)
	}

	/// 把一段**本地生成**的字节喂给网格，**不发给对端**。
	///
	/// 用来把客户端自己的提示写进终端画面，比如「连接已断开」。
	/// 走网格而不是画在面板上：这样提示会跟着内容一起滚动，
	/// 「空一行」也可以直接由 `\r\n` 形成，不必去算最后一行在哪、
	/// 屏幕满了又该怎么挤。
	pub fn inject(&mut self, bytes: &[u8]) {
		self.grid.feed(bytes);
	}

	/// 调整尺寸，同步到网格与对端。
	pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
		if (cols, rows) == (self.cols(), self.rows()) {
			return Ok(());
		}

		self.grid.resize(cols, rows);
		self.link.resize(cols, rows)?;
		Ok(())
	}

	/// 当前列数。
	#[must_use]
	pub fn cols(&self) -> u16 {
		self.grid.size().0 as u16
	}

	/// 当前行数。
	#[must_use]
	pub fn rows(&self) -> u16 {
		self.grid.size().1 as u16
	}

	/// 导出当前视口快照。
	#[must_use]
	pub fn snapshot(&self) -> GridSnapshot {
		self.grid.snapshot()
	}

	/// 只读访问网格，用于选区等操作。
	#[must_use]
	pub fn grid(&self) -> &TerminalGrid {
		&self.grid
	}

	/// 可写访问网格。
	pub fn grid_mut(&mut self) -> &mut TerminalGrid {
		&mut self.grid
	}

	/// 当前终端模式。
	#[must_use]
	pub fn mode(&self) -> TermMode {
		self.grid.mode()
	}

	/// 回滚。
	pub fn scroll(&mut self, lines: i32) {
		self.grid.scroll(lines);
	}

	/// 回到底部。
	pub fn scroll_to_bottom(&mut self) {
		self.grid.scroll_to_bottom();
	}

	/// 当前回滚偏移。
	#[must_use]
	pub fn display_offset(&self) -> usize {
		self.grid.display_offset()
	}

	/// 当前标题。
	#[must_use]
	pub fn title(&self) -> Option<&str> {
		self.title.as_deref()
	}

	/// 子进程是否已退出。
	#[must_use]
	pub fn is_exited(&self) -> bool {
		self.exit_code.is_some()
	}

	/// 退出码。
	#[must_use]
	pub fn exit_code(&self) -> Option<i32> {
		self.exit_code
	}

	/// 结束会话。
	pub fn kill(&mut self) -> Result<bool> {
		self.link.shutdown()
	}

	/// 需要重绘时会返回 `true`——用于决定是否请求下一帧。
	///
	/// 光标闪烁期间必须持续重绘，否则光标不会闪。
	#[must_use]
	pub fn needs_animation(&self) -> bool {
		!self.is_exited() && self.grid.mode().contains(TermMode::SHOW_CURSOR)
	}
}

impl std::fmt::Debug for TerminalSession {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("TerminalSession")
			.field("cols", &self.cols())
			.field("rows", &self.rows())
			.field("title", &self.title)
			.field("exit_code", &self.exit_code)
			.field("display_offset", &self.display_offset())
			.finish_non_exhaustive()
	}
}

impl Drop for TerminalSession {
	fn drop(&mut self) {
		// 链路的 Drop 会收掉子进程或关闭远端通道，
		// 这里只需要确保退出码状态一致
		if self.exit_code.is_none()
			&& let Err(error) = self.link.shutdown()
		{
			tracing::debug!("结束终端会话时出错：{error}");
		}
	}
}

#[cfg(test)]
// 同 pty.rs：`pump_until` 只被 Unix 专用用例调用，Windows 下「没人引用」
// 不代表它是死代码，别让它污染 Windows 交叉检查的零警告门。
#[cfg_attr(windows, allow(dead_code, unused_imports))]
mod tests {
	use super::*;
	use std::time::{Duration, Instant};

	#[cfg(unix)]
	fn shell(script: &str) -> PtyCommand {
		PtyCommand::new("/bin/sh")
			.arg("-c")
			.arg(script)
			.size(80, 24)
	}

	/// 泵到出现关键字或超时为止。
	fn pump_until(session: &mut TerminalSession, needle: &str, timeout: Duration) -> String {
		let deadline = Instant::now() + timeout;
		while Instant::now() < deadline {
			session.pump();
			let text = screen_text(session);
			if text.contains(needle) {
				return text;
			}
			std::thread::sleep(Duration::from_millis(10));
		}
		screen_text(session)
	}

	fn screen_text(session: &TerminalSession) -> String {
		let snapshot = session.snapshot();
		let mut out = String::new();
		for row in 0..snapshot.rows {
			for col in 0..snapshot.cols {
				if let Some(cell) = snapshot.cell(row, col)
					&& !cell.flags.wide_spacer
				{
					out.push(cell.ch);
				}
			}
			out.push('\n');
		}
		out
	}

	#[cfg(unix)]
	#[test]
	fn output_reaches_grid() {
		let mut session = TerminalSession::spawn(&shell("printf 'session-ok'")).expect("启动失败");

		let text = pump_until(&mut session, "session-ok", Duration::from_secs(5));
		assert!(text.contains("session-ok"), "实际屏幕内容：{text}");
	}

	#[cfg(unix)]
	#[test]
	fn exit_is_reported_once() {
		let mut session = TerminalSession::spawn(&shell("exit 3")).expect("启动失败");

		let deadline = Instant::now() + Duration::from_secs(5);
		let mut exit_events = 0;
		while Instant::now() < deadline {
			let update = session.pump();
			if update.exited.is_some() {
				exit_events += 1;
			}
			if session.is_exited() {
				break;
			}
			std::thread::sleep(Duration::from_millis(10));
		}

		assert_eq!(session.exit_code(), Some(3));
		assert_eq!(exit_events, 1, "退出通知应只上报一次");
	}

	#[cfg(unix)]
	#[test]
	fn keystrokes_are_forwarded() {
		// cat 会把输入回写，用它验证 编码 → PTY → 网格 的完整回路
		let mut session = TerminalSession::spawn(&PtyCommand::new("/bin/cat")).expect("启动失败");

		let events = vec![Event::Text("round".to_string())];
		session.handle_key_events(&events).expect("写入失败");

		let text = pump_until(&mut session, "round", Duration::from_secs(5));
		assert!(text.contains("round"), "实际屏幕内容：{text}");

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn enter_key_produces_newline() {
		let mut session = TerminalSession::spawn(&PtyCommand::new("/bin/cat")).expect("启动失败");

		let events = vec![
			Event::Text("a".to_string()),
			Event::Key {
				key: egui::Key::Enter,
				physical_key: None,
				pressed: true,
				repeat: false,
				modifiers: egui::Modifiers::default(),
			},
			Event::Text("b".to_string()),
		];
		session.handle_key_events(&events).expect("写入失败");

		let text = pump_until(&mut session, "b", Duration::from_secs(5));
		let lines: Vec<&str> = text
			.lines()
			.filter(|line| !line.trim().is_empty())
			.collect();
		assert!(lines.len() >= 2, "回车后应换行，实际：{text:?}");

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn paste_goes_through() {
		let mut session = TerminalSession::spawn(&PtyCommand::new("/bin/cat")).expect("启动失败");

		session.paste("pasted-text").expect("粘贴失败");
		let text = pump_until(&mut session, "pasted-text", Duration::from_secs(5));
		assert!(text.contains("pasted-text"), "实际屏幕内容：{text}");

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn resize_updates_both_sides() {
		let mut session = TerminalSession::spawn(&shell("sleep 5")).expect("启动失败");

		assert_eq!((session.cols(), session.rows()), (80, 24));
		session.resize(120, 40).expect("resize 失败");
		assert_eq!((session.cols(), session.rows()), (120, 40));
		assert_eq!(session.snapshot().cols, 120);

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn identical_resize_is_noop() {
		let mut session = TerminalSession::spawn(&shell("sleep 5")).expect("启动失败");
		session.resize(80, 24).expect("相同尺寸不应报错");
		assert_eq!((session.cols(), session.rows()), (80, 24));

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn title_event_updates_title() {
		let mut session =
			TerminalSession::spawn(&shell("printf '\\033]0;session-title\\007'; sleep 1"))
				.expect("启动失败");

		let deadline = Instant::now() + Duration::from_secs(5);
		while Instant::now() < deadline && session.title().is_none() {
			session.pump();
			std::thread::sleep(Duration::from_millis(10));
		}

		assert_eq!(session.title(), Some("session-title"));

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn scrollback_works_through_session() {
		let mut session = TerminalSession::spawn(&shell(
			"i=0; while [ $i -lt 60 ]; do echo line$i; i=$((i+1)); done; sleep 2",
		))
		.expect("启动失败");

		pump_until(&mut session, "line59", Duration::from_secs(5));

		assert_eq!(session.display_offset(), 0);
		session.scroll(10);
		assert!(session.display_offset() > 0, "回滚后偏移应大于 0");

		session.scroll_to_bottom();
		assert_eq!(session.display_offset(), 0);

		session.kill().expect("结束失败");
	}

	#[test]
	fn update_notification_flag() {
		let quiet = TerminalUpdate::default();
		assert!(!quiet.has_notification());

		let bell = TerminalUpdate {
			bell: true,
			..TerminalUpdate::default()
		};
		assert!(bell.has_notification());
	}

	/// 用测试替身充当链路，证明会话不依赖 PTY。
	///
	/// SSH 远端会话走的就是这条路：链路由 `daisy-ssh` 提供。
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
	fn attach_works_without_a_pty() {
		let mut session = TerminalSession::attach(
			Box::new(FakeLink {
				pending: vec![
					LinkEvent::Output(b"remote-output".to_vec()),
					LinkEvent::Exited(7),
				],
				written: Vec::new(),
				size: (80, 24),
				closed: false,
			}),
			80,
			24,
			DEFAULT_SCROLLBACK,
		)
		.expect("挂载失败");

		let update = session.pump();
		assert!(update.changed, "有输出时 changed 应为 true");
		assert_eq!(update.exited, Some(7), "退出码应上报");

		let text = screen_text(&session);
		assert!(text.contains("remote-output"), "实际屏幕内容：{text}");

		// 第二次 pump 不应重复上报退出
		let again = session.pump();
		assert_eq!(again.exited, None, "退出通知应只上报一次");
		assert_eq!(session.exit_code(), Some(7));
	}

	#[test]
	fn attach_forwards_input_and_resize() {
		let mut session = TerminalSession::attach(
			Box::new(FakeLink {
				pending: Vec::new(),
				written: Vec::new(),
				size: (80, 24),
				closed: false,
			}),
			80,
			24,
			DEFAULT_SCROLLBACK,
		)
		.expect("挂载失败");

		let events = vec![Event::Text("ssh".to_string())];
		let written = session.handle_key_events(&events).expect("写入失败");
		assert_eq!(written, 3, "应写入 3 个字节");

		session.resize(120, 40).expect("resize 失败");
		assert_eq!((session.cols(), session.rows()), (120, 40));
		assert_eq!(session.snapshot().cols, 120);

		assert!(session.kill().expect("关闭失败"));
	}

	#[cfg(unix)]
	#[test]
	fn colors_and_cjk_survive_the_whole_chain() {
		use crate::term::TermColor;

		// 命名色 + 真彩色 + 中文，覆盖 解析 → 网格 → 快照 的完整链路
		let script = "printf '\\033[31mR\\033[0m\\033[38;2;18;52;86mT\\033[0m中'";
		let mut session = TerminalSession::spawn(&shell(script)).expect("启动失败");

		// printf 没有换行，等到屏幕上出现中文即可
		let deadline = Instant::now() + Duration::from_secs(5);
		while Instant::now() < deadline {
			session.pump();
			if screen_text(&session).contains('中') {
				break;
			}
			std::thread::sleep(Duration::from_millis(10));
		}

		let snapshot = session.snapshot();
		let named = snapshot.cell(0, 0).expect("应有第 0 格");
		let truecolor = snapshot.cell(0, 1).expect("应有第 1 格");
		let wide = snapshot.cell(0, 2).expect("应有第 2 格");
		let spacer = snapshot.cell(0, 3).expect("应有第 3 格");

		assert_eq!(named.ch, 'R');
		assert_eq!(
			named.fg,
			TermColor::Indexed(1),
			"\\033[31m 应是命名色 1（红）"
		);

		assert_eq!(truecolor.ch, 'T');
		assert_eq!(truecolor.fg, TermColor::Rgb(18, 52, 86), "真彩色应原样保留");

		assert_eq!(wide.ch, '中', "中文应进入网格");
		assert!(spacer.flags.wide_spacer, "中文占两列，第二列应是占位格");
	}
}
