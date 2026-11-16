//! 端到端测试：客户端 ↔ 本地测试服务端。
//!
//! 用 [`crate::test_server`] 起的进程内 SSH 服务，把 S6 的整条链路跑通：
//! 握手 → 主机密钥核对 → 密码 / 公钥认证 → 远端 PTY → 字节流桥接到终端网格
//! → 尺寸同步 → 断线检测。
//!
//! **这不是 mock。** 服务端是一个真实的 SSH 实现，会真的做密钥交换与签名校验。
//! 唯一的差距是它跑在 macOS 上而不是 Linux，所以 STEPS.md 的 S6-50
//! （「连通真实 Linux 主机」）仍然要等 B1。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use daisy_term::{DEFAULT_SCROLLBACK, SessionLink, TerminalSession};
use zeroize::Zeroizing;

use crate::channel::RemoteShell;
use crate::error::SshError;
use crate::key::{LoadedKey, RsaSigningPolicy};
use crate::session::{
	AuthAttempt, AuthMethod, AuthOutcome, AuthSpec, HostKeyPolicy, HostKeyVerdict, SshParams,
	SshSession, SshTarget, TrustedHostKey, connect,
};
use crate::test_server::{HOST_KEY_FINGERPRINT, PASSWORD, TestServerOptions, start, start_on};

/// 测试用口令（见 `tests/fixtures/README.md`）。
const KEY_PASSPHRASE: &str = "daisy-test-passphrase";

fn fixture(name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("tests/fixtures")
		.join(name)
}

fn target(port: u16) -> SshTarget {
	target_at("127.0.0.1", port)
}

fn target_at(host: &str, port: u16) -> SshTarget {
	SshTarget::new(host, port, "daisy")
}

fn password_spec(password: &str) -> AuthSpec {
	AuthSpec::Password {
		password: Zeroizing::new(password.to_string()),
	}
}

fn key_spec(name: &str, passphrase: Option<&str>) -> AuthSpec {
	AuthSpec::PublicKey {
		path: fixture(name),
		passphrase: passphrase.map(|p| Zeroizing::new(p.to_string())),
	}
}

/// 等待会话进入关闭状态。
async fn wait_closed(session: &SshSession, timeout: Duration) -> bool {
	let deadline = Instant::now() + timeout;
	while Instant::now() < deadline {
		if session.is_closed() {
			return true;
		}
		tokio::time::sleep(Duration::from_millis(20)).await;
	}
	session.is_closed()
}

// ==================== 地址写法：域名 / IPv4 / IPv6 ====================
//
// 需求：SSH 与 RDP 都要支持 IP 地址、域名，以及 IPv6。
// 这里用测试服务端把三种写法各跑一遍**完整链路**（握手 + 认证 + 远端 shell），
// 而不是只验证地址解析——解析对了但连不上等于没支持。

#[tokio::test]
async fn connects_over_ipv4_literal() {
	let server = start_on("127.0.0.1:0", TestServerOptions::accepting_both()).await;

	let session = connect(SshParams::new(
		target_at("127.0.0.1", server.port),
		password_spec(PASSWORD),
	))
	.await
	.expect("IPv4 字面量应能连上");

	assert_eq!(session.target().host, "127.0.0.1");
	assert!(
		!session.target().needs_dns().expect("应能判定"),
		"IP 字面量不需要 DNS"
	);
}

#[tokio::test]
async fn connects_over_ipv6_loopback() {
	let server = start_on("[::1]:0", TestServerOptions::accepting_both()).await;

	// 裸写法
	let session = connect(SshParams::new(
		target_at("::1", server.port),
		password_spec(PASSWORD),
	))
	.await
	.expect("IPv6 裸写法应能连上");
	assert!(!session.target().needs_dns().expect("应能判定"));

	// 方括号写法（用户在界面里更可能这么填）
	let bracketed = connect(SshParams::new(
		target_at("[::1]", server.port),
		password_spec(PASSWORD),
	))
	.await
	.expect("IPv6 方括号写法应能连上");
	assert_eq!(bracketed.host_key().fingerprint, HOST_KEY_FINGERPRINT);
}

