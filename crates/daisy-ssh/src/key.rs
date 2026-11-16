//! 私钥加载与 RSA 签名算法策略。
//!
//! 对应 STEPS.md 步骤 42–44。
//!
//! ## 支持的格式
//!
//! | 格式 | PEM 头 | 来源 |
//! | --- | --- | --- |
//! | OpenSSH | `BEGIN OPENSSH PRIVATE KEY` | `ssh-keygen` 默认 |
//! | PKCS#1 | `BEGIN RSA PRIVATE KEY` | `openssl genrsa -traditional` |
//! | PKCS#8 | `BEGIN PRIVATE KEY` | `openssl pkcs8 -topk8` |
//! | PuTTY | `PuTTY-User-Key-File-` | PuTTY / WinSCP 导出 |
//!
//! 解析交给 `russh::keys::decode_secret_key`——它已经覆盖了 OpenSSH 的
//! bcrypt-kdf 与 PKCS#5 的加密封装，自己实现只会引入分歧。
//! 本模块负责的是**判定格式**与**把失败翻译成人能看懂的错误**。
//!
//! ## 口令
//!
//! 无法直接从 OpenSSH 格式的 PEM 头判断是否加密——加密前后头完全一样。
//! 因此这里用「不提供口令先试一次」来实测：能解开就是没加密，
//! 报「已加密」就是加密的。这多花一次解析，但换来准确的提示
//! （包括「你给了口令，但这个私钥其实没加密」这种情形）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::keys::ssh_key::{HashAlg, PrivateKey, PublicKey};

use crate::error::{Result, SshError};

/// 私钥文件的封装格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFormat {
	/// OpenSSH 自有格式（`ssh-keygen` 默认输出）。
	Openssh,
	/// PKCS#1（`RSA PRIVATE KEY`）。
	Pkcs1,
	/// PKCS#8（`PRIVATE KEY`）。
	Pkcs8,
	/// PuTTY 的 `.ppk`。
	Putty,
}

impl KeyFormat {
	/// 从 PEM 文本判定格式。
	///
	/// 只扫描文件开头的若干行——真要把整个文件读进来找标记，
	/// 遇到超大文件就是白做功。
	pub fn detect(pem: &str) -> Result<Self> {
		for line in pem.lines().take(64) {
			let line = line.trim();
			if line.is_empty() {
				continue;
			}
			if line.starts_with("PuTTY-User-Key-File-") {
				return Ok(Self::Putty);
			}
			if let Some(rest) = line.strip_prefix("-----BEGIN ") {
				let label = rest
					.strip_suffix("-----")
					.map(str::trim)
					.unwrap_or(rest.trim());
				return match label {
					"OPENSSH PRIVATE KEY" => Ok(Self::Openssh),
					"RSA PRIVATE KEY" | "DSA PRIVATE KEY" | "EC PRIVATE KEY" => Ok(Self::Pkcs1),
					"PRIVATE KEY" | "ENCRYPTED PRIVATE KEY" => Ok(Self::Pkcs8),
					other => Err(SshError::KeyFormatUnsupported {
						path: PathBuf::new(),
						reason: format!("不支持的 PEM 标签：{other}"),
					}),
				};
			}
		}

		Err(SshError::KeyFormatUnsupported {
			path: PathBuf::new(),
			reason: "看起来不是 PEM 格式的私钥文件".to_string(),
		})
	}

	/// 稳定的字符串表示，用于日志与界面展示。
	#[must_use]
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Openssh => "OpenSSH",
			Self::Pkcs1 => "PKCS#1",
			Self::Pkcs8 => "PKCS#8",
			Self::Putty => "PuTTY PPK",
		}
	}
}

/// 已加载的私钥。
pub struct LoadedKey {
	key: Arc<PrivateKey>,
	format: KeyFormat,
	encrypted: bool,
	path: PathBuf,
}

impl LoadedKey {
	/// 从内存中的 PEM 文本加载。
	///
	/// `source` 只用于错误信息里回指来源（通常是文件路径）。
	pub fn from_pem(pem: &str, passphrase: Option<&str>, source: &Path) -> Result<Self> {
		let format = KeyFormat::detect(pem).map_err(|error| relocate(error, source))?;

		// 先不带口令试一次，据此判断是否真的加密。
		// 这里必须匹配**底层**错误：译成 SshError 之后就分不出「加密」了。
		match russh::keys::decode_secret_key(pem, None) {
			Ok(key) => Ok(Self {
				key: Arc::new(key),
				format,
				encrypted: false,
				path: source.to_path_buf(),
			}),

			Err(raw) if is_encrypted_error(&raw) => {
				// 确实加密了，必须有口令
				let Some(passphrase) = passphrase else {
					return Err(SshError::KeyPassphraseRequired {
						path: source.to_path_buf(),
					});
				};

				let key = decode(pem, passphrase, source)?;
				Ok(Self {
					key: Arc::new(key),
					format,
					encrypted: true,
					path: source.to_path_buf(),
				})
			},

			// 不是加密问题，那就是其它格式错误；给不给口令都一样
			Err(raw) => Err(translate(&raw, source)),
		}
	}

