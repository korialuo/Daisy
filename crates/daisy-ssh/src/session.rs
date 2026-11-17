//! SSH 建连与认证。
//!
//! 对应 STEPS.md 步骤 40–41、45–46、49。
//!
//! ## 一次连接都做了什么
//!
//! ```text
//!   1. 经 Dialer 建立字节流（S7 起可能是代理隧道）
//!   2. SSH 握手 → check_server_key 回调里核对主机密钥
//!   3. 按顺序尝试认证：publickey → password（可只配其一）
//!   4. 返回 SshSession，附带「主机密钥结论」与「每种认证方式的结果」
//! ```
//!
//! 第 4 步的两个副产物是刻意的：界面要能在第一次连接时提示指纹并让用户保存，
//! 也要能在「公钥被拒、改用密码成功」时给出可理解的说明，而不是只说「登录成功」。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use daisy_core::HostAddr;
use daisy_net::{Dialer, DirectDialer, TargetAddr};
use russh::MethodKind;
use russh::client::{self, AuthResult, Config, Handle, Handler};
use russh::keys::PrivateKeyWithHashAlg;
use russh::keys::PublicKeyOrCertificate;
use russh::keys::ssh_key::{Algorithm, HashAlg, PublicKey};
use zeroize::Zeroizing;

use crate::error::{Result, SshError};
use crate::key::{LoadedKey, RsaSigningPolicy};

/// 默认连接与握手超时。
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// SSH 目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
	/// 主机名或 IP。
	pub host: String,
	/// 端口。SSH 默认 22。
	pub port: u16,
	/// 用户名。
	pub user: String,
}

impl SshTarget {
	/// 构造。
	#[must_use]
	pub fn new(host: impl Into<String>, port: u16, user: impl Into<String>) -> Self {
		Self {
			host: host.into(),
			port,
			user: user.into(),
		}
	}

	/// 用默认端口 22 构造。
	#[must_use]
	pub fn default_port(host: impl Into<String>, user: impl Into<String>) -> Self {
		Self::new(host, 22, user)
	}

	/// 校验并规范化主机地址。
	///
	/// 域名 / IPv4 / IPv6 三种写法都接受，IPv6 可带方括号
	/// （`[2001:db8::1]`）。规则来自 [`daisy_core::HostAddr`]，
	/// **与 RDP 侧共用**，避免两条路径行为不一致。
	pub fn addr(&self) -> Result<HostAddr> {
		HostAddr::parse(&self.host).map_err(|error| SshError::InvalidTarget(error.to_string()))
	}

	/// 该主机是否需要 DNS 解析（IP 字面量不需要）。
	pub fn needs_dns(&self) -> Result<bool> {
		Ok(self.addr()?.needs_dns())
	}

	/// `user@host:port`，用于日志与标签页标题。
	#[must_use]
	pub fn display(&self) -> String {
		format!("{}@{}:{}", self.user, self.host, self.port)
	}
}

/// 主机密钥的观测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyInfo {
	/// 算法名，如 `ssh-ed25519`、`rsa-sha2-512`。
	pub key_type: String,
	/// `SHA256:` 开头的指纹。
	pub fingerprint: String,
	/// 公钥的 OpenSSH base64 形式，可直接写进 `known_hosts`。
	pub base64: String,
}

impl HostKeyInfo {
	/// 从 russh 给出的服务器公钥构造。
	fn from_public_key(key: &PublicKey, hash_alg: Option<HashAlg>) -> Self {
		Self {
			key_type: host_key_algorithm_name(key, hash_alg),
			fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
			base64: key.to_openssh().unwrap_or_default(),
		}
	}
}

/// 服务器主机密钥的算法名。
///
/// 同一把 RSA 主机密钥在不同协商结果下叫的名字不同。`Algorithm::as_str()`
/// 只会给出 `ssh-rsa`，需要按协商到的哈希补出 `rsa-sha2-512` / `rsa-sha2-256`。
/// 这个名字只用于展示——**比较用的是指纹**，与算法名无关。
fn host_key_algorithm_name(key: &PublicKey, hash_alg: Option<HashAlg>) -> String {
	match (key.algorithm(), hash_alg) {
		(Algorithm::Rsa { .. }, Some(HashAlg::Sha512)) => "rsa-sha2-512".to_string(),
		(Algorithm::Rsa { .. }, Some(HashAlg::Sha256)) => "rsa-sha2-256".to_string(),
		(algorithm, _) => algorithm.as_str().to_string(),
	}
}

