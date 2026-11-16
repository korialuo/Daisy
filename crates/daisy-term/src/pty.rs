//! 本地 PTY 会话。
//!
//! 职责：拉起一个伪终端里的子进程，提供双向字节流与尺寸调整。
//!
//! ## 为什么用 `portable-pty` 而不是直接 `forkpty`
//!
//! Windows 上没有 fork，用的是 ConPTY，两套 API 完全不同。
//! `portable-pty` 把差异收在内部，因此本文件**不需要任何平台判断**
//! （除 `local_shell()` 里选默认 shell 这一处），符合 PLAN.md 8.1 的 D3、D5。
//!
//! ## 线程模型
//!
//! ```text
//!  ┌──────────────┐  read   ┌──────────────┐  try_recv  ┌────────┐
//!  │ PTY master   │ ──────▶ │ 读取线程      │ ─────────▶ │ 事件队列│ ──▶ UI
//!  └──────────────┘         └──────────────┘            └────────┘
//!          ▲
//!          │ write
//!    PtySession::write()
//! ```
//!
//! 只有一个后台线程（读取）。子进程退出状态由 [`PtySession::try_recv`] 在
//! 读取线程收尾后通过 `try_wait` 收割，不额外占一个线程等 `wait()`。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::{self, JoinHandle};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::{Result, TermError};
use crate::link::{LinkEvent, SessionLink, Waker, WakerSlot};

/// 单次读取的缓冲大小。
const READ_BUFFER_SIZE: usize = 8192;

/// 终端默认列数。
pub const DEFAULT_COLS: u16 = 80;

/// 终端默认行数。
pub const DEFAULT_ROWS: u16 = 24;

/// 传给子进程的 `TERM` 值。少了它，绝大多数程序不会输出颜色。
pub const TERM_VALUE: &str = "xterm-256color";

/// 要拉起的终端命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyCommand {
	/// 可执行文件路径。
	pub program: String,
	/// 参数。
	pub args: Vec<String>,
	/// 工作目录。`None` 表示继承当前目录。
	pub cwd: Option<PathBuf>,
	/// 追加的环境变量。
	pub env: Vec<(String, String)>,
	/// 初始列数。
	pub cols: u16,
	/// 初始行数。
	pub rows: u16,
}

impl Default for PtyCommand {
	fn default() -> Self {
		Self {
			program: String::new(),
			args: Vec::new(),
			cwd: None,
			env: Vec::new(),
			cols: DEFAULT_COLS,
			rows: DEFAULT_ROWS,
		}
	}
}

impl PtyCommand {
	/// 指定可执行文件。
	#[must_use]
	pub fn new(program: impl Into<String>) -> Self {
		Self {
			program: program.into(),
			..Self::default()
		}
	}

	/// 当前平台的默认登录 shell。
	///
	/// 这是本文件里**唯一**的平台分支。
	///
	/// # Unix
	///
	/// 读 `$SHELL` 退回 `/bin/sh`，加 `-l` 走登录 shell（加载 profile，
	/// 环境变量与 alias 才齐全）。
	///
	/// # Windows（最低 Win10）
	///
	/// 用系统自带的 `powershell.exe`（Win10 起预装，5.1 版）。
	/// 参数不是随便加的：
	///
	/// - `-NoLogo`：不打那四行版权横幅，终端从提示符开始。
	/// - `-NoExit`：执行完下面的初始化命令**留在交互模式**，
	///   否则 PowerShell 会跑完就退出，终端立刻「已结束」。
	/// - `[Console]::OutputEncoding=UTF8`：ConPTY 默认按控制台代码页
	///   （简中机器是 936/GBK）把子进程输出转成字节，而我们这一侧按 UTF-8
	///   解析。不改成 UTF-8，`dir` 出来的中文文件名、以及所有中文提示
	///   都会是乱码。这一条同时把输入编码改掉，粘贴中文才正常。
	///
	/// **逃逸口**：设置环境变量 `DAISY_SHELL`（如 `cmd.exe`、`pwsh.exe`）
	/// 可以整体替换，替换后不再附加任何参数。真机上万一 PowerShell
	/// 的初始化参数在某个版本上不成立，用户不必等我们改代码。
	#[must_use]
	pub fn local_shell() -> Self {
		if let Ok(custom) = std::env::var("DAISY_SHELL")
			&& !custom.trim().is_empty()
		{
			return Self::new(custom);
		}

		if cfg!(windows) {
			Self::new("powershell.exe")
				.arg("-NoLogo")
				.arg("-NoExit")
				.arg("-Command")
				.arg("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8")
		} else {
			let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
			// -l：登录 shell，会加载用户的 profile，环境变量与 alias 才齐全
			Self::new(program).arg("-l")
		}
	}

