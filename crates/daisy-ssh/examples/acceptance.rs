//! S6-50 验收：用 Daisy 自己的 SSH 实现连真实 Linux 主机。
//!
//! **不使用系统 `ssh` 客户端。** 这条链路上跑的是 `daisy-net` 的拨号器、
//! `daisy-ssh` 的建连与认证、`daisy-term` 的网格——系统 `ssh` 能连通
//! 完全不能说明 Daisy 能连通。
//!
//! ## 用法
//!
//! ```bash
//! # 密码认证
//! DAISY_SSH_HOST=192.168.10.21 \
//! DAISY_SSH_USER=<用户名> \
//! DAISY_SSH_PASSWORD='<密码>' \
//!   cargo run -q -p daisy-ssh --example acceptance
//!
//! # 公钥认证（同一台主机，换一种方式再验一遍）
//! DAISY_SSH_HOST=192.168.10.21 \
//! DAISY_SSH_USER=<用户名> \
//! DAISY_SSH_KEY=~/.ssh/id_rsa \
//!   cargo run -q -p daisy-ssh --example acceptance
//! ```
//!
//! 认证信息只从环境变量读，**不落盘、不打印**。
//!
//! ## 验收项
//!
//! | # | 检查 | 判据 |
//! | --- | --- | --- |
//! | 1 | 主机密钥可观测 | 打印算法与 `SHA256:` 指纹，首次连接判为 `Unknown` |
//! | 2 | 认证方式与结果 | 打印实际尝试的顺序与结果 |
//! | 3 | 远端 PTY + shell | 能拿到远端输出 |
//! | 4 | 双向字节流 | `echo` 的标记回到本地网格 |
//! | 5 | 远端窗口尺寸 | `stty size` 报出初始尺寸 |
//! | 6 | **resize 传到远端** | resize 后 `stty size` 跟着变（这是 S6-48 的核心） |
//! | 7 | 远端退出码 | `exit 7` 后本地读到 7 |
//!
//! 任一项失败则退出码非 0。

use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use daisy_ssh::{
	AuthSpec, HostKeyPolicy, RemoteShell, RsaSigningPolicy, SshParams, SshTarget, connect,
};
use daisy_term::{DEFAULT_SCROLLBACK, TerminalSession};
use zeroize::Zeroizing;

/// 远端 PTY 的初始尺寸。
const COLS: u16 = 100;
/// 远端 PTY 的初始行数。
const ROWS: u16 = 30;
/// 每个检查的等待上限。
const STEP_TIMEOUT: Duration = Duration::from_secs(12);