/// 库里已有的主机密钥记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedHostKey {
	/// 记录时的算法名。
	pub key_type: String,
	/// 记录时的指纹。
	pub fingerprint: String,
}

impl TrustedHostKey {
	/// 从一次观测构造，用于「用户确认后保存」。
	#[must_use]
	pub fn from_info(info: &HostKeyInfo) -> Self {
		Self {
			key_type: info.key_type.clone(),
			fingerprint: info.fingerprint.clone(),
		}
	}
}

/// 主机密钥的核对结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyVerdict {
	/// 与记录一致。
	Match,
	/// 首次见到，库里没有记录；本次已接受，**由上层决定是否保存**。
	Unknown,
}

/// 认证方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
	/// 公钥认证。
	PublicKey {
		/// 私钥路径。
		path: PathBuf,
	},
	/// 密码认证。
	Password,
}

impl AuthMethod {
	/// 用于日志与错误文案的名字。
	#[must_use]
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::PublicKey { .. } => "publickey",
			Self::Password => "password",
		}
	}
}

impl std::fmt::Display for AuthMethod {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(self.as_str())
	}
}

/// 某一种认证方式的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
	/// 成功。
	Success,
	/// 被服务端拒绝。
	Rejected,
	/// 没能发起——通常是私钥读不到或口令不对。
	Skipped {
		/// 原因描述。
		reason: String,
	},
}

/// 一次认证尝试的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthAttempt {
	/// 用的哪种方式。
	pub method: AuthMethod,
	/// 结果。
	pub outcome: AuthOutcome,
}

/// 要使用的认证方式。
#[derive(Debug)]
pub enum AuthSpec {
	/// 只用密码。
	Password {
		/// 密码明文。用 [`Zeroizing`] 包裹，离开作用域时自动清零。
		password: Zeroizing<String>,
	},
	/// 只用公钥。
	PublicKey {
		/// 私钥路径。
		path: PathBuf,
		/// 私钥口令（如果加密）。
		passphrase: Option<Zeroizing<String>>,
	},
	/// 两者都配：**先试公钥，失败再试密码**（步骤 45）。
	PublicKeyThenPassword {
		/// 私钥路径。
		path: PathBuf,
		/// 私钥口令。
		passphrase: Option<Zeroizing<String>>,
		/// 密码。
		password: Zeroizing<String>,
	},
}

impl AuthSpec {
	fn path(&self) -> Option<&PathBuf> {
		match self {
			Self::Password { .. } => None,
			Self::PublicKey { path, .. } | Self::PublicKeyThenPassword { path, .. } => Some(path),
		}
	}

	fn passphrase(&self) -> Option<&str> {
		match self {
			Self::Password { .. } => None,
			Self::PublicKey { passphrase, .. } | Self::PublicKeyThenPassword { passphrase, .. } => {
				passphrase.as_deref().map(String::as_str)
			},
		}
	}

	fn password(&self) -> Option<&str> {
		match self {
			Self::Password { password } | Self::PublicKeyThenPassword { password, .. } => {
				Some(password.as_str())
			},
			Self::PublicKey { .. } => None,
		}
	}

	/// 按顺序列出要尝试的手段。
	fn plan(&self) -> Vec<&'static str> {
		match self {
			Self::Password { .. } => vec!["password"],
			Self::PublicKey { .. } => vec!["publickey"],
			Self::PublicKeyThenPassword { .. } => vec!["publickey", "password"],
		}
	}
}

/// 连接参数。
pub struct SshParams {
	/// 目标。
	pub target: SshTarget,
	/// 认证方式。
	pub auth: AuthSpec,
	/// 主机密钥核对策略。
	pub host_key: HostKeyPolicy,
	/// RSA 签名算法策略。
	pub rsa: RsaSigningPolicy,
	/// 连接与握手超时。
	pub connect_timeout: Duration,
	/// 心跳间隔。`None` 表示不发。
	pub keepalive: Option<Duration>,
	/// 到达目标的方式。默认直连；S7 起可换成代理隧道。
	pub dialer: Box<dyn Dialer>,
}