	/// 追加一个参数。
	#[must_use]
	pub fn arg(mut self, arg: impl Into<String>) -> Self {
		self.args.push(arg.into());
		self
	}

	/// 追加多个参数。
	#[must_use]
	pub fn args<I, S>(mut self, args: I) -> Self
	where
		I: IntoIterator<Item = S>,
		S: Into<String>,
	{
		self.args.extend(args.into_iter().map(Into::into));
		self
	}

	/// 设置工作目录。
	#[must_use]
	pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
		self.cwd = Some(cwd.into());
		self
	}

	/// 追加一个环境变量。
	#[must_use]
	pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
		self.env.push((key.into(), value.into()));
		self
	}

	/// 设置初始尺寸。
	#[must_use]
	pub fn size(mut self, cols: u16, rows: u16) -> Self {
		self.cols = cols;
		self.rows = rows;
		self
	}

	fn validate(&self) -> Result<()> {
		if self.program.trim().is_empty() {
			return Err(TermError::Spawn("未指定要执行的程序".to_string()));
		}
		validate_size(self.cols, self.rows)?;
		Ok(())
	}

	fn to_builder(&self) -> CommandBuilder {
		let mut builder = CommandBuilder::new(&self.program);
		builder.args(&self.args);
		if let Some(cwd) = &self.cwd {
			builder.cwd(cwd);
		}
		// 没有 TERM，很多程序会退化成无颜色输出
		builder.env("TERM", TERM_VALUE);
		for (key, value) in &self.env {
			builder.env(key, value);
		}
		builder
	}
}

/// 一个运行中的 PTY 会话。
pub struct PtySession {
	master: Box<dyn MasterPty + Send>,
	writer: Box<dyn Write + Send>,
	child: Box<dyn Child + Send + Sync>,
	rx: Receiver<Vec<u8>>,
	reader_done: Arc<AtomicBool>,
	reader_thread: Option<JoinHandle<()>>,
	/// 读取线程与界面线程共享的唤醒回调槽。
	/// 会话先建、回调后装（界面层建完视图才拿到 `egui::Context`），所以是空槽起步。
	waker: WakerSlot,
	exit_code: Option<i32>,
	cols: u16,
	rows: u16,
}

impl PtySession {
	/// 拉起子进程并开始读取输出。
	pub fn spawn(command: &PtyCommand) -> Result<Self> {
		command.validate()?;

		let pty_system = native_pty_system();
		let size = pty_size(command.cols, command.rows);

		let pair = pty_system
			.openpty(size)
			.map_err(|e| TermError::Spawn(format!("创建 PTY 失败：{e}")))?;

		let child = pair
			.slave
			.spawn_command(command.to_builder())
			.map_err(|e| TermError::Spawn(format!("启动 {} 失败：{e}", command.program)))?;

		// slave 端在子进程启动后即可释放，否则父进程会一直占着一个句柄
		drop(pair.slave);

		let reader = pair
			.master
			.try_clone_reader()
			.map_err(|e| TermError::Terminal(format!("获取读取端失败：{e}")))?;
		let writer = pair
			.master
			.take_writer()
			.map_err(|e| TermError::Terminal(format!("获取写入端失败：{e}")))?;

		let (tx, rx) = mpsc::channel::<Vec<u8>>();
		let reader_done = Arc::new(AtomicBool::new(false));
		let waker = WakerSlot::default();

		let done_flag = Arc::clone(&reader_done);
		let waker_slot = waker.clone();
		let reader_thread = thread::Builder::new()
			.name("daisy-pty-reader".to_string())
			.spawn(move || read_loop(reader, tx, done_flag, waker_slot))
			.map_err(|e| TermError::Terminal(format!("创建读取线程失败：{e}")))?;

		Ok(Self {
			master: pair.master,
			writer,
			child,
			rx,
			reader_done,
			reader_thread: Some(reader_thread),
			waker,
			exit_code: None,
			cols: command.cols,
			rows: command.rows,
		})
	}