fn env_or(name: &str, fallback: &str) -> String {
	env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn env_required(name: &str) -> String {
	env::var(name).unwrap_or_else(|_| {
		eprintln!("缺少环境变量 {name}");
		std::process::exit(2);
	})
}

fn main() {
	let code = run();
	if code != 0 {
		eprintln!("\n验收未通过（退出码 {code}）");
	}
	std::process::exit(code);
}

fn run() -> i32 {
	// DAISY_SSH_TRACE=1 打开我们的 debug 日志（通道消息级别），
	// 排查真实主机的行为差异时非常有用
	if env::var("DAISY_SSH_TRACE").is_ok() {
		let _ = tracing_subscriber::fmt()
			.with_env_filter(
				tracing_subscriber::EnvFilter::try_from_default_env()
					.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("daisy_ssh=debug")),
			)
			.with_writer(std::io::stderr)
			.try_init();
	}

	let host = env_or("DAISY_SSH_HOST", "");
	if host.is_empty() {
		eprintln!("缺少环境变量 DAISY_SSH_HOST");
		return 2;
	}
	let port: u16 = env_or("DAISY_SSH_PORT", "22").parse().unwrap_or(22);
	let user = env_required("DAISY_SSH_USER");

	// 认证方式由环境变量决定：
	//   只给 KEY          → 只用公钥
	//   只给 PASSWORD     → 只用密码
	//   两个都给          → 先公钥后密码（步骤 45 的失败回退）
	let key = env::var("DAISY_SSH_KEY")
		.ok()
		.filter(|value| !value.trim().is_empty())
		.map(|value| {
			(
				expand_tilde(&value),
				env::var("DAISY_SSH_KEY_PASSPHRASE").ok(),
			)
		});
	let password = env::var("DAISY_SSH_PASSWORD")
		.ok()
		.filter(|value| !value.is_empty());

	let auth = match (key, password) {
		(Some((path, passphrase)), Some(password)) => AuthSpec::PublicKeyThenPassword {
			path,
			passphrase: passphrase
				.filter(|value| !value.is_empty())
				.map(Zeroizing::new),
			password: Zeroizing::new(password),
		},
		(Some((path, passphrase)), None) => AuthSpec::PublicKey {
			path,
			passphrase: passphrase
				.filter(|value| !value.is_empty())
				.map(Zeroizing::new),
		},
		(None, Some(password)) => AuthSpec::Password {
			password: Zeroizing::new(password),
		},
		(None, None) => {
			eprintln!("需要 DAISY_SSH_KEY 或 DAISY_SSH_PASSWORD");
			return 2;
		},
	};

	let method = match &auth {
		AuthSpec::Password { .. } => "密码",
		AuthSpec::PublicKey { .. } => "公钥",
		AuthSpec::PublicKeyThenPassword { .. } => "公钥 → 密码（失败回退）",
	};

	println!("== Daisy SSH 验收（S6-50）==");
	println!("目标：{user}@{host}:{port}");
	println!("认证：{method}");
	println!();

	let runtime = match tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
	{
		Ok(runtime) => runtime,
		Err(error) => {
			eprintln!("创建运行时失败：{error}");
			return 2;
		},
	};

	runtime.block_on(async move {
		// 主机密钥策略：首次连接接受并上报，不自动保存——
		// 保存与否是用户/上层的决定
		let mut params = SshParams::new(SshTarget::new(host.clone(), port, user.clone()), auth)
			.with_host_key_policy(HostKeyPolicy::first_connection())
			.with_rsa_policy(RsaSigningPolicy::MODERN);

		// 可选的代理隧道（S7）：DAISY_SSH_PROXY=socks5://host:port 或 http://host:port
		if let Ok(proxy) = env::var("DAISY_SSH_PROXY")
			&& !proxy.trim().is_empty()
		{
			let dialer =
				daisy_net::build_dialer(parse_proxy_endpoint(&proxy), Duration::from_secs(8));
			params = params.with_dialer(dialer);
			println!("代理：{proxy}");
		}

		let session = match connect(params).await {
			Ok(session) => session,
			Err(error) => {
				eprintln!("[1/7] 连接失败：{error}");
				return 1;
			},
		};

		// ---- 1. 主机密钥 ----
		let host_key = session.host_key();
		println!("[1/7] 主机密钥");
		println!("      算法   : {}", host_key.key_type);
		println!("      指纹   : {}", host_key.fingerprint);
		println!("      结论   : {:?}", session.host_key_verdict());
		println!("      公钥   : {}", host_key.base64);

		// ---- 2. 认证 ----
		println!();
		println!("[2/7] 认证");
		for attempt in session.auth_attempts() {
			println!("      {} → {:?}", attempt.method, attempt.outcome);
		}

		// ---- 3~6. 远端 shell ----
		let shell = match RemoteShell::open(&session, COLS, ROWS).await {
			Ok(shell) => shell,
			Err(error) => {
				eprintln!("[3/7] 打开远端 shell 失败：{error}");
				return 1;
			},
		};

		let mut terminal =
			match TerminalSession::attach(Box::new(shell), COLS, ROWS, DEFAULT_SCROLLBACK) {
				Ok(terminal) => terminal,
				Err(error) => {
					eprintln!("[3/7] 挂载终端网格失败：{error}");
					return 1;
				},
			};

		// 先等登录横幅与提示符出现
		let banner = pump_until(&mut terminal, "$", Duration::from_secs(10)).await;
		println!();
		println!("[3/7] 远端 shell 已就绪（{} 字节屏幕内容）", banner.len());
		if !banner.contains('$')
			&& !banner.contains('#')
			&& !banner.contains('%')
			&& !banner.contains('>')
		{
			eprintln!("      未看到 shell 提示符，屏幕内容：");
			print_screen(&banner);
		}

		let mut failures = 0;

		// ---- 4. 双向字节流 ----
		let marker = "DAISY-ROUNDTRIP-OK";
		terminal
			.write(format!("echo {marker}\n").as_bytes())
			.expect("写入失败");
		let screen = pump_until(&mut terminal, marker, STEP_TIMEOUT).await;
		if screen.contains(marker) {
			println!("[4/7] 双向字节流      OK（{marker} 回到本地网格）");
		} else {
			println!("[4/7] 双向字节流      失败");
			print_screen(&screen);
			failures += 1;
		}

		// 顺便取一次远端身份信息，写进验收记录
		terminal.write(b"uname -srm\n").expect("写入失败");
		let screen = pump_until(&mut terminal, "Linux", Duration::from_secs(6)).await;
		if let Some(line) = screen.lines().find(|line| line.contains("Linux"))
			&& line.trim().len() < 60
		{
			println!("      远端内核      : {}", line.trim());
		}

		// ---- 5. 初始窗口尺寸 ----
		let expect_initial = format!("{ROWS} {COLS}");
		terminal.write(b"stty size\n").expect("写入失败");
		let screen = pump_until(&mut terminal, &expect_initial, STEP_TIMEOUT).await;
		if screen.contains(&expect_initial) {
			println!("[5/7] 远端初始尺寸    OK（stty size 报 {expect_initial}）");
		} else {
			println!("[5/7] 远端初始尺寸    失败，期望 {expect_initial}");
			print_screen(&screen);
			failures += 1;
		}

		// ---- 6. resize 传到远端（S6-48 的核心） ----
		let new_cols = COLS + 20;
		let new_rows = ROWS + 10;
		let expect_resized = format!("{new_rows} {new_cols}");
		terminal
			.resize(new_cols, new_rows)
			.expect("本地 resize 失败");
		// 给对端一点时间处理
		tokio::time::sleep(Duration::from_millis(300)).await;

		terminal.write(b"stty size\n").expect("写入失败");
		let screen = pump_until(&mut terminal, &expect_resized, STEP_TIMEOUT).await;
		if screen.contains(&expect_resized) {
			println!("[6/7] resize 传到远端  OK（{COLS}x{ROWS} → {new_cols}x{new_rows}）");
		} else {
			println!("[6/7] resize 传到远端  失败，期望 {expect_resized}，屏幕里找不到");
			print_screen(&screen);
			failures += 1;
		}

		// ---- 7. 远端退出码 ----
		terminal.write(b"exit 7\n").expect("写入失败");
		let deadline = Instant::now() + STEP_TIMEOUT;
		while Instant::now() < deadline && !terminal.is_exited() {
			terminal.pump();
			tokio::time::sleep(Duration::from_millis(20)).await;
		}

		match terminal.exit_code() {
			Some(7) => println!("[7/7] 远端退出码      OK（exit 7 被正确上报）"),
			Some(other) => {
				println!("[7/7] 远端退出码      失败，期望 7，实际 {other}");
				failures += 1;
			},
			None => {
				println!("[7/7] 远端退出码      失败，未收到退出状态");
				failures += 1;
			},
		}

		println!();
		if failures == 0 {
			println!("全部 7 项通过");
			0
		} else {
			println!("{failures} 项失败");
			1
		}
	})
}