impl std::fmt::Debug for SshParams {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		// auth 与 dialer 都不打印内容：前者可能含口令，后者只有一句描述
		f.debug_struct("SshParams")
			.field("target", &self.target)
			.field("auth_methods", &self.auth.plan())
			.field("host_key", &self.host_key)
			.field("rsa", &self.rsa)
			.field("connect_timeout", &self.connect_timeout)
			.field("keepalive", &self.keepalive)
			.field("dialer", &self.dialer.describe())
			.finish()
	}
}

impl SshParams {
	/// 用直连与默认策略构造。
	#[must_use]
	pub fn new(target: SshTarget, auth: AuthSpec) -> Self {
		Self {
			target,
			auth,
			host_key: HostKeyPolicy::first_connection(),
			rsa: RsaSigningPolicy::MODERN,
			connect_timeout: DEFAULT_CONNECT_TIMEOUT,
			keepalive: Some(Duration::from_secs(30)),
			dialer: Box::new(DirectDialer::default()),
		}
	}

	/// 换成指定的连接方式。
	#[must_use]
	pub fn with_dialer(mut self, dialer: Box<dyn Dialer>) -> Self {
		self.dialer = dialer;
		self
	}

	/// 设置主机密钥策略。
	#[must_use]
	pub fn with_host_key_policy(mut self, policy: HostKeyPolicy) -> Self {
		self.host_key = policy;
		self
	}

	/// 设置 RSA 签名策略（「兼容旧服务器」开关）。
	#[must_use]
	pub fn with_rsa_policy(mut self, policy: RsaSigningPolicy) -> Self {
		self.rsa = policy;
		self
	}
}

/// 主机密钥核对策略。
#[derive(Debug, Clone)]
pub struct HostKeyPolicy {
	/// 库里已有的记录。`None` 表示这台主机是第一次连。
	pub known: Option<TrustedHostKey>,
	/// 首次连接时是否接受。
	///
	/// `true` 对应「提示指纹并允许继续」——**结论会通过
	/// [`HostKeyVerdict::Unknown`] 报给上层**，由上层决定要不要保存。
	/// `false` 对应严格模式，直接拒绝。
	pub accept_unknown: bool,
}

impl HostKeyPolicy {
	/// 首次连接：接受未知密钥，但把结论上报。
	#[must_use]
	pub fn first_connection() -> Self {
		Self {
			known: None,
			accept_unknown: true,
		}
	}

	/// 严格模式：库里没记录就拒绝。
	#[must_use]
	pub fn strict_unknown() -> Self {
		Self {
			known: None,
			accept_unknown: false,
		}
	}

	/// 已有记录，必须匹配。
	#[must_use]
	pub fn require_match(known: TrustedHostKey) -> Self {
		Self {
			known: Some(known),
			accept_unknown: false,
		}
	}
}

/// 一次成功建立的 SSH 会话。
pub struct SshSession {
	handle: Handle<ClientHandler>,
	target: SshTarget,
	host_key: HostKeyInfo,
	verdict: HostKeyVerdict,
	attempts: Vec<AuthAttempt>,
}

impl SshSession {
	/// 远端服务端给出的主机密钥。
	#[must_use]
	pub fn host_key(&self) -> &HostKeyInfo {
		&self.host_key
	}

	/// 主机密钥核对结论。
	#[must_use]
	pub fn host_key_verdict(&self) -> HostKeyVerdict {
		self.verdict
	}

	/// 认证尝试记录，按发生顺序。
	#[must_use]
	pub fn auth_attempts(&self) -> &[AuthAttempt] {
		&self.attempts
	}

	/// 目标。
	#[must_use]
	pub fn target(&self) -> &SshTarget {
		&self.target
	}

	/// 连接是否已被对端关闭（步骤 49 的断线检测）。
	#[must_use]
	pub fn is_closed(&self) -> bool {
		self.handle.is_closed()
	}

	/// 内部 handle。通道层需要它来开 channel。
	pub(crate) fn handle(&self) -> &Handle<ClientHandler> {
		&self.handle
	}

	/// 一句话描述本次连接，用于日志。
	#[must_use]
	pub fn describe(&self) -> String {
		let methods: Vec<&str> = self.attempts.iter().map(|a| a.method.as_str()).collect();
		format!(
			"{}，主机密钥 {}（{}），认证 {}",
			self.target.display(),
			self.host_key.key_type,
			self.host_key.fingerprint,
			if methods.is_empty() {
				"未记录".to_string()
			} else {
				methods.join(" → ")
			}
		)
	}

