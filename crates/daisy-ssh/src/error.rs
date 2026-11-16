//! SSH 层错误。
//!
//! 设计原则（对应 STEPS.md 步骤 46）：**用户能据此决定下一步做什么**。
//! 「认证失败」这种笼统措辞没有价值——要区分是密码错、密钥被拒、还是算法不匹配。

use std::path::PathBuf;

use thiserror::Error;

/// SSH 层错误。
#[derive(Debug, Error)]
pub enum SshError {
	/// 网络层错误（连接不上、超时、代理失败）。
	#[error("{0}")]
	Net(#[from] daisy_net::NetError),

	/// 目标地址不合法（主机写错、端口越界等）。
	///
	/// 与 [`Self::Net`] 分开是刻意的：地址写错是**用户输入问题**，
	/// 应当在发起连接之前就报出来，而不是等一次 DNS 失败。
	#[error("SSH 目标地址不合法：{0}")]
	InvalidTarget(String),

	/// 协议层错误。多数是服务端行为异常或协商不一致。
	#[error("SSH 协议错误：{0}")]
	Protocol(String),

	/// 主机密钥首次出现（库里没有记录），且策略要求严格核对。
	#[error(
		"首次连接 {host}:{port}，服务器密钥 {key_type} 指纹为 {fingerprint}。\
		 需要你确认后才能保存并继续。"
	)]
	HostKeyUnknown {
		/// 主机名或 IP。
		host: String,
		/// 端口。
		port: u16,
		/// 密钥类型，如 `rsa-sha2-512`。
		key_type: String,
		/// `SHA256:` 开头的指纹。
		fingerprint: String,
	},

	/// 主机密钥与记录不一致。
	///
	/// **这是最危险的一类**：可能是中间人，也可能是服务器重装过。
	/// 因此文案必须明确说清「什么变了」，而不是简单拒绝。
	#[error(
		"主机 {host}:{port} 的密钥与记录不一致！\
		 记录的 {key_type} 指纹是 {expected_fingerprint}，本次收到 {fingerprint}。\
		 可能是服务器重装，也可能是中间人攻击。请核实后再决定是否更新记录。"
	)]
	HostKeyMismatch {
		/// 主机名或 IP。
		host: String,
		/// 端口。
		port: u16,
		/// 密钥类型。
		key_type: String,
		/// 本次观测到的指纹。
		fingerprint: String,
		/// 记录中的指纹。
		expected_fingerprint: String,
	},

	/// 密码被拒绝。
	#[error("用户 {user} 在 {host} 上密码认证被拒绝，请检查密码是否正确")]
	PasswordRejected {
		/// 用户名。
		user: String,
		/// 主机名。
		host: String,
	},

	/// 公钥被拒绝。
	///
	/// 常见原因是服务端 `authorized_keys` 里没有这把公钥，
	/// 而不是「密钥坏了」——文案要把这个可能性说出来。
	#[error(
		"用户 {user} 在 {host} 上公钥认证被拒绝。\
		 请确认该公钥已加入服务端的 ~/.ssh/authorized_keys，且权限正确（600）"
	)]
	PublicKeyRejected {
		/// 用户名。
		user: String,
		/// 主机名。
		host: String,
		/// 使用的私钥路径。
		key_path: PathBuf,
	},

	/// 配置的认证方式全部失败。
	#[error("在 {host} 上 {user} 的认证全部失败，依次尝试了：{attempts}")]
	AuthExhausted {
		/// 用户名。
		user: String,
		/// 主机名。
		host: String,
		/// 已尝试的方式，按顺序拼接。
		attempts: String,
	},

	/// 私钥需要口令但未提供。
	#[error("私钥 {path} 已加密，需要口令")]
	KeyPassphraseRequired {
		/// 私钥路径。
		path: PathBuf,
	},

	/// 私钥口令错误。
	#[error("私钥 {path} 的口令不正确")]
	KeyPassphraseRejected {
		/// 私钥路径。
		path: PathBuf,
	},

	/// 私钥格式无法识别或不受支持。
	#[error("无法解析私钥 {path}：{reason}")]
	KeyFormatUnsupported {
		/// 私钥路径。
		path: PathBuf,
		/// 具体原因。
		reason: String,
	},

	/// 私钥文件读取失败。
	#[error("读取私钥 {path} 失败：{source}")]
	KeyIo {
		/// 私钥路径。
		path: PathBuf,
		/// 底层错误。
		source: std::io::Error,
	},

	/// 服务端只支持 SHA-1 的 `ssh-rsa`，而当前策略不允许。
	#[error(
		"服务器 {host} 只支持 SHA-1 签名的 ssh-rsa，当前策略禁止使用。\
		 如果确认内网可信，请显式开启「兼容旧服务器」开关"
	)]
	RsaSha1NotAllowed {
		/// 主机名。
		host: String,
	},

	/// 通道层错误。
	#[error("SSH 通道错误：{0}")]
	Channel(String),

	/// 文件传输层错误（SFTP）。
	///
	/// 与 [`Self::Channel`] 分开：文件操作的失败绝大多数是**远端文件系统**的事
	/// （不存在、没权限、目录非空），文案要能直接告诉用户该改什么。
	#[error("{0}")]
	Sftp(String),

	/// 经跳板机连接内网目标时失败。
	///
	/// 跳板机本身已连通，但无法在其上到达目标——这两层要分开，
	/// 否则用户分不清是「跳板机连不上」还是「目标连不上」。
	#[error("经跳板机 {jump} 连接目标失败：{reason}")]
	JumpConnect {
		/// 跳板机地址。
		jump: String,
		/// 底层原因。
		reason: String,
	},

	/// 连接已被对端关闭。
	#[error("SSH 连接已断开：{reason}")]
	Disconnected {
		/// 断开原因。
		reason: String,
	},
}

/// SSH 层结果别名。
pub type Result<T, E = SshError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn host_key_mismatch_message_spells_out_both_fingerprints() {
		let error = SshError::HostKeyMismatch {
			host: "10.0.0.1".to_string(),
			port: 22,
			key_type: "ssh-ed25519".to_string(),
			fingerprint: "SHA256:new".to_string(),
			expected_fingerprint: "SHA256:old".to_string(),
		};
		let text = error.to_string();

		assert!(text.contains("SHA256:old"), "应给出记录的指纹：{text}");
		assert!(text.contains("SHA256:new"), "应给出本次的指纹：{text}");
		assert!(text.contains("中间人"), "应提示安全风险：{text}");
	}

	#[test]
	fn public_key_rejection_mentions_authorized_keys() {
		let error = SshError::PublicKeyRejected {
			user: "root".to_string(),
			host: "10.0.0.1".to_string(),
			key_path: PathBuf::from("~/.ssh/id_rsa"),
		};
		assert!(error.to_string().contains("authorized_keys"));
	}

	#[test]
	fn auth_exhausted_lists_the_attempts() {
		let error = SshError::AuthExhausted {
			user: "root".to_string(),
			host: "10.0.0.1".to_string(),
			attempts: "publickey → password".to_string(),
		};
		assert!(error.to_string().contains("publickey → password"));
	}
}