	/// 从文件加载。
	pub fn load(path: impl AsRef<Path>, passphrase: Option<&str>) -> Result<Self> {
		let path = path.as_ref();
		let pem = std::fs::read_to_string(path).map_err(|source| SshError::KeyIo {
			path: path.to_path_buf(),
			source,
		})?;
		Self::from_pem(&pem, passphrase, path)
	}

	/// 封装格式。
	#[must_use]
	pub fn format(&self) -> KeyFormat {
		self.format
	}

	/// 该私钥在磁盘上是否加密。
	#[must_use]
	pub fn is_encrypted(&self) -> bool {
		self.encrypted
	}

	/// 私钥路径。
	#[must_use]
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// 对应的公钥。
	#[must_use]
	pub fn public_key(&self) -> &PublicKey {
		self.key.public_key()
	}

	/// 公钥指纹，形如 `SHA256:xxxx`。
	#[must_use]
	pub fn fingerprint(&self) -> String {
		self.public_key().fingerprint(HashAlg::Sha256).to_string()
	}

	/// 密钥算法名，如 `ssh-rsa`、`ssh-ed25519`。
	#[must_use]
	pub fn algorithm(&self) -> String {
		self.key.algorithm().as_str().to_string()
	}

	/// 是否 RSA 密钥。RSA 需要额外的签名算法协商（步骤 44）。
	#[must_use]
	pub fn is_rsa(&self) -> bool {
		self.key.algorithm().is_rsa()
	}

	/// 取出内部私钥。认证时需要 `Arc` 与请求一起发送。
	#[must_use]
	pub fn into_key(self) -> Arc<PrivateKey> {
		self.key
	}

	/// 一句人话描述，用于日志与界面。
	#[must_use]
	pub fn describe(&self) -> String {
		format!(
			"{} 格式的 {}（{}，指纹 {}）",
			self.format.as_str(),
			self.algorithm(),
			if self.encrypted {
				"带口令"
			} else {
				"无口令"
			},
			self.fingerprint()
		)
	}
}

impl std::fmt::Debug for LoadedKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		// 私钥内容绝不进日志，只暴露可公开的元数据
		f.debug_struct("LoadedKey")
			.field("format", &self.format)
			.field("encrypted", &self.encrypted)
			.field("algorithm", &self.algorithm())
			.field("fingerprint", &self.fingerprint())
			.field("path", &self.path)
			.finish()
	}
}

/// RSA 签名算法策略（步骤 44）。
///
/// RFC 8332 起，RSA 用 `rsa-sha2-512` / `rsa-sha2-256`；老的 SHA-1 `ssh-rsa`
/// 已被主流服务端默认禁用。但内网里确实还存在只认 SHA-1 的老设备，
/// 所以给出一个**显式的**开关，而不是默默地放宽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsaSigningPolicy {
	allow_sha1: bool,
}

impl RsaSigningPolicy {
	/// 现代策略：禁用 SHA-1。默认值。
	pub const MODERN: Self = Self { allow_sha1: false };

	/// 兼容策略：允许降级到 SHA-1 的 `ssh-rsa`。
	pub const LEGACY: Self = Self { allow_sha1: true };

	/// 按开关构造。
	#[must_use]
	pub fn new(allow_sha1: bool) -> Self {
		if allow_sha1 {
			Self::LEGACY
		} else {
			Self::MODERN
		}
	}

	/// 是否允许 SHA-1。
	#[must_use]
	pub fn allows_sha1(&self) -> bool {
		self.allow_sha1
	}