	/// 断开连接。
	pub async fn disconnect(&self) -> Result<()> {
		self.handle
			.disconnect(russh::Disconnect::ByApplication, "Daisy 主动断开", "")
			.await?;
		Ok(())
	}
}

impl std::fmt::Debug for SshSession {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("SshSession")
			.field("target", &self.target)
			.field("host_key", &self.host_key)
			.field("verdict", &self.verdict)
			.field("closed", &self.is_closed())
			.finish()
	}
}

/// 建立 SSH 会话。
///
/// 成功返回的 [`SshSession`] 里带着主机密钥结论与认证记录，
/// 供上层提示「是否保存指纹」与「用了哪种方式登录」。
pub async fn connect(params: SshParams) -> Result<SshSession> {
	let addr = validate(&params)?;

	let target = TargetAddr::new(addr, params.target.port);
	let observed = Arc::new(Mutex::new(None));

	let handler = ClientHandler {
		host: params.target.host.clone(),
		port: params.target.port,
		policy: params.host_key.clone(),
		observed: Arc::clone(&observed),
	};

	// 先经 Dialer 建立字节流，再交给 russh。
	// 这样 S7 换成代理隧道时，这一层完全不用改。
	let stream = params.dialer.dial(&target).await?;

	let config = Arc::new(Config {
		// 空闲超时给足，长会话不该被客户端自己掐断；
		// 真正的保活用 keepalive 探。
		inactivity_timeout: Some(Duration::from_secs(3600)),
		keepalive_interval: params.keepalive,
		nodelay: true,
		..Config::default()
	});

	let handshake = tokio::time::timeout(
		params.connect_timeout,
		client::connect_stream(config, stream, handler),
	);

	let mut handle = match handshake.await {
		Err(_) => {
			return Err(SshError::Protocol(format!(
				"SSH 握手超时（{} 秒）",
				params.connect_timeout.as_secs()
			)));
		},
		Ok(Ok(handle)) => handle,
		Ok(Err(error)) => return Err(refine_host_key_error(&params, &observed, error)),
	};

	let observation = observed
		.lock()
		.map_err(|_| SshError::Protocol("主机密钥观测锁已损坏".to_string()))?
		.clone()
		.ok_or_else(|| SshError::Protocol("握手完成但没有观测到服务器密钥".to_string()))?;

	let attempts = authenticate(&mut handle, &params).await?;

	Ok(SshSession {
		handle,
		target: params.target,
		host_key: observation.info,
		verdict: observation.verdict,
		attempts,
	})
}

// ==================== 认证 ====================