#[tokio::test]
async fn ipv6_connection_supports_a_full_shell_session() {
	// 只有认证成功不够——通道、PTY、resize 都要在 IPv6 上真的能用
	let server = start_on("[::1]:0", TestServerOptions::accepting_both()).await;
	let session = connect(SshParams::new(
		target_at("::1", server.port),
		password_spec(PASSWORD),
	))
	.await
	.expect("IPv6 连接失败");

	let shell = RemoteShell::open(&session, 100, 30)
		.await
		.expect("IPv6 上打开远端 shell 失败");
	let mut terminal =
		TerminalSession::attach(Box::new(shell), 100, 30, DEFAULT_SCROLLBACK).expect("挂载失败");

	let text = pump_until(&mut terminal, "ready", Duration::from_secs(5)).await;
	assert!(text.contains("ready"), "实际屏幕：{text}");

	terminal.write(b"echo over-ipv6\n").expect("写入失败");
	let text = pump_until(&mut terminal, "over-ipv6", Duration::from_secs(5)).await;
	assert!(text.contains("over-ipv6"), "IPv6 上双向数据不通：{text}");

	terminal.resize(120, 40).expect("resize 失败");
	terminal.write(b"size\n").expect("写入失败");
	let text = pump_until(&mut terminal, "size: 120x40", Duration::from_secs(5)).await;
	assert!(
		text.contains("size: 120x40"),
		"IPv6 上 resize 未传到对端：{text}"
	);

	let seen = server.snapshot();
	assert_eq!(seen.pty_requests.len(), 1);
	assert_eq!(
		(seen.pty_requests[0].cols, seen.pty_requests[0].rows),
		(100, 30)
	);
}

#[tokio::test]
async fn connects_via_domain_name_and_falls_back_between_address_families() {
	// 服务端只监听 IPv4，而 localhost 同时解析出 ::1 与 127.0.0.1——
	// 因此这条用例同时验证了「域名解析」与「一个地址失败后换下一个」。
	let server = start_on("127.0.0.1:0", TestServerOptions::accepting_both()).await;

	let session = connect(SshParams::new(
		target_at("localhost", server.port),
		password_spec(PASSWORD),
	))
	.await
	.expect("域名应能连上（必要时在 IPv6/IPv4 之间回退）");

	assert!(
		session.target().needs_dns().expect("应能判定"),
		"域名需要 DNS"
	);
	assert_eq!(session.host_key().fingerprint, HOST_KEY_FINGERPRINT);
}

#[tokio::test]
async fn prefers_ipv6_is_honoured_by_the_ssh_path() {
	// 与 FreeRDP 的 PreferIPv6OverIPv4 对齐：开着时应优先走 IPv6。
	// 这里只验证开关能落到 Dialer 上并被正确描述，真正的落地次序由
	// daisy-net 的地址重排用例覆盖。
	let params = SshParams::new(target(22), password_spec(PASSWORD)).with_dialer(Box::new(
		daisy_net::DirectDialer::default().with_prefer_ipv6(true),
	));
	assert!(format!("{params:?}").contains("IPv6"), "描述里应体现偏好");
}

// ==================== 主机密钥（步骤 40） ====================

#[tokio::test]
async fn first_connection_reports_the_server_fingerprint() {
	let server = start(TestServerOptions::accepting_both()).await;
	let params = SshParams::new(target(server.port), password_spec(PASSWORD));

	let session = connect(params).await.expect("应能连上测试服务端");

	assert_eq!(
		session.host_key_verdict(),
		HostKeyVerdict::Unknown,
		"库里没有记录时应报「首次连接」"
	);
	assert_eq!(
		session.host_key().fingerprint,
		HOST_KEY_FINGERPRINT,
		"观测到的指纹应与服务端主机密钥一致"
	);
	assert!(
		!session.host_key().base64.is_empty(),
		"应能拿到可入库的公钥"
	);
}

