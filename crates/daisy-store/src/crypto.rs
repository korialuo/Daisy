//! 密钥派生与字段级加解密。
//!
//! ## 两层加密
//!
//! ```text
//! MasterKey (32B)
//!    │  HKDF-SHA256，按 info 做域分离
//!    ├─ info="daisy/sqlcipher-key/v1" ──→ 库密钥 ──→ PRAGMA key
//!    └─ info="daisy/field-key/v1"     ──→ 字段密钥 ──→ AES-256-GCM
//! ```
//!
//! 两把子密钥互不可推，因此即使某一层的密钥泄露也不会连累另一层。
//!
//! ## 字段密文格式
//!
//! ```text
//! [ nonce 12B ][ AES-256-GCM 密文 + 16B 认证标签 ]
//! ```
//!
//! nonce 每条消息随机生成，不重用的概率由 96 位随机性保证。

use std::fmt;

// Nonce 必须从 `aead` 取：`aes_gcm::Nonce<Size>` 收的是尺寸类型（U12），
// 而 `aead::Nonce<Cipher>` 收的是算法类型，后者才是我们想要的写法。
use aes_gcm::aead::{Aead, KeyInit, Nonce};
use aes_gcm::{Aes256Gcm, Key};
use daisy_core::key::MasterKey;
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::error::{Result, StoreError};

/// 子密钥长度。
pub const KEY_LEN: usize = 32;
/// AES-GCM nonce 长度。
pub const NONCE_LEN: usize = 12;
/// AES-GCM 认证标签长度。
pub const TAG_LEN: usize = 16;

/// 库密钥的 HKDF info。
const DB_KEY_INFO: &[u8] = b"daisy/sqlcipher-key/v1";
/// 字段密钥的 HKDF info。
const FIELD_KEY_INFO: &[u8] = b"daisy/field-key/v1";
/// HKDF salt。固定值即可——IKM 才是机密，salt 只用于域分离。
const HKDF_SALT: &[u8] = b"daisy/hkdf-salt/v1";

/// 从主密钥派生出的一对子密钥。drop 时清零。
pub struct DerivedKeys {
	database: [u8; KEY_LEN],
	field: [u8; KEY_LEN],
}

impl DerivedKeys {
	/// 按域分离的 info 分别派生两把子密钥。
	pub fn from_master(master: &MasterKey) -> Result<Self> {
		Ok(Self {
			database: expand(master.as_bytes(), DB_KEY_INFO)?,
			field: expand(master.as_bytes(), FIELD_KEY_INFO)?,
		})
	}

	/// 库密钥（供 SQLCipher 使用）。
	#[must_use]
	pub fn database(&self) -> &[u8; KEY_LEN] {
		&self.database
	}

	/// 字段密钥（供 [`FieldCipher`] 使用）。
	#[must_use]
	pub fn field(&self) -> &[u8; KEY_LEN] {
		&self.field
	}

	/// 生成 SQLCipher 的 `PRAGMA key` 字面量：`x'<64 位十六进制>'`。
	///
	/// 直接给原始密钥可以跳过 SQLCipher 内置的 PBKDF2，避免对已经过 HKDF
	/// 的高熵密钥再做一次无意义的慢哈希。
	#[must_use]
	pub fn sqlcipher_key_literal(&self) -> Zeroizing<String> {
		let mut hex = Zeroizing::new(String::with_capacity(2 + KEY_LEN * 2 + 1));
		hex.push_str("x'");
		for byte in self.database {
			hex.push_str(&format!("{byte:02x}"));
		}
		hex.push('\'');
		hex
	}
}

impl Drop for DerivedKeys {
	fn drop(&mut self) {
		self.database.fill(0);
		self.field.fill(0);
	}
}

impl fmt::Debug for DerivedKeys {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("DerivedKeys(<已脱敏>)")
	}
}

fn expand(ikm: &[u8], info: &[u8]) -> Result<[u8; KEY_LEN]> {
	let hkdf = Hkdf::<Sha256>::new(Some(HKDF_SALT), ikm);
	let mut okm = [0u8; KEY_LEN];

	hkdf.expand(info, &mut okm)
		.map_err(|e| StoreError::Crypto(format!("密钥派生失败：{e}")))?;

	Ok(okm)
}