/// 按 `plan()` 的顺序尝试认证。
///
/// 顺序是刻意的：公钥优先。私钥认证不会在服务端留下失败计数，
/// 也不会把口令送到对端；密码是退路。
async fn authenticate(
	handle: &mut Handle<ClientHandler>,
	params: &SshParams,
) -> Result<Vec<AuthAttempt>> {
	let mut attempts = Vec::new();
	// 私钥加载失败属于「配置问题」，报出来比笼统的「认证失败」有用得多
	let mut config_error: Option<SshError> = None;

	for method in params.auth.plan() {
		match method {
			"publickey" => {
				let path = params
					.auth
					.path()
					.cloned()
					.ok_or_else(|| SshError::Protocol("缺少私钥路径".to_string()))?;

				let loaded = match LoadedKey::load(&path, params.auth.passphrase()) {
					Ok(loaded) => loaded,
					Err(error) => {
						attempts.push(AuthAttempt {
							method: AuthMethod::PublicKey { path: path.clone() },
							outcome: AuthOutcome::Skipped {
								reason: error.to_string(),
							},
						});
						config_error.get_or_insert(error);
						continue;
					},
				};

				tracing::info!("公钥认证：{}", loaded.describe());

				// RSA 需要先问服务端支持哪个哈希（步骤 44）
				let hash = if loaded.is_rsa() {
					let best = handle.best_supported_rsa_hash().await.map_err(|error| {
						SshError::Protocol(format!("协商 RSA 签名算法失败：{error}"))
					})?;
					let resolved = params.rsa.resolve(best, &params.target.host)?;
					// 这条日志在排查「算法不匹配」类故障时是第一条要看的信息
					tracing::info!("RSA 签名算法：{}", rsa_algorithm_name(resolved));
					resolved
				} else {
					None
				};

				let signer = PrivateKeyWithHashAlg::new(loaded.into_key(), hash);
				let result = handle
					.authenticate_publickey(params.target.user.clone(), signer)
					.await
					.map_err(|error| SshError::Protocol(format!("公钥认证请求失败：{error}")))?;

				let method = AuthMethod::PublicKey { path: path.clone() };
				if result.success() {
					attempts.push(AuthAttempt {
						method,
						outcome: AuthOutcome::Success,
					});
					return Ok(attempts);
				}

				// 服务端会告诉客户端「还能试什么」，这是排查认证问题的关键线索
				tracing::debug!("公钥被拒，服务端建议：{}", remaining_of(&result));
				attempts.push(AuthAttempt {
					method,
					outcome: AuthOutcome::Rejected,
				});
			},

			"password" => {
				let Some(password) = params.auth.password() else {
					continue;
				};

				// 注意：russh 的接口收 `Into<String>`，这里必然产生一次堆拷贝，
				// 由 russh 在认证完成后释放。我们自己的 Zeroizing 副本仍会清零。
				let result = handle
					.authenticate_password(params.target.user.clone(), password)
					.await
					.map_err(|error| SshError::Protocol(format!("密码认证请求失败：{error}")))?;

				if result.success() {
					attempts.push(AuthAttempt {
						method: AuthMethod::Password,
						outcome: AuthOutcome::Success,
					});
					return Ok(attempts);
				}

				tracing::debug!("密码被拒，服务端建议：{}", remaining_of(&result));
				attempts.push(AuthAttempt {
					method: AuthMethod::Password,
					outcome: AuthOutcome::Rejected,
				});
			},

			other => {
				return Err(SshError::Protocol(format!("未知的认证方式：{other}")));
			},
		}
	}

	// 全都没成功。如果只配了公钥且私钥本身有问题，直接报那个原因更准确。
	if attempts.len() == 1
		&& let Some(AuthOutcome::Skipped { reason }) = attempts.first().map(|a| &a.outcome)
	{
		let _ = reason;
		if let Some(error) = config_error {
			return Err(error);
		}
	}

	Err(build_auth_error(&params.target, &attempts))
}

/// 把认证记录翻成一个能指导下一步的错误。
fn build_auth_error(target: &SshTarget, attempts: &[AuthAttempt]) -> SshError {
	// 只尝试了一种方式时，给出该方式专属的错误
	if attempts.len() == 1 {
		match attempts.first() {
			Some(AuthAttempt {
				method: AuthMethod::Password,
				outcome: AuthOutcome::Rejected,
			}) => {
				return SshError::PasswordRejected {
					user: target.user.clone(),
					host: target.host.clone(),
				};
			},
			Some(AuthAttempt {
				method: AuthMethod::PublicKey { path },
				outcome: AuthOutcome::Rejected,
			}) => {
				return SshError::PublicKeyRejected {
					user: target.user.clone(),
					host: target.host.clone(),
					key_path: path.clone(),
				};
			},
			_ => {},
		}
	}

	let attempts_text = attempts
		.iter()
		.map(|attempt| match &attempt.outcome {
			AuthOutcome::Success => format!("{}（成功）", attempt.method),
			AuthOutcome::Rejected => format!("{}（被拒）", attempt.method),
			AuthOutcome::Skipped { reason } => format!("{}（未发起：{reason}）", attempt.method),
		})
		.collect::<Vec<_>>()
		.join(" → ");

	SshError::AuthExhausted {
		user: target.user.clone(),
		host: target.host.clone(),
		attempts: attempts_text,
	}
}

// ==================== 主机密钥 ====================

/// 一次主机密钥观测。
#[derive(Debug, Clone)]
struct HostKeyObservation {
	info: HostKeyInfo,
	verdict: HostKeyVerdict,
	/// 若与记录不符，记下记录里的指纹，便于生成错误文案。
	mismatched_with: Option<String>,
}

/// russh 的客户端回调。
pub(crate) struct ClientHandler {
	host: String,
	port: u16,
	policy: HostKeyPolicy,
	observed: Arc<Mutex<Option<HostKeyObservation>>>,
}

impl Handler for ClientHandler {
	type Error = SshError;