#[tokio::test]
async fn recorded_fingerprint_matches_on_the_second_connection() {
	let server = start(TestServerOptions::accepting_both()).await;

	// 第一次：确认指纹
	let first = connect(SshParams::new(target(server.port), password_spec(PASSWORD)))
		.await
		.expect("首次连接失败");
	let trusted = TrustedHostKey::from_info(first.host_key());
	first.disconnect().await.expect("断开失败");

	// 第二次：带上记录，应判为一致
	let second = connect(
		SshParams::new(target(server.port), password_spec(PASSWORD))
			.with_host_key_policy(HostKeyPolicy::require_match(trusted)),
	)
	.await
	.expect("带记录连接失败");

	assert_eq!(second.host_key_verdict(), HostKeyVerdict::Match);
}

#[tokio::test]
async fn mismatched_fingerprint_is_refused_and_reported() {
	let server = start(TestServerOptions::accepting_both()).await;

	let wrong = TrustedHostKey {
		key_type: "ssh-ed25519".to_string(),
		fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
	};

	let error = connect(
		SshParams::new(target(server.port), password_spec(PASSWORD))
			.with_host_key_policy(HostKeyPolicy::require_match(wrong)),
	)
	.await
	.expect_err("指纹不符必须拒绝连接");

	match error {
		SshError::HostKeyMismatch {
			fingerprint,
			expected_fingerprint,
			..
		} => {
			assert_eq!(
				fingerprint, HOST_KEY_FINGERPRINT,
				"错误里应给出本次观测到的指纹"
			);
			assert!(
				expected_fingerprint.starts_with("SHA256:AAAA"),
				"错误里应给出记录里的指纹：{expected_fingerprint}"
			);
		},
		other => panic!("应为 HostKeyMismatch，实际 {other:?}"),
	}
}

#[tokio::test]
async fn strict_policy_refuses_the_first_connection_with_the_fingerprint() {
	let server = start(TestServerOptions::accepting_both()).await;

	let error = connect(
		SshParams::new(target(server.port), password_spec(PASSWORD))
			.with_host_key_policy(HostKeyPolicy::strict_unknown()),
	)
	.await
	.expect_err("严格模式下首次连接应被拒绝");

	match error {
		SshError::HostKeyUnknown { fingerprint, .. } => {
			assert_eq!(
				fingerprint, HOST_KEY_FINGERPRINT,
				"拒绝时也要把指纹带出来，用户才好核对"
			);
		},
		other => panic!("应为 HostKeyUnknown，实际 {other:?}"),
	}
}

// ==================== 密码认证（步骤 41） ====================

#[tokio::test]
async fn password_auth_succeeds() {
	let server = start(TestServerOptions::accepting_both()).await;
	let session = connect(SshParams::new(target(server.port), password_spec(PASSWORD)))
		.await
		.expect("密码认证应成功");

	assert_eq!(
		session.auth_attempts(),
		&[AuthAttempt {
			method: AuthMethod::Password,
			outcome: AuthOutcome::Success,
		}]
	);

	// 服务端确实收到了这次尝试
	let seen = server.snapshot();
	assert_eq!(seen.password_attempts.len(), 1);
	assert_eq!(seen.password_attempts[0].0, "daisy");
	assert_eq!(seen.password_attempts[0].1, PASSWORD);
}

#[tokio::test]
async fn wrong_password_gives_a_specific_error() {
	let server = start(TestServerOptions::accepting_both()).await;

	let error = connect(SshParams::new(
		target(server.port),
		password_spec("definitely-wrong"),
	))
	.await
	.expect_err("错误密码不应连上");

	match error {
		SshError::PasswordRejected { user, host } => {
			assert_eq!(user, "daisy");
			assert_eq!(host, "127.0.0.1");
		},
		other => panic!("应为 PasswordRejected，实际 {other:?}"),
	}
}

#[tokio::test]
async fn password_against_a_publickey_only_server_is_rejected() {
	// 服务端只开了公钥认证，用密码登录必然失败
	let server = start(TestServerOptions::publickey_only()).await;

	let error = connect(SshParams::new(target(server.port), password_spec(PASSWORD)))
		.await
		.expect_err("只开公钥的服务端不应接受密码");

	assert!(
		matches!(error, SshError::PasswordRejected { .. }),
		"应为 PasswordRejected，实际 {error:?}"
	);
}

// ==================== 公钥认证（步骤 42–44） ====================