	/// 依据服务端能力决定用哪个哈希。
	///
	/// `server_best` 来自 `russh` 的 `best_supported_rsa_hash()`：
	///
	/// | 取值 | 含义 |
	/// | --- | --- |
	/// | `Some(Some(alg))` | 服务端明确支持 `rsa-sha2-512/256` |
	/// | `Some(None)` | 服务端只声明支持 SHA-1 的 `ssh-rsa` |
	/// | `None` | 服务端没声明 `server-sig-algs`，能力未知 |
	///
	/// 返回 `None` 表示使用 SHA-1 的 `ssh-rsa`。
	pub fn resolve(
		&self,
		server_best: Option<Option<HashAlg>>,
		host: &str,
	) -> Result<Option<HashAlg>> {
		match server_best {
			// 服务端给出了明确的哈希，直接用
			Some(Some(alg)) => Ok(Some(alg)),

			// 服务端只支持 SHA-1
			Some(None) => {
				if self.allow_sha1 {
					Ok(None)
				} else {
					Err(SshError::RsaSha1NotAllowed {
						host: host.to_string(),
					})
				}
			},

			// 服务端没声明能力。现代服务端即使不声明也基本都支持
			// rsa-sha2-512（RFC 8332 是强制项），所以默认按现代算法试；
			// 开了兼容开关则反过来优先照顾老设备。
			None if self.allow_sha1 => Ok(None),
			None => Ok(Some(HashAlg::Sha512)),
		}
	}
}

impl Default for RsaSigningPolicy {
	fn default() -> Self {
		Self::MODERN
	}
}

// ==================== 内部实现 ====================

/// 用口令解密并解析私钥，把库的错误翻译成我们的错误。
fn decode(pem: &str, passphrase: &str, source: &Path) -> Result<PrivateKey> {
	russh::keys::decode_secret_key(pem, Some(passphrase)).map_err(|error| translate(&error, source))
}

/// 把 `russh::keys::Error` 归类。
///
/// 口令错误在底层表现为「解密/去填充失败」，没有专门的变体，
/// 因此除了按变体匹配，还要看错误文本兜底。
fn translate(error: &russh::keys::Error, source: &Path) -> SshError {
	use russh::keys::Error as KeysError;
	use russh::keys::ssh_key::Error as SshKeyError;

	let text = error.to_string();
	let lower = text.to_ascii_lowercase();

	if is_encrypted_error(error) {
		return SshError::KeyPassphraseRequired {
			path: source.to_path_buf(),
		};
	}

	// 口令不对的表现：解密去填充失败。
	//
	// OpenSSH 加密私钥走 bcrypt-kdf + AES-CTR，口令错时去填充会失败，
	// 底层报的是 `SshKey(Crypto)`——文案只有 "cryptographic error"，
	// 光看文本认不出来，所以要按变体匹配。
	let wrong_passphrase = matches!(
		error,
		KeysError::SshKey(SshKeyError::Crypto) | KeysError::Unpad(_)
	) || lower.contains("padding")
		|| lower.contains("unpad");

	if wrong_passphrase {
		return SshError::KeyPassphraseRejected {
			path: source.to_path_buf(),
		};
	}

	let reason = match error {
		KeysError::CouldNotReadKey => "密钥内容无法识别".to_string(),
		KeysError::KeyIsCorrupt => "密钥数据损坏或口令不正确".to_string(),
		KeysError::UnsupportedKeyType {
			key_type_string, ..
		} => {
			format!("不支持的密钥类型：{key_type_string}")
		},
		_ => text,
	};

	SshError::KeyFormatUnsupported {
		path: source.to_path_buf(),
		reason,
	}
}

/// 是否为「密钥已加密」这一类错误。
fn is_encrypted_error(error: &russh::keys::Error) -> bool {
	use russh::keys::Error as KeysError;
	use russh::keys::ssh_key::Error as SshKeyError;

	matches!(
		error,
		KeysError::KeyIsEncrypted | KeysError::SshKey(SshKeyError::Encrypted)
	)
}