/// 字段级加解密器。
///
/// 密文形如 `nonce || ciphertext || tag`，可直接作为 BLOB 落库。
pub struct FieldCipher {
	key: [u8; KEY_LEN],
}

impl FieldCipher {
	/// 用字段密钥构造。
	#[must_use]
	pub fn new(key: [u8; KEY_LEN]) -> Self {
		Self { key }
	}

	fn aead(&self) -> Aes256Gcm {
		Aes256Gcm::new(&Key::<Aes256Gcm>::from(self.key))
	}

	/// 加密。每次调用都会生成新的随机 nonce，因此**同一明文两次加密结果不同**。
	pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
		let cipher = self.aead();

		let mut nonce_bytes = [0u8; NONCE_LEN];
		getrandom::fill(&mut nonce_bytes)
			.map_err(|e| StoreError::Crypto(format!("获取随机数失败：{e}")))?;

		let nonce = Nonce::<Aes256Gcm>::from(nonce_bytes);
		let ciphertext = cipher
			.encrypt(&nonce, plaintext)
			.map_err(|_| StoreError::Crypto("加密失败".to_string()))?;

		let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
		out.extend_from_slice(&nonce_bytes);
		out.extend_from_slice(&ciphertext);
		Ok(out)
	}

	/// 解密。长度不足或认证失败一律报错，不区分——避免给攻击者额外信息。
	pub fn decrypt(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
		if blob.len() < NONCE_LEN + TAG_LEN {
			return Err(StoreError::Crypto(format!(
				"密文长度异常：{} 字节，至少需要 {} 字节",
				blob.len(),
				NONCE_LEN + TAG_LEN
			)));
		}

		let mut nonce_bytes = [0u8; NONCE_LEN];
		nonce_bytes.copy_from_slice(&blob[..NONCE_LEN]);
		let nonce = Nonce::<Aes256Gcm>::from(nonce_bytes);

		let plaintext = self
			.aead()
			.decrypt(&nonce, &blob[NONCE_LEN..])
			.map_err(|_| StoreError::Crypto("解密失败：数据被篡改或密钥不匹配".to_string()))?;

		Ok(Zeroizing::new(plaintext))
	}

	/// 加密字符串。
	pub fn encrypt_str(&self, plaintext: &str) -> Result<Vec<u8>> {
		self.encrypt(plaintext.as_bytes())
	}

	/// 解密为字符串。
	pub fn decrypt_str(&self, blob: &[u8]) -> Result<Zeroizing<String>> {
		let bytes = self.decrypt(blob)?;
		let text = std::str::from_utf8(&bytes)
			.map_err(|e| StoreError::Crypto(format!("解密结果不是合法 UTF-8：{e}")))?;
		Ok(Zeroizing::new(text.to_string()))
	}

	/// 便捷方法：`None` 原样透传。
	pub fn encrypt_optional_str(&self, value: Option<&str>) -> Result<Option<Vec<u8>>> {
		value.map(|text| self.encrypt_str(text)).transpose()
	}

	/// 便捷方法：`None` 原样透传。
	pub fn decrypt_optional_str(&self, blob: Option<&[u8]>) -> Result<Option<Zeroizing<String>>> {
		blob.map(|bytes| self.decrypt_str(bytes)).transpose()
	}
}

impl Drop for FieldCipher {
	fn drop(&mut self) {
		self.key.fill(0);
	}
}

impl fmt::Debug for FieldCipher {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("FieldCipher(<已脱敏>)")
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use daisy_core::key::{EmbeddedKey, KeyProvider};

	fn field_cipher() -> FieldCipher {
		let master = EmbeddedKey.master_key().expect("派生主密钥失败");
		let keys = DerivedKeys::from_master(&master).expect("派生子密钥失败");
		FieldCipher::new(*keys.field())
	}

	#[test]
	fn derived_keys_are_separated() {
		let master = EmbeddedKey.master_key().expect("派生失败");
		let keys = DerivedKeys::from_master(&master).expect("派生子密钥失败");

		assert_ne!(
			keys.database(),
			keys.field(),
			"库密钥与字段密钥必须不同（域分离失败）"
		);
		assert_eq!(keys.database().len(), KEY_LEN);
	}