#[tokio::test]
async fn publickey_auth_succeeds() {
	let server = start(TestServerOptions::accepting_both()).await;
	let session = connect(SshParams::new(
		target(server.port),
		key_spec("openssh_rsa", None),
	))
	.await
	.expect("公钥认证应成功");

	assert_eq!(
		session.auth_attempts(),
		&[AuthAttempt {
			method: AuthMethod::PublicKey {
				path: fixture("openssh_rsa"),
			},
			outcome: AuthOutcome::Success,
		}]
	);

	let seen = server.snapshot();
	assert_eq!(seen.publickey_attempts, vec!["daisy".to_string()]);
}

#[tokio::test]
async fn encrypted_private_key_authenticates_with_its_passphrase() {
	let loaded = LoadedKey::load(fixture("openssh_rsa_encrypted"), Some(KEY_PASSPHRASE))
		.expect("带口令私钥应能加载");

	let server = start(TestServerOptions {
		password: None,
		authorized_keys: vec![loaded.public_key().clone()],
	})
	.await;

	let session = connect(SshParams::new(
		target(server.port),
		key_spec("openssh_rsa_encrypted", Some(KEY_PASSPHRASE)),
	))
	.await
	.expect("带口令私钥应能完成认证");

	assert_eq!(
		session.auth_attempts().first().map(|a| &a.outcome),
		Some(&AuthOutcome::Success)
	);
}

#[tokio::test]
async fn pkcs1_and_pkcs8_keys_also_authenticate() {
	for name in ["pkcs1_rsa.pem", "pkcs8_rsa.pem"] {
		let loaded = LoadedKey::load(fixture(name), None).expect("夹具应能加载");
		let server = start(TestServerOptions {
			password: None,
			authorized_keys: vec![loaded.public_key().clone()],
		})
		.await;

		let session = connect(SshParams::new(target(server.port), key_spec(name, None)))
			.await
			.unwrap_or_else(|error| panic!("{name} 认证失败：{error}"));

		assert_eq!(
			session.auth_attempts().first().map(|a| &a.outcome),
			Some(&AuthOutcome::Success),
			"{name} 应认证成功"
		);
	}
}

#[tokio::test]
async fn rejected_publickey_falls_back_to_password() {
	// 服务端只认密码：公钥必然被拒，然后应自动改走密码（步骤 45）
	let server = start(TestServerOptions::password_only()).await;

	let session = connect(SshParams::new(
		target(server.port),
		AuthSpec::PublicKeyThenPassword {
			path: fixture("openssh_rsa"),
			passphrase: None,
			password: Zeroizing::new(PASSWORD.to_string()),
		},
	))
	.await
	.expect("应回退到密码并成功");

	let attempts = session.auth_attempts();
	assert_eq!(attempts.len(), 2, "应记录两次尝试：{attempts:?}");
	assert_eq!(
		attempts[0].method,
		AuthMethod::PublicKey {
			path: fixture("openssh_rsa"),
		}
	);
	assert_eq!(attempts[0].outcome, AuthOutcome::Rejected);
	assert_eq!(attempts[1].method, AuthMethod::Password);
	assert_eq!(attempts[1].outcome, AuthOutcome::Success);
}

#[tokio::test]
async fn all_methods_failing_reports_the_attempt_order() {
	let server = start(TestServerOptions::password_only()).await;

	let error = connect(SshParams::new(
		target(server.port),
		AuthSpec::PublicKeyThenPassword {
			path: fixture("openssh_rsa"),
			passphrase: None,
			password: Zeroizing::new("wrong-password".to_string()),
		},
	))
	.await
	.expect_err("两种方式都失败时不应连上");

	match error {
		SshError::AuthExhausted { attempts, .. } => {
			let publickey_at = attempts.find("publickey").expect("应含 publickey");
			let password_at = attempts.find("password").expect("应含 password");
			assert!(
				publickey_at < password_at,
				"顺序应是公钥在前、密码在后：{attempts}"
			);
		},
		other => panic!("应为 AuthExhausted，实际 {other:?}"),
	}
}