/// 把 `~` 展开成家目录。`LoadedKey` 自己不做这件事——它收的是路径。
fn expand_tilde(path: &str) -> PathBuf {
	if let Some(rest) = path.strip_prefix("~/")
		&& let Ok(home) = env::var("HOME")
	{
		return PathBuf::from(home).join(rest);
	}
	PathBuf::from(path)
}

/// 解析 `socks5://host:port` 或 `http://host:port` 成代理端点。
fn parse_proxy_endpoint(raw: &str) -> Option<daisy_net::ProxyEndpoint> {
	use daisy_core::model::ProxyKind;

	let (kind, rest) = if let Some(rest) = raw.strip_prefix("socks5://") {
		(ProxyKind::Socks5, rest)
	} else if let Some(rest) = raw.strip_prefix("http://") {
		(ProxyKind::Http, rest)
	} else {
		eprintln!("DAISY_SSH_PROXY 需以 socks5:// 或 http:// 开头");
		return None;
	};

	let (host, port) = if let Some(rest) = rest.strip_prefix('[') {
		let (h, tail) = rest.split_once(']')?;
		(h, tail.trim_start_matches(':').parse::<u16>().ok()?)
	} else {
		let (h, p) = rest.rsplit_once(':')?;
		(h, p.parse::<u16>().ok()?)
	};

	daisy_net::ProxyEndpoint::new(kind, host, port).ok()
}

/// 泵到屏幕出现关键字或超时为止，返回屏幕纯文本。
async fn pump_until(session: &mut TerminalSession, needle: &str, timeout: Duration) -> String {
	let deadline = Instant::now() + timeout;
	while Instant::now() < deadline {
		session.pump();
		let text = screen_text(session);
		if text.contains(needle) {
			return text;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	screen_text(session)
}

/// 把网格拍成纯文本，去掉右侧空白。
fn screen_text(session: &TerminalSession) -> String {
	let snapshot = session.snapshot();
	let mut out = String::new();
	for row in 0..snapshot.rows {
		let mut line = String::new();
		for col in 0..snapshot.cols {
			if let Some(cell) = snapshot.cell(row, col)
				&& !cell.flags.wide_spacer
			{
				line.push(cell.ch);
			}
		}
		out.push_str(line.trim_end());
		out.push('\n');
	}
	out
}

fn print_screen(text: &str) {
	eprintln!("      ── 屏幕内容 ──");
	for line in text.lines() {
		if !line.trim().is_empty() {
			eprintln!("      | {line}");
		}
	}
	eprintln!("      ── 结束 ──");
}
