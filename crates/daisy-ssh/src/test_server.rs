//! 测试用 SSH 服务端。
//!
//! STEPS.md 的 B1（一台可连的 Linux 主机）还没到位。与其干等，这里用 `russh`
//! 的 server 端在 `127.0.0.1` 起一个**真 SSH 服务**——真握手、真密码与公钥认证、
//! 真 PTY 请求、真窗口尺寸调整。
//!
//! 因此 S6 的绝大部分逻辑都能端到端跑通，而不是靠桩函数。
//! 明确的差距只有一条：**它不是 Linux**，所以「连上真实 Linux 主机」这件事
//! 仍然要等 B1 才能算数（这一步的验收记录见 S6-50）。
//!
//! ## 服务端行为
//!
//! 收到一个字节流后按行处理：
//!
//! | 输入 | 输出 |
//! | --- | --- |
//! | `size` | `size: <cols>x<rows>`，用于验证 resize 是否传到服务端 |
//! | `echo <文本>` | 原样回显 |
//! | `exit` | 回显 `bye` 并带退出码 7 结束通道 |
//! | 其它 | 回显 `got: <行>` |

use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::PrivateKey;
use russh::keys::ssh_key::PublicKey;
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, Pty};

use crate::error::SshError;

/// 服务端主机私钥（Ed25519，一次性测试密钥）。
pub const HOST_KEY_PEM: &str = include_str!("../tests/fixtures/host_ed25519");

/// 上面那把主机密钥的指纹。客户端观测到的应当就是它。
pub const HOST_KEY_FINGERPRINT: &str = "SHA256:jI/d9NM+bENAvZyaqWTJV1gYY0nVxGUg5rZvjvtgAys";

/// 客户端用的公钥（与 `openssh_rsa` 是同一对）。
pub const CLIENT_PUBKEY: &str = include_str!("../tests/fixtures/openssh_rsa.pub");

/// 测试服务端接受的密码。
pub const PASSWORD: &str = "daisy-test-password";

/// 服务端侧观测到的一次 PTY 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyRequest {
	/// 客户端声明的 TERM。
	pub term: String,
	/// 列数。
	pub cols: u32,
	/// 行数。
	pub rows: u32,
}

/// 测试期间累积的服务端观测。
#[derive(Debug, Default)]
pub struct ServerState {
	/// 收到的密码认证尝试：`(用户, 密码)`。
	pub password_attempts: Vec<(String, String)>,
	/// 收到的公钥认证尝试：用户名。
	pub publickey_attempts: Vec<String>,
	/// 收到的 PTY 请求。
	pub pty_requests: Vec<PtyRequest>,
	/// 收到的窗口尺寸调整。
	pub window_changes: Vec<(u32, u32)>,
	/// 收到的 shell 请求次数。
	pub shell_requests: usize,
	/// 当前尺寸，`size` 命令据此作答。
	pub size: (u32, u32),
}

/// 服务端行为配置。
#[derive(Debug, Clone)]
pub struct TestServerOptions {
	/// 接受的密码。`None` 表示拒绝所有密码认证。
	pub password: Option<String>,
	/// 接受的公钥。空表示拒绝所有公钥。
	pub authorized_keys: Vec<PublicKey>,
}

impl TestServerOptions {
	/// 同时接受 [`PASSWORD`] 与 [`CLIENT_PUBKEY`]。
	#[must_use]
	pub fn accepting_both() -> Self {
		Self {
			password: Some(PASSWORD.to_string()),
			authorized_keys: vec![
				PublicKey::from_openssh(CLIENT_PUBKEY.trim()).expect("客户端公钥夹具应能解析"),
			],
		}
	}

	/// 只接受密码。
	#[must_use]
	pub fn password_only() -> Self {
		Self {
			password: Some(PASSWORD.to_string()),
			authorized_keys: Vec::new(),
		}
	}

	/// 只接受公钥。
	#[must_use]
	pub fn publickey_only() -> Self {
		Self {
			password: None,
			authorized_keys: vec![
				PublicKey::from_openssh(CLIENT_PUBKEY.trim()).expect("客户端公钥夹具应能解析"),
			],
		}
	}
}

