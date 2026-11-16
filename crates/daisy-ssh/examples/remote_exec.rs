//! 一次性远程命令 —— Daisy SSH 侧的最小可用工具。
//!
//! 存在两个理由：
//!
//! 1. **脚本化运维**：把「连上去跑一条命令」这件事做成一条可组合的命令行，
//!    后续自动化（部署、巡检、验收前置）都用得上
//! 2. **验收自举**：S6-50 要验公钥认证，就得先把测试公钥写进远端的
//!    `authorized_keys`。这件事没有理由借助系统 `ssh`——用我们自己的实现做，
//!    顺带又多跑了一遍真实链路
//!
//! ## 用法
//!
//! ```bash
//! DAISY_SSH_HOST=192.168.10.21 \
//! DAISY_SSH_USER=root \
//! DAISY_SSH_PASSWORD='<密码>' \
//! DAISY_SSH_CMD='uname -srm' \
//!   cargo run -q -p daisy-ssh --example remote_exec
//! ```
//!
//! 退出码等于**远端命令的退出码**；拿不到退出码时返回 1。
//!
//! 注意走的是 `exec` 通道而不是登录 shell，因此 `PATH` / profile 不会加载。

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use daisy_ssh::{AuthSpec, HostKeyPolicy, RemoteShell, SshParams, SshTarget, connect};
use zeroize::Zeroizing;

/// 单条命令的等待上限。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

fn main() {
	std::process::exit(run());
}

fn run() -> i32 {
	let host = match env::var("DAISY_SSH_HOST") {
		Ok(value) if !value.trim().is_empty() => value,
		_ => {
			eprintln!("缺少环境变量 DAISY_SSH_HOST");
			return 2;
		},
	};
	let port: u16 = env::var("DAISY_SSH_PORT")
		.ok()
		.and_then(|value| value.parse().ok())
		.unwrap_or(22);
	let user = match env::var("DAISY_SSH_USER") {
		Ok(value) if !value.trim().is_empty() => value,
		_ => {
			eprintln!("缺少环境变量 DAISY_SSH_USER");
			return 2;
		},
	};
	let command = match env::var("DAISY_SSH_CMD") {
		Ok(value) if !value.trim().is_empty() => value,
		_ => {
			eprintln!("缺少环境变量 DAISY_SSH_CMD");
			return 2;
		},
	};

	let auth = match env::var("DAISY_SSH_KEY") {
		Ok(path) if !path.trim().is_empty() => AuthSpec::PublicKey {
			path: expand_tilde(&path),
			passphrase: env::var("DAISY_SSH_KEY_PASSPHRASE")
				.ok()
				.filter(|value| !value.is_empty())
				.map(Zeroizing::new),
		},
		_ => match env::var("DAISY_SSH_PASSWORD") {
			Ok(password) if !password.is_empty() => AuthSpec::Password {
				password: Zeroizing::new(password),
			},
			_ => {
				eprintln!("需要 DAISY_SSH_KEY 或 DAISY_SSH_PASSWORD");
				return 2;
			},
		},
	};

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
		let params = SshParams::new(SshTarget::new(host, port, user), auth)
			.with_host_key_policy(HostKeyPolicy::first_connection());

		let session = match connect(params).await {
			Ok(session) => session,
			Err(error) => {
				eprintln!("连接失败：{error}");
				return 1;
			},
		};

		let mut channel = match RemoteShell::exec(&session, &command).await {
			Ok(channel) => channel,
			Err(error) => {
				eprintln!("执行失败：{error}");
				return 1;
			},
		};

		let (output, code) = match channel.collect(COMMAND_TIMEOUT).await {
			Ok(result) => result,
			Err(error) => {
				eprintln!("{error}");
				return 1;
			},
		};

		// 远端输出按原样转发到 stdout，不做 ANSI 解析
		let mut stdout = std::io::stdout();
		let _ = stdout.write_all(&output);
		let _ = stdout.flush();

		let _ = session.disconnect().await;

		match code {
			Some(status) => status,
			None => {
				eprintln!("未收到远端退出状态");
				1
			},
		}
	})
}

/// 展开 `~`。
fn expand_tilde(path: &str) -> PathBuf {
	if let Some(rest) = path.strip_prefix("~/")
		&& let Ok(home) = env::var("HOME")
	{
		return PathBuf::from(home).join(rest);
	}
	PathBuf::from(path)
}