#[tokio::test]
async fn missing_key_file_is_reported_clearly_when_it_is_the_only_method() {
	let server = start(TestServerOptions::accepting_both()).await;

	let error = connect(SshParams::new(
		target(server.port),
		key_spec("no-such-key-file", None),
	))
	.await
	.expect_err("私钥不存在时不应连上");

	assert!(
		matches!(error, SshError::KeyIo { .. }),
		"只配了公钥时，应直接报私钥读取失败而不是笼统的认证失败：{error:?}"
	);
}

#[tokio::test]
async fn rsa_sha1_can_be_disabled_and_enabled_explicitly() {
	// 现代策略下仍应连上（测试服务端支持 rsa-sha2-*）
	let server = start(TestServerOptions::accepting_both()).await;
	let modern = connect(
		SshParams::new(target(server.port), key_spec("openssh_rsa", None))
			.with_rsa_policy(RsaSigningPolicy::MODERN),
	)
	.await
	.expect("现代策略下应能连上");
	assert_eq!(modern.auth_attempts()[0].outcome, AuthOutcome::Success);

	// 兼容策略同样应能连上——开关只放宽候选，不该把现代算法挤掉
	let legacy = connect(
		SshParams::new(target(server.port), key_spec("openssh_rsa", None))
			.with_rsa_policy(RsaSigningPolicy::LEGACY),
	)
	.await
	.expect("兼容策略下也应能连上");
	assert_eq!(legacy.auth_attempts()[0].outcome, AuthOutcome::Success);
}

// ==================== 远端 shell（步骤 47–49） ====================