	/// 非阻塞取一个事件。没有就绪事件时返回 `None`。
	///
	/// 界面层每帧调用一次即可。
	pub fn try_recv(&mut self) -> Option<LinkEvent> {
		match self.rx.try_recv() {
			Ok(chunk) => return Some(LinkEvent::Output(chunk)),
			Err(TryRecvError::Empty) => {},
			Err(TryRecvError::Disconnected) => {
				self.reader_done.store(true, Ordering::Release);
			},
		}

		// 读取线程收尾之后才去收割退出码，避免过早 try_wait 拿到 None
		if self.exit_code.is_none() && self.reader_done.load(Ordering::Acquire) {
			match self.child.try_wait() {
				Ok(Some(status)) => {
					let code = status.exit_code() as i32;
					self.exit_code = Some(code);
					return Some(LinkEvent::Exited(code));
				},
				Ok(None) => {},
				Err(e) => {
					tracing::warn!("收割终端退出码失败：{e}");
				},
			}
		}

		None
	}

	/// 向子进程写入数据（键盘输入）。
	pub fn write(&mut self, data: &[u8]) -> Result<()> {
		if self.exit_code.is_some() {
			return Err(TermError::SessionClosed);
		}
		self.writer.write_all(data)?;
		self.writer.flush()?;
		Ok(())
	}

	/// 调整 PTY 尺寸。子进程会收到 `SIGWINCH`，`stty size` 随之变化。
	pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
		validate_size(cols, rows)?;
		if (cols, rows) == (self.cols, self.rows) {
			return Ok(());
		}

		self.master
			.resize(pty_size(cols, rows))
			.map_err(|e| TermError::Terminal(format!("调整 PTY 尺寸失败：{e}")))?;

		self.cols = cols;
		self.rows = rows;
		Ok(())
	}

	/// 当前尺寸 `(cols, rows)`。
	#[must_use]
	pub fn size(&self) -> (u16, u16) {
		(self.cols, self.rows)
	}

	/// 子进程是否已退出。
	#[must_use]
	pub fn is_exited(&self) -> bool {
		self.exit_code.is_some()
	}

	/// 已收割到的退出码。
	#[must_use]
	pub fn exit_code(&self) -> Option<i32> {
		self.exit_code
	}

	/// 主动结束子进程。会话已结束时返回 `Ok(false)`。
	pub fn kill(&mut self) -> Result<bool> {
		if self.exit_code.is_some() {
			return Ok(false);
		}
		self.child.kill()?;
		Ok(true)
	}
}

impl Drop for PtySession {
	fn drop(&mut self) {
		// 子进程还活着就收掉，否则会残留成孤儿进程
		if self.exit_code.is_none() {
			let _ = self.child.kill();
		}
		// 读取线程会因为 master 句柄关闭而自然退出
		if let Some(handle) = self.reader_thread.take() {
			let _ = handle.join();
		}
	}
}

impl std::fmt::Debug for PtySession {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("PtySession")
			.field("cols", &self.cols)
			.field("rows", &self.rows)
			.field("exit_code", &self.exit_code)
			.finish_non_exhaustive()
	}
}

impl SessionLink for PtySession {
	fn try_recv(&mut self) -> Option<LinkEvent> {
		PtySession::try_recv(self)
	}

	fn write(&mut self, data: &[u8]) -> Result<()> {
		PtySession::write(self, data)
	}

	fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
		PtySession::resize(self, cols, rows)
	}

	fn shutdown(&mut self) -> Result<bool> {
		self.kill()
	}

	fn set_waker(&mut self, waker: Waker) {
		self.waker.set(waker);
	}
}

// ==================== 内部实现 ====================