	async fn check_server_key(
		&mut self,
		server_public_key: &PublicKeyOrCertificate,
	) -> std::result::Result<bool, Self::Error> {
		let (key, hash_alg) = match server_public_key {
			PublicKeyOrCertificate::PublicKey { key, hash_alg } => (key.clone(), *hash_alg),
			// 证书认证：证书里带的是 KeyData，包成 PublicKey 才能算指纹
			PublicKeyOrCertificate::Certificate(cert) => (
				PublicKey::new(cert.public_key().clone(), String::new()),
				None,
			),
		};

		let info = HostKeyInfo::from_public_key(&key, hash_alg);

		// 指纹是公钥内容的摘要，与签名算法无关——
		// 所以同一个 RSA 主机密钥无论用 ssh-rsa 还是 rsa-sha2-512 出现，
		// 指纹都相同，不会误报不一致。
		let (accept, verdict, mismatched_with) = match &self.policy.known {
			Some(known) => {
				if known.fingerprint == info.fingerprint {
					(true, HostKeyVerdict::Match, None)
				} else {
					tracing::warn!(
						"{}:{} 主机密钥不一致：记录 {}，本次 {}",
						self.host,
						self.port,
						known.fingerprint,
						info.fingerprint
					);
					(
						false,
						HostKeyVerdict::Match,
						Some(known.fingerprint.clone()),
					)
				}
			},
			None => {
				if self.policy.accept_unknown {
					tracing::info!(
						"{}:{} 首次连接，服务器密钥 {} {}",
						self.host,
						self.port,
						info.key_type,
						info.fingerprint
					);
					(true, HostKeyVerdict::Unknown, None)
				} else {
					tracing::warn!(
						"{}:{} 没有主机密钥记录，严格模式下拒绝",
						self.host,
						self.port
					);
					(false, HostKeyVerdict::Unknown, None)
				}
			},
		};

		// 无论接受与否都记下来：连接失败时要靠它生成精确的错误
		if let Ok(mut slot) = self.observed.lock() {
			*slot = Some(HostKeyObservation {
				info,
				verdict,
				mismatched_with,
			});
		}

		Ok(accept)
	}
}

/// 连接失败时，用观测记录把笼统的握手错误换成精确的主机密钥错误。
fn refine_host_key_error(
	params: &SshParams,
	observed: &Arc<Mutex<Option<HostKeyObservation>>>,
	error: SshError,
) -> SshError {
	let Ok(guard) = observed.lock() else {
		return error;
	};
	let Some(observation) = guard.as_ref() else {
		return error;
	};

	// 记录里有 expected → 是不一致
	if let Some(expected) = &observation.mismatched_with {
		return SshError::HostKeyMismatch {
			host: params.target.host.clone(),
			port: params.target.port,
			key_type: observation.info.key_type.clone(),
			fingerprint: observation.info.fingerprint.clone(),
			expected_fingerprint: expected.clone(),
		};
	}

	// 没见过记录且策略拒绝 → 首次连接
	if params.host_key.known.is_none() && !params.host_key.accept_unknown {
		return SshError::HostKeyUnknown {
			host: params.target.host.clone(),
			port: params.target.port,
			key_type: observation.info.key_type.clone(),
			fingerprint: observation.info.fingerprint.clone(),
		};
	}

	error
}

// ==================== 参数校验 ====================

/// 校验参数并返回规范化后的主机地址。
///
/// 主机地址的合法性在**发起连接之前**就判定，因此写错的域名或
/// 畸形的 IPv6 会立刻得到「地址不合法」，而不是等一次 DNS 失败。
fn validate(params: &SshParams) -> Result<HostAddr> {
	if params.target.port == 0 {
		return Err(SshError::InvalidTarget("端口不能为 0".to_string()));
	}
	if params.target.user.trim().is_empty() {
		return Err(SshError::InvalidTarget("用户名为空".to_string()));
	}
	params.target.addr()
}

/// 把 russh 的错误转成我们的错误类型。
impl From<russh::Error> for SshError {
	fn from(error: russh::Error) -> Self {
		Self::Protocol(error.to_string())
	}
}

/// RSA 签名算法的可读名字。
///
/// `None` 表示用 SHA-1 的老 `ssh-rsa`——只在显式开启「兼容旧服务器」时才会出现。
fn rsa_algorithm_name(hash: Option<HashAlg>) -> &'static str {
	match hash {
		Some(HashAlg::Sha512) => "rsa-sha2-512",
		Some(HashAlg::Sha256) => "rsa-sha2-256",
		// HashAlg 目前只有这两种；留出分支以免上游新增时静默走错
		Some(_) => "rsa-sha2-其它",
		None => "ssh-rsa（SHA-1，兼容模式）",
	}
}