	#[test]
	fn derivation_is_deterministic() {
		let master = EmbeddedKey.master_key().expect("派生失败");

		let a = DerivedKeys::from_master(&master).expect("派生失败");
		let b = DerivedKeys::from_master(&master).expect("派生失败");

		assert_eq!(a.database(), b.database());
		assert_eq!(a.field(), b.field());
	}

	#[test]
	fn sqlcipher_literal_is_hex_form() {
		let master = EmbeddedKey.master_key().expect("派生失败");
		let keys = DerivedKeys::from_master(&master).expect("派生失败");

		let literal = keys.sqlcipher_key_literal();
		let hex = literal.as_str();
		assert!(hex.starts_with("x'"), "字面量应以 x' 开头：{hex}");
		assert!(hex.ends_with('\''));
		assert_eq!(hex.len(), 2 + KEY_LEN * 2 + 1);
		assert!(
			hex[2..hex.len() - 1].chars().all(|c| c.is_ascii_hexdigit()),
			"密钥部分应全部是十六进制"
		);
	}

	#[test]
	fn round_trip_restores_plaintext() {
		let cipher = field_cipher();
		let secret = "tmayat 之类的密码 🔐 with unicode";

		let blob = cipher.encrypt_str(secret).expect("加密失败");
		assert_ne!(blob, secret.as_bytes(), "密文不应等于明文");

		let restored = cipher.decrypt_str(&blob).expect("解密失败");
		assert_eq!(restored.as_str(), secret);
	}

	#[test]
	fn encryption_is_randomized() {
		let cipher = field_cipher();
		let secret = "same plaintext";

		let first = cipher.encrypt_str(secret).expect("加密失败");
		let second = cipher.encrypt_str(secret).expect("加密失败");

		assert_ne!(first, second, "两次加密结果相同，说明 nonce 被重用了");
		// 但两者都能解回同一明文
		assert_eq!(
			cipher.decrypt_str(&first).expect("解密失败").as_str(),
			secret
		);
		assert_eq!(
			cipher.decrypt_str(&second).expect("解密失败").as_str(),
			secret
		);
	}

	#[test]
	fn tampered_ciphertext_is_rejected() {
		let cipher = field_cipher();
		let mut blob = cipher.encrypt_str("secret").expect("加密失败");

		let last = blob.len() - 1;
		blob[last] ^= 0x01;

		let outcome = cipher.decrypt_str(&blob);
		assert!(outcome.is_err(), "被篡改的密文竟然解密成功了");
	}

	#[test]
	fn wrong_key_cannot_decrypt() {
		let cipher = field_cipher();
		let blob = cipher.encrypt_str("secret").expect("加密失败");

		let other = FieldCipher::new([0x42u8; KEY_LEN]);
		assert!(other.decrypt_str(&blob).is_err(), "错误密钥竟然解密成功");
	}

	#[test]
	fn short_blob_is_rejected_without_panic() {
		let cipher = field_cipher();
		assert!(cipher.decrypt(&[]).is_err());
		assert!(cipher.decrypt(&[0u8; NONCE_LEN + TAG_LEN - 1]).is_err());
	}

	#[test]
	fn optional_helpers_pass_through_none() {
		let cipher = field_cipher();

		assert!(
			cipher
				.encrypt_optional_str(None)
				.expect("加密失败")
				.is_none()
		);
		assert!(
			cipher
				.decrypt_optional_str(None)
				.expect("解密失败")
				.is_none()
		);

		let blob = cipher.encrypt_optional_str(Some("pw")).expect("加密失败");
		let back = cipher
			.decrypt_optional_str(blob.as_deref())
			.expect("解密失败");
		assert_eq!(back.expect("应有值").as_str(), "pw");
	}

	#[test]
	fn debug_output_is_redacted() {
		let cipher = field_cipher();
		assert_eq!(format!("{cipher:?}"), "FieldCipher(<已脱敏>)");

		let master = EmbeddedKey.master_key().expect("派生失败");
		let keys = DerivedKeys::from_master(&master).expect("派生失败");
		assert_eq!(format!("{keys:?}"), "DerivedKeys(<已脱敏>)");
	}
}