/// 把远端 shell 挂进终端会话，pump 到屏幕出现关键字或超时。
async fn pump_until(session: &mut TerminalSession, needle: &str, timeout: Duration) -> String {
	let deadline = Instant::now() + timeout;
	while Instant::now() < deadline {
		session.pump();
		let text = screen_text(session);
		if text.contains(needle) {
			return text;
		}
		tokio::time::sleep(Duration::from_millis(10)).await;
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

/// 连上测试服务端并从 shell 事件流建一个终端会话。
async fn attach_shell(port: u16, cols: u16, rows: u16) -> (SshSession, TerminalSession) {
	let session = connect(SshParams::new(target(port), password_spec(PASSWORD)))
		.await
		.expect("连接失败");

	let shell = RemoteShell::open(&session, cols, rows)
		.await
		.expect("打开远端 shell 失败");

	let terminal = TerminalSession::attach(Box::new(shell), cols, rows, DEFAULT_SCROLLBACK)
		.expect("挂载终端失败");

	(session, terminal)
}

#[tokio::test]
async fn remote_shell_round_trips_through_the_terminal_grid() {
	let server = start(TestServerOptions::accepting_both()).await;
	let (_session, mut terminal) = attach_shell(server.port, 80, 24).await;

	// 服务端的欢迎语应当出现在网格里
	let text = pump_until(&mut terminal, "ready", Duration::from_secs(5)).await;
	assert!(text.contains("daisy-test-shell ready"), "实际屏幕：{text}");

	// 写一行命令，看回显
	terminal.write(b"echo round-trip\n").expect("写入失败");
	let text = pump_until(&mut terminal, "round-trip", Duration::from_secs(5)).await;
	assert!(text.contains("round-trip"), "实际屏幕：{text}");

	// 服务端确实收到了 PTY 请求，且 TERM 正确
	let seen = server.snapshot();
	assert_eq!(seen.pty_requests.len(), 1, "应收到一次 PTY 请求");
	assert_eq!(seen.pty_requests[0].term, "xterm-256color");
	assert_eq!(
		(seen.pty_requests[0].cols, seen.pty_requests[0].rows),
		(80, 24)
	);
	assert_eq!(seen.shell_requests, 1);
}

/// 远端来数据时要主动叫醒界面，而不是等界面下一次周期性重绘。
///
/// 这一条**故意不去拉数据**（全程不调 `try_recv`）：要证明的正是
/// 「界面自己不来问，也能被通知到」——没有它，输出只能等光标闪烁
/// （500ms）或用户碰鼠标才上屏，交互手感就是慢半拍。
#[tokio::test]
async fn remote_output_wakes_the_interface() {
	let server = start(TestServerOptions::accepting_both()).await;

	let session = connect(SshParams::new(target(server.port), password_spec(PASSWORD)))
		.await
		.expect("连接失败");
	let mut shell = RemoteShell::open(&session, 80, 24)
		.await
		.expect("打开远端 shell 失败");

	let wakes = Arc::new(AtomicUsize::new(0));
	let counter = Arc::clone(&wakes);
	shell.set_waker(Arc::new(move || {
		counter.fetch_add(1, Ordering::AcqRel);
	}));

	// 先让服务端的欢迎横幅落地，免得把「开局那一次」算进来
	tokio::time::sleep(Duration::from_millis(200)).await;
	let before = wakes.load(Ordering::Acquire);

	shell.write(b"echo wake-check\n").expect("写入失败");

	let deadline = Instant::now() + Duration::from_secs(5);
	while wakes.load(Ordering::Acquire) == before && Instant::now() < deadline {
		tokio::time::sleep(Duration::from_millis(10)).await;
	}
	assert!(
		wakes.load(Ordering::Acquire) > before,
		"远端有输出时必须叫醒界面（当前唤醒次数 {before} → {}）",
		wakes.load(Ordering::Acquire)
	);
}

#[tokio::test]
async fn resize_reaches_the_remote_side() {
	let server = start(TestServerOptions::accepting_both()).await;
	let (_session, mut terminal) = attach_shell(server.port, 80, 24).await;

	pump_until(&mut terminal, "ready", Duration::from_secs(5)).await;

	// 先把当前尺寸问出来，确认初始值是 80x24
	terminal.write(b"size\n").expect("写入失败");
	let text = pump_until(&mut terminal, "size: 80x24", Duration::from_secs(5)).await;
	assert!(text.contains("size: 80x24"), "初始尺寸不符：{text}");

	// 改尺寸，远端应当收到 window_change_request
	terminal.resize(120, 40).expect("resize 失败");

	terminal.write(b"size\n").expect("写入失败");
	let text = pump_until(&mut terminal, "size: 120x40", Duration::from_secs(5)).await;
	assert!(text.contains("size: 120x40"), "远端尺寸未跟随：{text}");

	let seen = server.snapshot();
	assert!(
		seen.window_changes.contains(&(120, 40)),
		"服务端应记录到尺寸调整：{:?}",
		seen.window_changes
	);
}

#[tokio::test]
async fn remote_exit_code_is_reported_once() {
	let server = start(TestServerOptions::accepting_both()).await;
	let (_session, mut terminal) = attach_shell(server.port, 80, 24).await;

	pump_until(&mut terminal, "ready", Duration::from_secs(5)).await;
	terminal.write(b"exit\n").expect("写入失败");

	let deadline = Instant::now() + Duration::from_secs(5);
	let mut exit_events = 0;
	while Instant::now() < deadline {
		let update = terminal.pump();
		if update.exited.is_some() {
			exit_events += 1;
		}
		if terminal.is_exited() {
			break;
		}
		tokio::time::sleep(Duration::from_millis(10)).await;
	}

	assert_eq!(terminal.exit_code(), Some(7), "远端退出码应原样上报");
	assert_eq!(exit_events, 1, "退出通知应只上报一次");
	assert!(
		screen_text(&terminal).contains("bye"),
		"退出前的输出应已进入网格"
	);
}

#[tokio::test]
async fn disconnect_ends_the_remote_shell() {
	let server = start(TestServerOptions::accepting_both()).await;
	let (session, mut terminal) = attach_shell(server.port, 80, 24).await;

	pump_until(&mut terminal, "ready", Duration::from_secs(5)).await;

	session.disconnect().await.expect("断开请求失败");
	assert!(
		wait_closed(&session, Duration::from_secs(5)).await,
		"断开后会话应进入关闭状态"
	);

	// 通道结束要能传到终端层，否则界面会一直显示「连接中」
	let deadline = Instant::now() + Duration::from_secs(5);
	while Instant::now() < deadline && !terminal.is_exited() {
		terminal.pump();
		tokio::time::sleep(Duration::from_millis(10)).await;
	}

	assert!(terminal.is_exited(), "通道结束后终端应被告知");
	assert_eq!(
		terminal.exit_code(),
		Some(-1),
		"没收到退出状态时应为 -1，表示非正常结束"
	);
}
