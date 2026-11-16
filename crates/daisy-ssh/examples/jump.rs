//! S7-55 验收：经跳板机 SSH 会话连接目标主机。
//!
//! 用法：
//!
//! ```bash
//! DAISY_JUMP_HOST=192.168.10.21 DAISY_JUMP_USER=root DAISY_JUMP_PASSWORD='<跳板机密码>' \
//! DAISY_TARGET_HOST=192.168.10.21 DAISY_TARGET_USER=root DAISY_TARGET_PASSWORD='<目标密码>' \
//!   cargo run -q -p daisy-ssh --example jump
//! ```
//!
//! 第一步经 `connect` 连跳板机，第二步用 `JumpDialer` 作为内层拨号器
//! 连目标，最后在目标上跑一条命令验证连通。全程走 Daisy 自己的实现，
//! 不使用系统 `ssh`。

use std::env;

use daisy_ssh::{AuthSpec, HostKeyPolicy, SshParams, SshTarget, connect_via_jump};
use zeroize::Zeroizing;

fn env_required(key: &str) -> String {
	env::var(key).unwrap_or_else(|_| {
		eprintln!("缺少环境变量 {key}");
		std::process::exit(2);
	})
}

fn password_auth(password: &str) -> AuthSpec {
	AuthSpec::Password {
		password: Zeroizing::new(password.to_string()),
	}
}

#[tokio::main]
async fn main() {
	let jump_host = env_required("DAISY_JUMP_HOST");
	let jump_user = env_required("DAISY_JUMP_USER");
	let jump_password = env_required("DAISY_JUMP_PASSWORD");

	let target_host = env_required("DAISY_TARGET_HOST");
	let target_user = env_required("DAISY_TARGET_USER");
	let target_password = env_required("DAISY_TARGET_PASSWORD");

	let jump_params = SshParams::new(
		SshTarget::new(&jump_host, 22, &jump_user),
		password_auth(&jump_password),
	);

	let target = SshTarget::new(&target_host, 22, &target_user);

	println!("跳板机：{jump_user}@{jump_host}");
	println!("目标  ：{target_user}@{target_host}（经跳板机）");
	println!();

	// 目标的主机密钥策略与直连同样处理：库外示例没有 known_hosts 可查，
	// 首次连接接受并上报（生产路径由界面层查库后传入）
	let session = match connect_via_jump(
		jump_params,
		target,
		password_auth(&target_password),
		HostKeyPolicy::first_connection(),
	)
	.await
	{
		Ok(session) => session,
		Err(error) => {
			eprintln!("连接失败：{error}");
			std::process::exit(1);
		},
	};

	println!("会话已建立：{}", session.describe());
	println!("主机密钥指纹：{}", session.host_key().fingerprint);
	println!("认证：");
	for attempt in session.auth_attempts() {
		println!("      {} → {:?}", attempt.method, attempt.outcome);
	}

	// 在目标上跑一条命令，证明链路真的通到目标了
	let mut exec = match daisy_ssh::RemoteShell::exec(&session, "uname -srm && id -un").await {
		Ok(exec) => exec,
		Err(error) => {
			eprintln!("exec 失败：{error}");
			std::process::exit(1);
		},
	};

	match exec.collect(std::time::Duration::from_secs(10)).await {
		Ok((output, code)) => {
			println!();
			println!("远端输出（退出码 {code:?}）：");
			print!("{}", String::from_utf8_lossy(&output));
			println!("经跳板机连接目标：成功");
		},
		Err(error) => {
			eprintln!("执行命令失败：{error}");
			std::process::exit(1);
		},
	}
}