/// 把服务端建议的剩余认证方式整理成可读文本，便于排查。
#[must_use]
pub fn describe_remaining(methods: &[MethodKind]) -> String {
	if methods.is_empty() {
		return "服务端未给出剩余可选方式".to_string();
	}
	methods
		.iter()
		.map(|method| format!("{method:?}"))
		.collect::<Vec<_>>()
		.join(", ")
}

/// 认证结果里若被拒，取出服务端建议的下一步方式。
pub(crate) fn remaining_of(result: &AuthResult) -> String {
	match result {
		AuthResult::Success => String::new(),
		AuthResult::Failure {
			remaining_methods,
			partial_success,
		} => {
			let mut text = describe_remaining(remaining_methods);
			if *partial_success {
				text.push_str("（服务端要求继续二次认证）");
			}
			text
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use daisy_core::HostKind;

	#[test]
	fn target_display_includes_user_and_port() {
		let target = SshTarget::new("10.0.0.1", 2222, "root");
		assert_eq!(target.display(), "root@10.0.0.1:2222");
		assert_eq!(SshTarget::default_port("h", "u").port, 22);
	}

	#[test]
	fn params_debug_never_prints_the_password() {
		let params = SshParams::new(
			SshTarget::new("h", 22, "u"),
			AuthSpec::Password {
				password: Zeroizing::new("s3cret-value".to_string()),
			},
		);
		let text = format!("{params:?}");

		assert!(!text.contains("s3cret-value"), "Debug 不应包含口令：{text}");
		assert!(text.contains("password"), "应说明用哪种方式：{text}");
		assert!(text.contains("直连"), "应说明连接路径：{text}");
	}

	#[test]
	fn debug_never_prints_key_passphrase() {
		let params = SshParams::new(
			SshTarget::new("h", 22, "u"),
			AuthSpec::PublicKeyThenPassword {
				path: PathBuf::from("/tmp/id_rsa"),
				passphrase: Some(Zeroizing::new("key-phrase".to_string())),
				password: Zeroizing::new("pw-123".to_string()),
			},
		);
		let text = format!("{params:?}");

		assert!(!text.contains("key-phrase"), "不应含私钥口令：{text}");
		assert!(!text.contains("pw-123"), "不应含密码：{text}");
		// 顺序也要体现：先公钥后密码
		assert!(
			text.contains("publickey") && text.contains("password"),
			"应列出两种方式：{text}"
		);
	}

	#[test]
	fn auth_plan_order_is_publickey_first() {
		let spec = AuthSpec::PublicKeyThenPassword {
			path: PathBuf::from("/tmp/id_rsa"),
			passphrase: None,
			password: Zeroizing::new("p".to_string()),
		};
		assert_eq!(spec.plan(), vec!["publickey", "password"]);
	}

	#[test]
	fn single_method_specs_plan_one_step() {
		let password = AuthSpec::Password {
			password: Zeroizing::new("p".to_string()),
		};
		assert_eq!(password.plan(), vec!["password"]);

		let key = AuthSpec::PublicKey {
			path: PathBuf::from("/tmp/k"),
			passphrase: None,
		};
		assert_eq!(key.plan(), vec!["publickey"]);
	}

	#[test]
	fn validation_rejects_empty_fields() {
		let mut params = SshParams::new(
			SshTarget::new("h", 22, "u"),
			AuthSpec::Password {
				password: Zeroizing::new("p".to_string()),
			},
		);
		assert!(validate(&params).is_ok());

		params.target.host = "   ".to_string();
		assert!(validate(&params).is_err(), "空主机名应被拒绝");

		params.target.host = "h".to_string();
		params.target.port = 0;
		assert!(validate(&params).is_err(), "0 端口应被拒绝");

		params.target.port = 22;
		params.target.user = String::new();
		assert!(validate(&params).is_err(), "空用户名应被拒绝");
	}

	#[test]
	fn target_accepts_domains_and_both_ip_versions() {
		let password = || AuthSpec::Password {
			password: Zeroizing::new("p".to_string()),
		};

		let domain = SshTarget::new("example.com", 22, "u");
		assert_eq!(domain.addr().expect("应能校验").kind(), HostKind::Domain);
		assert!(domain.needs_dns().expect("应能判定"));

		let v4 = SshTarget::new("192.168.10.20", 22, "u");
		assert_eq!(v4.addr().expect("应能校验").kind(), HostKind::V4);
		assert!(!v4.needs_dns().expect("应能判定"));

		// 裸写法与方括号写法都要接受
		let v6_bare = SshTarget::new("2001:db8::1", 22, "u");
		let v6_bracketed = SshTarget::new("[2001:db8::1]", 22, "u");
		assert_eq!(v6_bare.addr().expect("应能校验").kind(), HostKind::V6);
		assert_eq!(
			v6_bare.addr().expect("应能校验"),
			v6_bracketed.addr().expect("应能校验"),
			"两种 IPv6 写法应归一"
		);

		// 完整参数也要能过校验
		let params = SshParams::new(v6_bracketed, password());
		assert!(validate(&params).is_ok());
	}

	#[test]
	fn target_rejects_malformed_hosts() {
		for bad in ["", "   ", "1.2.3.999", "a b.com", "[::1", "host/name"] {
			let target = SshTarget::new(bad, 22, "u");
			let error = target.addr().expect_err(&format!("{bad:?} 应被拒绝"));
			assert!(
				matches!(error, SshError::InvalidTarget(_)),
				"{bad:?} 应报地址不合法，实际 {error:?}"
			);
		}
	}

	#[test]
	fn display_keeps_the_user_written_form() {
		// 展示给用户看的仍是原始输入，不做规范化——用户填什么就回显什么，
		// 否则「我明明填的是 ::1，怎么变成别的了」会引起困惑
		let target = SshTarget::new("Example.COM", 22, "root");
		assert_eq!(target.display(), "root@Example.COM:22");
		assert_eq!(
			target.addr().expect("应能校验").host(),
			"example.com",
			"内部规范化仍然生效"
		);
	}

	#[test]
	fn auth_error_prefers_the_specific_reason() {
		// 只试了密码 → 报密码错误
		let target = SshTarget::new("h", 22, "u");
		let error = build_auth_error(
			&target,
			&[AuthAttempt {
				method: AuthMethod::Password,
				outcome: AuthOutcome::Rejected,
			}],
		);
		assert!(matches!(error, SshError::PasswordRejected { .. }));

		// 只试了公钥 → 报公钥被拒
		let error = build_auth_error(
			&target,
			&[AuthAttempt {
				method: AuthMethod::PublicKey {
					path: PathBuf::from("/tmp/k"),
				},
				outcome: AuthOutcome::Rejected,
			}],
		);
		assert!(matches!(error, SshError::PublicKeyRejected { .. }));

		// 两种都试了 → 报「全部失败」并列出顺序
		let error = build_auth_error(
			&target,
			&[
				AuthAttempt {
					method: AuthMethod::PublicKey {
						path: PathBuf::from("/tmp/k"),
					},
					outcome: AuthOutcome::Rejected,
				},
				AuthAttempt {
					method: AuthMethod::Password,
					outcome: AuthOutcome::Rejected,
				},
			],
		);
		match error {
			SshError::AuthExhausted { attempts, .. } => {
				assert!(attempts.contains("publickey"), "应含 publickey：{attempts}");
				assert!(attempts.contains("password"), "应含 password：{attempts}");
				assert!(
					attempts.find("publickey") < attempts.find("password"),
					"顺序应是公钥在前：{attempts}"
				);
			},
			other => unreachable!("应为 AuthExhausted，实际 {other:?}"),
		}
	}

	#[test]
	fn host_key_policy_constructors() {
		let first = HostKeyPolicy::first_connection();
		assert!(first.known.is_none() && first.accept_unknown);

		let strict = HostKeyPolicy::strict_unknown();
		assert!(strict.known.is_none() && !strict.accept_unknown);

		let known = TrustedHostKey {
			key_type: "ssh-ed25519".to_string(),
			fingerprint: "SHA256:abc".to_string(),
		};
		let require = HostKeyPolicy::require_match(known.clone());
		assert_eq!(require.known, Some(known));
		assert!(!require.accept_unknown);
	}

	#[test]
	fn trusted_key_can_be_built_from_observation() {
		let info = HostKeyInfo {
			key_type: "ssh-ed25519".to_string(),
			fingerprint: "SHA256:xyz".to_string(),
			base64: "AAAA".to_string(),
		};
		let trusted = TrustedHostKey::from_info(&info);
		assert_eq!(trusted.key_type, "ssh-ed25519");
		assert_eq!(trusted.fingerprint, "SHA256:xyz");
	}
}