fn read_loop(
	mut reader: Box<dyn Read + Send>,
	tx: mpsc::Sender<Vec<u8>>,
	done: Arc<AtomicBool>,
	waker: WakerSlot,
) {
	let mut buffer = vec![0u8; READ_BUFFER_SIZE];

	loop {
		match reader.read(&mut buffer) {
			Ok(0) => break,
			Ok(n) => {
				// 接收端已丢弃时说明界面已经不需要了，直接收工
				if tx.send(buffer[..n].to_vec()).is_err() {
					break;
				}
				// 送完就叫醒界面。没有这一步，输出只能等下一次周期性重绘
				// （光标闪烁 500ms 一次），远端刷屏看起来像卡住。
				waker.wake();
			},
			Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
			Err(e) => {
				// 子进程退出时 Unix PTY 会报 EIO，属正常收尾路径
				tracing::debug!("PTY 读取结束：{e}");
				break;
			},
		}
	}

	done.store(true, Ordering::Release);
	// 收尾也要叫一次：退出码要等 `reader_done` 置位后才收割，
	// 界面不知道读取线程已经结束就会一直不刷新，「已退出」迟迟不出现。
	waker.wake();
}

fn validate_size(cols: u16, rows: u16) -> Result<()> {
	if cols == 0 || rows == 0 {
		return Err(TermError::InvalidSize(format!(
			"列数与行数必须大于 0，收到 {cols}x{rows}"
		)));
	}
	Ok(())
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
	PtySize {
		rows,
		cols,
		pixel_width: 0,
		pixel_height: 0,
	}
}

#[cfg(test)]
// 测试辅助函数（read_until / wait_for_exit）只被 `/bin/sh`、`/bin/cat` 这类
// Unix 专用用例调用，Windows 目标下它们自然「没人引用」。
// 这**不是**真的死代码，所以在测试模块上整体放行，别让它污染
// Windows 交叉检查的零警告门。
#[cfg_attr(windows, allow(dead_code, unused_imports))]
mod tests {
	use super::*;
	use std::time::{Duration, Instant};

	/// 收集输出直到出现关键字或超时。返回累积的输出文本。
	fn read_until(session: &mut PtySession, needle: &str, timeout: Duration) -> String {
		let deadline = Instant::now() + timeout;
		let mut acc = Vec::new();

		while Instant::now() < deadline {
			match session.try_recv() {
				Some(LinkEvent::Output(chunk)) => {
					acc.extend_from_slice(&chunk);
					if String::from_utf8_lossy(&acc).contains(needle) {
						break;
					}
				},
				Some(LinkEvent::Exited(_)) => break,
				None => std::thread::sleep(Duration::from_millis(10)),
			}
		}

		String::from_utf8_lossy(&acc).into_owned()
	}

	fn wait_for_exit(session: &mut PtySession, timeout: Duration) -> Option<i32> {
		let deadline = Instant::now() + timeout;
		while Instant::now() < deadline {
			match session.try_recv() {
				Some(LinkEvent::Exited(code)) => return Some(code),
				Some(LinkEvent::Output(_)) | None => std::thread::sleep(Duration::from_millis(10)),
			}
		}
		None
	}

	#[test]
	fn command_validation() {
		let err = PtyCommand::new("   ").size(80, 24);
		assert!(PtySession::spawn(&err).is_err(), "空程序名应被拒绝");

		let mut cmd = PtyCommand::new("/bin/sh");
		cmd.cols = 0;
		assert!(PtySession::spawn(&cmd).is_err(), "0 列应被拒绝");
	}

	#[test]
	fn builder_collects_arguments() {
		let cmd = PtyCommand::new("prog")
			.arg("-a")
			.args(["-b", "-c"])
			.env("K", "V")
			.size(100, 30);

		assert_eq!(cmd.program, "prog");
		assert_eq!(cmd.args, vec!["-a", "-b", "-c"]);
		assert_eq!(cmd.env, vec![("K".to_string(), "V".to_string())]);
		assert_eq!((cmd.cols, cmd.rows), (100, 30));
	}