/// 一个跑起来的测试服务端。
pub struct TestServerHandle {
	/// 监听端口。
	pub port: u16,
	/// 服务端观测。
	pub state: Arc<Mutex<ServerState>>,
	shutdown: russh::server::RunningServerHandle,
}

impl TestServerHandle {
	/// 读取一份观测快照。
	#[must_use]
	pub fn snapshot(&self) -> ServerState {
		// 观测结构里的字段都是 Clone，直接整体克隆最简单
		let guard = self.state.lock().expect("观测锁不应损坏");
		ServerState {
			password_attempts: guard.password_attempts.clone(),
			publickey_attempts: guard.publickey_attempts.clone(),
			pty_requests: guard.pty_requests.clone(),
			window_changes: guard.window_changes.clone(),
			shell_requests: guard.shell_requests,
			size: guard.size,
		}
	}

	/// 停止服务端。
	pub fn stop(&self) {
		self.shutdown.shutdown("测试结束".to_string());
	}
}

impl Drop for TestServerHandle {
	fn drop(&mut self) {
		self.stop();
	}
}

/// 在 `127.0.0.1` 的随机端口上起一个测试服务端。
pub async fn start(options: TestServerOptions) -> TestServerHandle {
	start_on("127.0.0.1:0", options).await
}

/// 在指定地址上起一个测试服务端。
///
/// 传 `[::1]:0` 即可验证 IPv6 路径——用于覆盖
/// 「域名 / IPv4 / IPv6 三种写法都要能连」这条要求。
pub async fn start_on(bind: &str, options: TestServerOptions) -> TestServerHandle {
	let listener = tokio::net::TcpListener::bind(bind)
		.await
		.unwrap_or_else(|error| panic!("绑定 {bind} 失败：{error}"));
	let port = listener.local_addr().expect("取地址失败").port();

	let host_key = PrivateKey::from_openssh(HOST_KEY_PEM).expect("主机私钥夹具应能解析");

	let config = Arc::new(russh::server::Config {
		keys: vec![host_key],
		// 认证失败后的固定等待在测试里纯属浪费
		auth_rejection_time: Duration::from_millis(0),
		auth_rejection_time_initial: Some(Duration::from_millis(0)),
		inactivity_timeout: None,
		nodelay: true,
		..Default::default()
	});

	let state = Arc::new(Mutex::new(ServerState::default()));
	let server = TestServer {
		state: Arc::clone(&state),
		options: Arc::new(options),
	};

	// run_on_socket 借用了 server，所以两者必须一起进同一个任务
	let (handle_tx, handle_rx) = tokio::sync::oneshot::channel();
	tokio::spawn(async move {
		let mut server = server;
		let running = server.run_on_socket(config, &listener);
		// 先把停止句柄送出来，再开始真正跑
		let _ = handle_tx.send(running.handle());
		let _ = running.await;
	});

	let shutdown = handle_rx.await.expect("测试服务端未能启动");

	TestServerHandle {
		port,
		state,
		shutdown,
	}
}

// ==================== 服务端实现 ====================

struct TestServer {
	state: Arc<Mutex<ServerState>>,
	options: Arc<TestServerOptions>,
}

impl russh::server::Server for TestServer {
	type Handler = TestSession;

	fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self::Handler {
		TestSession {
			state: Arc::clone(&self.state),
			options: Arc::clone(&self.options),
			line: String::new(),
		}
	}
}

struct TestSession {
	state: Arc<Mutex<ServerState>>,
	options: Arc<TestServerOptions>,
	/// 按行处理输入，这里攒当前这一行。
	line: String,
}

impl TestSession {
	fn record<F: FnOnce(&mut ServerState)>(&self, edit: F) {
		if let Ok(mut guard) = self.state.lock() {
			edit(&mut guard);
		}
	}

	/// 处理一整行输入，返回要回给客户端的内容。
	fn respond(&mut self, line: &str) -> String {
		let trimmed = line.trim();

		if trimmed == "size" {
			let size = self.state.lock().map(|guard| guard.size).unwrap_or((0, 0));
			return format!("size: {}x{}\r\n", size.0, size.1);
		}

		if let Some(rest) = trimmed.strip_prefix("echo ") {
			return format!("{rest}\r\n");
		}

		format!("got: {trimmed}\r\n")
	}
}