/// `KeyFormat::detect` 在没有路径信息时产生的错误，这里补回路径。
fn relocate(error: SshError, source: &Path) -> SshError {
	match error {
		SshError::KeyFormatUnsupported { reason, .. } => SshError::KeyFormatUnsupported {
			path: source.to_path_buf(),
			reason,
		},
		other => other,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const OPENSSH_KEY: &str = include_str!("../tests/fixtures/openssh_rsa");
	const OPENSSH_ENCRYPTED: &str = include_str!("../tests/fixtures/openssh_rsa_encrypted");
	const OPENSSH_PUB: &str = include_str!("../tests/fixtures/openssh_rsa.pub");
	const PKCS1_KEY: &str = include_str!("../tests/fixtures/pkcs1_rsa.pem");
	const PKCS8_KEY: &str = include_str!("../tests/fixtures/pkcs8_rsa.pem");

	const PASSPHRASE: &str = "daisy-test-passphrase";

	fn fixture(name: &str) -> PathBuf {
		Path::new(env!("CARGO_MANIFEST_DIR"))
			.join("tests/fixtures")
			.join(name)
	}

	// ---------- 格式判定 ----------

	#[test]
	fn detects_all_three_formats() {
		assert_eq!(
			KeyFormat::detect(OPENSSH_KEY).expect("应识别 OpenSSH"),
			KeyFormat::Openssh
		);
		assert_eq!(
			KeyFormat::detect(PKCS1_KEY).expect("应识别 PKCS#1"),
			KeyFormat::Pkcs1
		);
		assert_eq!(
			KeyFormat::detect(PKCS8_KEY).expect("应识别 PKCS#8"),
			KeyFormat::Pkcs8
		);
		assert_eq!(
			KeyFormat::detect("PuTTY-User-Key-File-3: ssh-rsa").expect("应识别 PPK"),
			KeyFormat::Putty
		);
	}

	#[test]
	fn rejects_non_pem_input() {
		let error = KeyFormat::detect("这不是密钥").expect_err("应被拒绝");
		assert!(
			matches!(error, SshError::KeyFormatUnsupported { .. }),
			"错误类型不符：{error:?}"
		);

		let error = KeyFormat::detect("-----BEGIN CERTIFICATE-----\nxx").expect_err("应被拒绝");
		let text = error.to_string();
		assert!(text.contains("CERTIFICATE"), "应指出不支持的标签：{text}");
	}

	#[test]
	fn format_labels_are_stable() {
		assert_eq!(KeyFormat::Openssh.as_str(), "OpenSSH");
		assert_eq!(KeyFormat::Pkcs1.as_str(), "PKCS#1");
		assert_eq!(KeyFormat::Pkcs8.as_str(), "PKCS#8");
		assert_eq!(KeyFormat::Putty.as_str(), "PuTTY PPK");
	}

	// ---------- 加载（步骤 42） ----------

	#[test]
	fn loads_openssh_format() {
		let key = LoadedKey::from_pem(OPENSSH_KEY, None, &fixture("openssh_rsa"))
			.expect("OpenSSH 格式应能加载");

		assert_eq!(key.format(), KeyFormat::Openssh);
		assert!(!key.is_encrypted(), "这个夹具没有口令");
		assert_eq!(key.algorithm(), "ssh-rsa");
		assert!(key.is_rsa());
		assert!(key.fingerprint().starts_with("SHA256:"));
	}

	#[test]
	fn loads_pkcs1_format() {
		let key = LoadedKey::from_pem(PKCS1_KEY, None, &fixture("pkcs1_rsa.pem"))
			.expect("PKCS#1 格式应能加载");

		assert_eq!(key.format(), KeyFormat::Pkcs1);
		assert_eq!(key.algorithm(), "ssh-rsa");
	}

	#[test]
	fn loads_pkcs8_format() {
		let key = LoadedKey::from_pem(PKCS8_KEY, None, &fixture("pkcs8_rsa.pem"))
			.expect("PKCS#8 格式应能加载");

		assert_eq!(key.format(), KeyFormat::Pkcs8);
		assert_eq!(key.algorithm(), "ssh-rsa");
	}

	#[test]
	fn loaded_key_matches_the_pub_file() {
		// openssh_rsa.pub 是同一对密钥的公钥，用它钉住「推导出的公钥正确」
		let key =
			LoadedKey::from_pem(OPENSSH_KEY, None, &fixture("openssh_rsa")).expect("加载失败");
		let expected = PublicKey::from_openssh(OPENSSH_PUB.trim()).expect("公钥夹具应能解析");

		assert_eq!(
			key.public_key(),
			&expected,
			"私钥推导出的公钥与 .pub 文件不一致"
		);
	}

	#[test]
	fn key_loading_from_path_works() {
		let key = LoadedKey::load(fixture("openssh_rsa"), None).expect("从文件加载失败");
		assert_eq!(key.path(), fixture("openssh_rsa"));
	}

	#[test]
	fn missing_file_is_reported_as_io_error() {
		let error =
			LoadedKey::load(fixture("no-such-key"), None).expect_err("不存在的文件不应成功");
		assert!(
			matches!(error, SshError::KeyIo { .. }),
			"错误类型不符：{error:?}"
		);
	}

	// ---------- 口令（步骤 43） ----------

	#[test]
	fn encrypted_key_requires_a_passphrase() {
		let error = LoadedKey::from_pem(OPENSSH_ENCRYPTED, None, &fixture("openssh_rsa_encrypted"))
			.expect_err("带口令的私钥不应在不给口令时加载成功");

		assert!(
			matches!(error, SshError::KeyPassphraseRequired { .. }),
			"应提示需要口令，实际：{error:?}"
		);
	}

	#[test]
	fn encrypted_key_loads_with_correct_passphrase() {
		let key = LoadedKey::from_pem(
			OPENSSH_ENCRYPTED,
			Some(PASSPHRASE),
			&fixture("openssh_rsa_encrypted"),
		)
		.expect("正确口令应能加载");

		assert!(key.is_encrypted(), "应识别为加密私钥");
		assert_eq!(key.algorithm(), "ssh-rsa");
	}

	#[test]
	fn wrong_passphrase_is_rejected() {
		let error = LoadedKey::from_pem(
			OPENSSH_ENCRYPTED,
			Some("definitely-not-the-passphrase"),
			&fixture("openssh_rsa_encrypted"),
		)
		.expect_err("错误口令不应加载成功");

		assert!(
			matches!(error, SshError::KeyPassphraseRejected { .. }),
			"应提示口令错误，实际：{error:?}"
		);
	}

	#[test]
	fn unused_passphrase_is_tolerated_on_plain_key() {
		// 用户多填了口令不该报错，而且加密标记要保持准确
		let key = LoadedKey::from_pem(OPENSSH_KEY, Some(PASSPHRASE), &fixture("openssh_rsa"))
			.expect("未加密的私钥应忽略多余口令");
		assert!(!key.is_encrypted(), "未加密的私钥不应被标成加密");
	}

	// ---------- 展示 ----------

	#[test]
	fn debug_output_does_not_leak_key_material() {
		let key =
			LoadedKey::from_pem(OPENSSH_KEY, None, &fixture("openssh_rsa")).expect("加载失败");
		let text = format!("{key:?}");

		assert!(text.contains("LoadedKey"), "应带结构名：{text}");
		assert!(text.contains("SHA256:"), "应带指纹：{text}");
		assert!(
			!text.contains("OPENSSH PRIVATE KEY"),
			"Debug 不应包含私钥正文"
		);
		// 夹具正文里的 base64 片段不应出现
		let body_line = OPENSSH_KEY.lines().nth(2).expect("夹具应有第 3 行").trim();
		assert!(!text.contains(body_line), "Debug 不应包含私钥字节");
	}

	// ---------- RSA 算法策略（步骤 44） ----------

	#[test]
	fn modern_policy_uses_server_advertised_hash() {
		let policy = RsaSigningPolicy::MODERN;
		assert_eq!(
			policy
				.resolve(Some(Some(HashAlg::Sha512)), "h")
				.expect("应通过"),
			Some(HashAlg::Sha512)
		);
		assert_eq!(
			policy
				.resolve(Some(Some(HashAlg::Sha256)), "h")
				.expect("应通过"),
			Some(HashAlg::Sha256)
		);
	}

	#[test]
	fn modern_policy_rejects_sha1_only_server() {
		let error = RsaSigningPolicy::MODERN
			.resolve(Some(None), "10.0.0.1")
			.expect_err("只支持 SHA-1 的服务端在现代策略下应被拒绝");

		let text = error.to_string();
		assert!(text.contains("兼容旧服务器"), "应提示开关位置：{text}");
		assert!(text.contains("10.0.0.1"), "应指出是哪台主机：{text}");
	}

	#[test]
	fn legacy_policy_allows_sha1() {
		let policy = RsaSigningPolicy::LEGACY;
		assert!(policy.allows_sha1());
		assert_eq!(
			policy.resolve(Some(None), "h").expect("兼容模式下应允许"),
			None,
			"None 表示使用 SHA-1 的 ssh-rsa"
		);
	}

	#[test]
	fn unknown_server_capability_defaults_by_policy() {
		// 服务端没声明 server-sig-algs
		assert_eq!(
			RsaSigningPolicy::MODERN.resolve(None, "h").expect("应通过"),
			Some(HashAlg::Sha512),
			"现代策略下应默认按 rsa-sha2-512 试"
		);
		assert_eq!(
			RsaSigningPolicy::LEGACY.resolve(None, "h").expect("应通过"),
			None,
			"兼容策略下应优先照顾老设备"
		);
	}

	#[test]
	fn policy_from_bool() {
		assert!(!RsaSigningPolicy::new(false).allows_sha1());
		assert!(RsaSigningPolicy::new(true).allows_sha1());
		assert_eq!(RsaSigningPolicy::default(), RsaSigningPolicy::MODERN);
	}
}