	#[cfg(unix)]
	#[test]
	fn reads_child_output() {
		let mut session = PtySession::spawn(
			&PtyCommand::new("/bin/sh")
				.arg("-c")
				.arg("printf 'hello-pty'"),
		)
		.expect("启动失败");

		let output = read_until(&mut session, "hello-pty", Duration::from_secs(5));
		assert!(output.contains("hello-pty"), "实际输出：{output:?}");
	}

	#[cfg(unix)]
	#[test]
	fn exit_code_is_reported() {
		let mut session = PtySession::spawn(&PtyCommand::new("/bin/sh").arg("-c").arg("exit 7"))
			.expect("启动失败");

		assert_eq!(wait_for_exit(&mut session, Duration::from_secs(5)), Some(7));
		assert!(session.is_exited());
		assert_eq!(session.exit_code(), Some(7));
	}

	#[cfg(unix)]
	#[test]
	fn reader_thread_wakes_the_caller() {
		use std::sync::atomic::AtomicUsize;

		let wakes = Arc::new(AtomicUsize::new(0));
		let counter = Arc::clone(&wakes);

		// 用 `cat` 而不是一行就退出的命令：只有这样输出才必定来自读取线程，
		// 而不是收尾路径那一次唤醒
		let mut session = PtySession::spawn(&PtyCommand::new("/bin/cat")).expect("启动失败");
		session.set_waker(Arc::new(move || {
			counter.fetch_add(1, Ordering::AcqRel);
		}));

		session.write(b"wake-me\n").expect("写入失败");

		let deadline = Instant::now() + Duration::from_secs(5);
		while wakes.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
			std::thread::sleep(Duration::from_millis(5));
		}
		assert!(
			wakes.load(Ordering::Acquire) > 0,
			"读取线程送完数据后应叫醒调用方，否则输出只能等下一次周期性重绘"
		);

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn bidirectional_echo() {
		// cat 会把 stdin 原样回写，正好用来验证双向链路
		let mut session = PtySession::spawn(&PtyCommand::new("/bin/cat")).expect("启动失败");

		session.write(b"round-trip-check\n").expect("写入失败");
		let output = read_until(&mut session, "round-trip-check", Duration::from_secs(5));
		assert!(output.contains("round-trip-check"), "实际输出：{output:?}");

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn writing_after_exit_is_rejected() {
		let mut session = PtySession::spawn(&PtyCommand::new("/bin/sh").arg("-c").arg("exit 0"))
			.expect("启动失败");

		wait_for_exit(&mut session, Duration::from_secs(5));
		let err = session.write(b"x").expect_err("已结束的会话不应接受写入");
		assert!(
			matches!(err, TermError::SessionClosed),
			"错误类型不符：{err:?}"
		);
	}

	#[cfg(unix)]
	#[test]
	fn resize_is_observed_by_child() {
		// stty size 会打印 "行数 列数"
		let mut session = PtySession::spawn(
			&PtyCommand::new("/bin/sh")
				.arg("-c")
				.arg("stty size; sleep 5")
				.size(100, 30),
		)
		.expect("启动失败");

		let output = read_until(&mut session, "30 100", Duration::from_secs(5));
		assert!(
			output.contains("30 100"),
			"初始尺寸不符，实际输出：{output:?}"
		);

		// 改变尺寸后子进程会收到 SIGWINCH；这里只验证调用不报错且状态已更新
		session.resize(120, 40).expect("resize 失败");
		assert_eq!(session.size(), (120, 40));

		// 相同尺寸是幂等的
		session.resize(120, 40).expect("重复 resize 失败");

		session.kill().expect("结束失败");
	}

	#[cfg(unix)]
	#[test]
	fn kill_terminates_long_running_child() {
		let mut session = PtySession::spawn(&PtyCommand::new("/bin/sh").arg("-c").arg("sleep 30"))
			.expect("启动失败");

		assert!(session.kill().expect("kill 失败"), "首次 kill 应返回 true");
		let code = wait_for_exit(&mut session, Duration::from_secs(5));
		assert!(code.is_some(), "被 kill 的子进程应上报退出");
	}

	#[test]
	fn local_shell_is_not_empty() {
		let shell = PtyCommand::local_shell();
		assert!(!shell.program.trim().is_empty());
	}
}