impl russh::server::Handler for TestSession {
	type Error = SshError;

	async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
		self.record(|state| {
			state
				.password_attempts
				.push((user.to_string(), password.to_string()));
		});

		match &self.options.password {
			Some(expected) if expected == password => Ok(Auth::Accept),
			_ => Ok(Auth::reject()),
		}
	}

	async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
		self.record(|state| state.publickey_attempts.push(user.to_string()));

		// 只比 key data，不比整个 PublicKey。
		//
		// `ssh_key::PublicKey` 派生的 `PartialEq` **包含 comment**，但 comment
		// 只是字符串序列化时的附注，不进二进制传输——所以认证请求里收到的
		// 公钥必然没有 comment。拿带 comment 的 `.pub` 去比会永远不相等。
		// （这不是测试代码的权宜之计：任何按 authorized_keys 做的比对都得这样。）
		if self
			.options
			.authorized_keys
			.iter()
			.any(|known| known.key_data() == key.key_data())
		{
			Ok(Auth::Accept)
		} else {
			Ok(Auth::reject())
		}
	}

	async fn channel_open_session(
		&mut self,
		_channel: Channel<Msg>,
		reply: russh::server::ChannelOpenHandle,
		_session: &mut Session,
	) -> Result<(), Self::Error> {
		reply.accept().await;
		Ok(())
	}

	async fn pty_request(
		&mut self,
		_channel: ChannelId,
		term: &str,
		col_width: u32,
		row_height: u32,
		_pix_width: u32,
		_pix_height: u32,
		_modes: &[(Pty, u32)],
		_session: &mut Session,
	) -> Result<(), Self::Error> {
		self.record(|state| {
			state.pty_requests.push(PtyRequest {
				term: term.to_string(),
				cols: col_width,
				rows: row_height,
			});
			state.size = (col_width, row_height);
		});
		Ok(())
	}

	async fn window_change_request(
		&mut self,
		_channel: ChannelId,
		col_width: u32,
		row_height: u32,
		_pix_width: u32,
		_pix_height: u32,
		_session: &mut Session,
	) -> Result<(), Self::Error> {
		self.record(|state| {
			state.window_changes.push((col_width, row_height));
			state.size = (col_width, row_height);
		});
		Ok(())
	}

	async fn shell_request(
		&mut self,
		channel: ChannelId,
		session: &mut Session,
	) -> Result<(), Self::Error> {
		self.record(|state| state.shell_requests += 1);

		let _ = session.channel_success(channel);
		session.data(channel, &b"daisy-test-shell ready\r\n"[..])?;
		Ok(())
	}

	async fn data(
		&mut self,
		channel: ChannelId,
		data: &[u8],
		session: &mut Session,
	) -> Result<(), Self::Error> {
		self.line.push_str(&String::from_utf8_lossy(data));

		// 逐行处理。客户端可能一次送多个字节，也可能分多次送一行。
		while let Some(index) = self.line.find('\n') {
			let line: String = self.line.drain(..=index).collect();
			let line = line.trim_end_matches(['\r', '\n']).to_string();

			if line.trim() == "exit" {
				session.data(channel, &b"bye\r\n"[..])?;

				// ⚠️ **顺序必须照抄真实 sshd：data → EOF → exit-status → CLOSE**
				//
				// 实测 OpenSSH 9.9 在交互式 session 上就是这个顺序，
				// 也就是 **exit-status 在 EOF 之后**。
				// 这个服务端一开始把 exit-status 放在 EOF 前面，结果本地测试
				// 全绿、却在真机上把退出码丢成 -1——顺序写错就等于测了个假东西。
				session.eof(channel)?;
				session.exit_status_request(channel, 7)?;

				// 关闭还要再晚一步：russh 的服务端是在 handler 返回之后才刷缓冲的，
				// 若在同一回调里紧跟着 close，客户端可能一个字节都收不到。
				let handle = session.handle();
				tokio::spawn(async move {
					tokio::time::sleep(Duration::from_millis(80)).await;
					let _ = handle.close(channel).await;
				});
				return Ok(());
			}

			let response = self.respond(&line);
			session.data(channel, response.into_bytes())?;
		}

		Ok(())
	}
}
