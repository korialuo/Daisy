//! 主密钥提供者。
//!
//! 存储层用主密钥经 HKDF 派生出两把子密钥：SQLCipher 的库密钥、
//! 敏感字段的行内加密密钥。业务层只依赖 [`KeyProvider`] 抽象，
//! 不关心密钥从哪来。
//!
//! # 安全声明
//!
//! **内置密钥（[`EmbeddedKey`]）本质是混淆，不是加密。**
//! 任何能读到二进制的人都可以还原出这把密钥，进而解密数据库。
//! 该方案只提高门槛，不构成对本地攻击者的防护。
//! 需要真实安全性时应换成系统密钥库实现。
//!
//! # 各平台现状
//!
//! | 平台 | 实现 | 强度 |
//! | --- | --- | --- |
//! | macOS | [`EmbeddedKey`] | 混淆（Keychain 实现尚未接入） |
//! | Windows | [`DpapiKey`] | 真加密：密钥由 DPAPI 绑定当前用户 |
//!
//! 两个实现都满足 [`KeyProvider`]，调用方不需要分平台。

use std::fmt;

use sha2::{Digest, Sha256};

use crate::error::{CoreError, Result};

/// 32 字节主密钥。
///
/// 两道防泄漏措施：
/// - `Debug` 输出被脱敏，密钥不会随日志或 panic 信息外泄
/// - drop 时清零内存，降低驻留窗口
pub struct MasterKey([u8; Self::LEN]);

impl MasterKey {
	/// 密钥长度。
	pub const LEN: usize = 32;

	/// 从字节数组构造。
	#[must_use]
	pub fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
		Self(bytes)
	}

	/// 原始字节。
	///
	/// **调用方不得把它写进日志、错误信息或任何持久化结构。**
	#[must_use]
	pub fn as_bytes(&self) -> &[u8; Self::LEN] {
		&self.0
	}
}

impl Drop for MasterKey {
	fn drop(&mut self) {
		self.0.fill(0);
	}
}

impl fmt::Debug for MasterKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("MasterKey(<已脱敏>)")
	}
}

impl PartialEq for MasterKey {
	fn eq(&self, other: &Self) -> bool {
		self.0 == other.0
	}
}

impl Eq for MasterKey {}

/// 主密钥来源。
pub trait KeyProvider: Send + Sync + fmt::Debug {
	/// 取得主密钥。
	fn master_key(&self) -> Result<MasterKey>;

	/// 人类可读的来源描述，用于设置页展示当前用的哪种密钥。
	fn describe(&self) -> &'static str;
}

/// 内置密钥。
///
/// 种子由编译期常量经 SHA-256 派生，因此**确定且公开**——
/// 这不是机密，只是让 `strings` 之类的工具读不出明文密钥而已。
#[derive(Debug, Default, Clone, Copy)]
pub struct EmbeddedKey;

impl EmbeddedKey {
	/// 派生种子所用的材料。**不是机密。**
	const MATERIAL: &[u8] = b"daisy/embedded-key/v1";
}

impl KeyProvider for EmbeddedKey {
	fn master_key(&self) -> Result<MasterKey> {
		let digest = Sha256::digest(Self::MATERIAL);

		let mut bytes = [0u8; MasterKey::LEN];
		if digest.len() != MasterKey::LEN {
			// SHA-256 输出恒为 32 字节，走到这里说明依赖被换过了
			return Err(CoreError::Internal(format!(
				"密钥派生长度异常：期望 {} 字节，实际 {} 字节",
				MasterKey::LEN,
				digest.len()
			)));
		}
		bytes.copy_from_slice(&digest);

		Ok(MasterKey::from_bytes(bytes))
	}

	fn describe(&self) -> &'static str {
		"内置密钥（混淆，非真实加密）"
	}
}

// ==================== Windows：DPAPI ====================

/// Windows 上用 DPAPI 保护的主密钥。
///
/// 与 [`EmbeddedKey`] 的区别是**真加密**：密钥是运行时随机生成的 32 字节，
/// 用 `CryptProtectData` 按「当前用户」范围加密后落到 `key.bin`。
/// 换一个 Windows 账户、或把文件拷到别的机器，都解不开。
///
/// # 为什么需要知道数据库路径
///
/// 三条退路必须有优先级，否则会把用户的数据锁死：
///
/// 1. `key.bin` 存在 → 解密；**解不开就硬报错**（数据本来也救不回来了，
///    悄悄换个密钥只会得到「文件不是数据库」这种更难查的错误）。
/// 2. `key.bin` 不存在，但数据库已存在 → 这是**早期用内置密钥建的库**，
///    走 [`EmbeddedKey`]，否则升级一次就把旧库锁死了。
/// 3. 两者都不存在 → 全新安装，生成随机密钥并用 DPAPI 保护。
///    DPAPI 本身不可用时（域策略禁用、服务账户等）退回内置密钥并且**不写
///    `key.bin`**，这样下次启动还能重新尝试，不会留下一个打不开的库。
#[cfg(windows)]
#[derive(Debug)]
pub struct DpapiKey {
	/// 被 DPAPI 加密过的密钥文件。
	key_file: std::path::PathBuf,
	/// 数据库文件。只用来判断「这是不是升级上来的旧库」。
	database_file: std::path::PathBuf,
}

#[cfg(windows)]
impl DpapiKey {
	/// 指定密钥文件与数据库文件。
	#[must_use]
	pub fn new(
		key_file: impl Into<std::path::PathBuf>,
		database_file: impl Into<std::path::PathBuf>,
	) -> Self {
		Self {
			key_file: key_file.into(),
			database_file: database_file.into(),
		}
	}

	/// 生成一把新的随机密钥并用 DPAPI 保护后落盘。
	fn create_and_protect(&self) -> Result<MasterKey> {
		let mut bytes = [0u8; MasterKey::LEN];
		getrandom::fill(&mut bytes)
			.map_err(|error| CoreError::Internal(format!("生成随机主密钥失败：{error}")))?;

		let blob = match dpapi::protect(&bytes) {
			Ok(blob) => blob,
			Err(error) => {
				tracing::warn!(
					"DPAPI 加密主密钥失败（{error}），本次退回内置密钥；\
					 数据仍可用，但保护强度回到「混淆」级别"
				);
				return EmbeddedKey.master_key();
			},
		};

		// 先写临时文件再改名：中途崩溃也不会留下半截 key.bin，
		// 否则下次启动会把它当「存在的密钥文件」去解密然后报错。
		let temp = self.key_file.with_extension("tmp");
		if let Some(parent) = self.key_file.parent() {
			std::fs::create_dir_all(parent).map_err(|error| {
				CoreError::InvalidConfig(format!("无法创建密钥目录 {}：{error}", parent.display()))
			})?;
		}
		std::fs::write(&temp, &blob).map_err(|error| {
			CoreError::InvalidConfig(format!("无法写入密钥文件 {}：{error}", temp.display()))
		})?;
		std::fs::rename(&temp, &self.key_file).map_err(|error| {
			CoreError::InvalidConfig(format!(
				"无法保存密钥文件 {}：{error}",
				self.key_file.display()
			))
		})?;

		tracing::info!("已生成主密钥并用 DPAPI 保护：{}", self.key_file.display());
		Ok(MasterKey::from_bytes(bytes))
	}
}

#[cfg(windows)]
impl KeyProvider for DpapiKey {
	fn master_key(&self) -> Result<MasterKey> {
		match std::fs::read(&self.key_file) {
			Ok(blob) => {
				let plain = dpapi::unprotect(&blob).map_err(|error| {
					CoreError::InvalidConfig(format!(
						"无法解密主密钥文件 {}：{error}。该文件与当初创建它的 Windows \
						 用户绑定，换账户或换机器后会解不开；若旧数据不需要了，\
						 删掉该文件与数据库即可重新开始",
						self.key_file.display()
					))
				})?;

				let Ok(bytes) = <[u8; MasterKey::LEN]>::try_from(plain.as_slice()) else {
					return Err(CoreError::InvalidConfig(format!(
						"主密钥文件 {} 内容异常：期望 {} 字节，实际 {} 字节",
						self.key_file.display(),
						MasterKey::LEN,
						plain.len()
					)));
				};
				Ok(MasterKey::from_bytes(bytes))
			},
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
				if self.database_file.exists() {
					tracing::warn!(
						"数据库已存在但没有密钥文件，按早期版本（内置密钥）打开：{}",
						self.database_file.display()
					);
					return EmbeddedKey.master_key();
				}
				self.create_and_protect()
			},
			Err(error) => Err(CoreError::InvalidConfig(format!(
				"无法读取主密钥文件 {}：{error}",
				self.key_file.display()
			))),
		}
	}

	fn describe(&self) -> &'static str {
		"Windows DPAPI（绑定当前用户）"
	}
}

/// DPAPI 的最小封装：只包加解密两个调用。
///
/// 不用 `windows` 这类高层封装是为了少一层依赖与版本约束——
/// 我们需要的就是两个函数与一个结构体。
#[cfg(windows)]
mod dpapi {
	use windows_sys::Win32::Foundation::LocalFree;
	use windows_sys::Win32::Security::Cryptography::{
		CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
	};

	/// 把一段字节包成 `DATA_BLOB`（windows-sys 里叫 `CRYPT_INTEGER_BLOB`）。
	/// 生命周期与调用方一致，DPAPI 只读不写。
	fn as_blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
		CRYPT_INTEGER_BLOB {
			cbData: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
			pbData: bytes.as_ptr().cast_mut(),
		}
	}

	/// 空输出 blob。
	fn empty_blob() -> CRYPT_INTEGER_BLOB {
		CRYPT_INTEGER_BLOB {
			cbData: 0,
			pbData: std::ptr::null_mut(),
		}
	}

	/// 取出 `DATA_BLOB` 里的字节并释放系统分配的内存。
	///
	/// # Safety
	///
	/// 调用方必须保证 `blob.pbData` 是 DPAPI 用 `LocalAlloc` 分配、
	/// 所有权尚未转移给别人的缓冲区。
	unsafe fn take_blob(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
		if blob.pbData.is_null() {
			return Vec::new();
		}
		// SAFETY: 由本函数的调用约定保证（DPAPI 输出参数）
		let out = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec();
		// SAFETY: 同上，这块内存归我们释放
		unsafe {
			LocalFree(blob.pbData.cast());
		}
		out
	}

	/// 用当前用户的 DPAPI 加密。
	pub fn protect(plain: &[u8]) -> std::io::Result<Vec<u8>> {
		let input = as_blob(plain);
		let mut output = empty_blob();

		// SAFETY: 入参出参都是本栈帧里的有效对象；描述、熵、提示结构都传
		// null 表示不使用；CRYPTPROTECT_UI_FORBIDDEN 保证不弹任何 UI。
		let ok = unsafe {
			CryptProtectData(
				&input,
				std::ptr::null(),
				std::ptr::null(),
				std::ptr::null(),
				std::ptr::null(),
				CRYPTPROTECT_UI_FORBIDDEN,
				&mut output,
			)
		};
		if ok == 0 {
			return Err(std::io::Error::last_os_error());
		}

		// SAFETY: 上面刚由 DPAPI 填充，所有权在此转给我们
		Ok(unsafe { take_blob(output) })
	}

	/// 用当前用户的 DPAPI 解密。
	pub fn unprotect(blob: &[u8]) -> std::io::Result<Vec<u8>> {
		let input = as_blob(blob);
		let mut output = empty_blob();

		// SAFETY: 同 protect。第二个参数是「输出描述字符串」的指针，
		// 传 null 表示不要描述。
		let ok = unsafe {
			CryptUnprotectData(
				&input,
				std::ptr::null_mut(),
				std::ptr::null(),
				std::ptr::null(),
				std::ptr::null(),
				CRYPTPROTECT_UI_FORBIDDEN,
				&mut output,
			)
		};
		if ok == 0 {
			return Err(std::io::Error::last_os_error());
		}

		// SAFETY: 同上
		Ok(unsafe { take_blob(output) })
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn embedded_key_returns_32_bytes() {
		let key = EmbeddedKey.master_key().expect("派生失败");
		assert_eq!(key.as_bytes().len(), MasterKey::LEN);
	}

	#[test]
	fn embedded_key_is_deterministic() {
		let a = EmbeddedKey.master_key().expect("派生失败");
		let b = EmbeddedKey.master_key().expect("派生失败");
		assert_eq!(a, b, "同一进程内两次派生应一致");
	}

	#[test]
	fn master_key_debug_is_redacted() {
		let key = EmbeddedKey.master_key().expect("派生失败");
		let rendered = format!("{key:?}");

		assert_eq!(rendered, "MasterKey(<已脱敏>)");

		// 确保没有任何一个字节以十六进制形式出现在 Debug 输出里
		for byte in key.as_bytes() {
			let hex = format!("{byte:02x}");
			assert!(!rendered.contains(&hex), "Debug 输出泄露了密钥字节 {hex}");
		}
	}

	#[test]
	fn key_provider_is_object_safe() {
		// 存储层会以 trait object 持有密钥来源
		let providers: Vec<Box<dyn KeyProvider>> = vec![Box::new(EmbeddedKey)];
		for provider in &providers {
			assert_eq!(
				provider.master_key().expect("派生失败").as_bytes().len(),
				32
			);
			assert!(!provider.describe().is_empty());
		}
	}

	#[test]
	fn key_is_usable_as_trait_bound() {
		fn take(_: &dyn KeyProvider) {}
		take(&EmbeddedKey);
	}

	/// DPAPI 只存在于 Windows 目标，用例也在 Windows 上才跑。
	///
	/// 这些用例在 macOS 的交叉检查里会被**编译**（能挡住 API 用错），
	/// 但要真跑得在 Windows 机器上 `cargo test -p daisy-core`。
	#[cfg(windows)]
	mod windows_dpapi {
		use super::*;
		use tempfile::TempDir;

		fn provider(dir: &TempDir) -> DpapiKey {
			DpapiKey::new(dir.path().join("key.bin"), dir.path().join("daisy.db"))
		}

		#[test]
		fn creates_a_key_then_reads_it_back() {
			let dir = TempDir::new().expect("临时目录");
			let key_file = dir.path().join("key.bin");
			let db_file = dir.path().join("daisy.db");

			let first = DpapiKey::new(&key_file, &db_file)
				.master_key()
				.expect("首次应生成密钥");
			assert!(key_file.exists(), "生成后应留下密钥文件");

			let second = DpapiKey::new(&key_file, &db_file)
				.master_key()
				.expect("再次应读回同一把密钥");
			assert_eq!(first, second, "两次取到的必须是同一把密钥");
		}

		#[test]
		fn key_file_does_not_contain_the_plaintext() {
			let dir = TempDir::new().expect("临时目录");
			let key_file = dir.path().join("key.bin");
			let provider = DpapiKey::new(&key_file, dir.path().join("daisy.db"));
			let key = provider.master_key().expect("生成失败");

			let blob = std::fs::read(&key_file).expect("读密钥文件");
			let needle = key.as_bytes();
			assert!(
				!blob.windows(needle.len()).any(|window| window == needle),
				"密钥文件里出现了明文密钥"
			);
		}

		#[test]
		fn two_installations_get_different_keys() {
			let a = TempDir::new().expect("临时目录");
			let b = TempDir::new().expect("临时目录");

			let key_a = provider(&a).master_key().expect("生成失败");
			let key_b = provider(&b).master_key().expect("生成失败");
			assert_ne!(key_a, key_b, "两台机器（两个数据目录）不应共用密钥");
		}

		#[test]
		fn existing_database_without_key_file_falls_back_to_embedded() {
			let dir = TempDir::new().expect("临时目录");
			let db_file = dir.path().join("daisy.db");
			std::fs::write(&db_file, "旧库").expect("写假数据库");

			let key = DpapiKey::new(dir.path().join("key.bin"), &db_file)
				.master_key()
				.expect("应回退到内置密钥");
			assert_eq!(
				key,
				EmbeddedKey.master_key().expect("派生失败"),
				"早期版本建的库必须还能用内置密钥打开"
			);
			assert!(
				!dir.path().join("key.bin").exists(),
				"回退时不应顺手写一个 key.bin 出来"
			);
		}

		#[test]
		fn corrupted_key_file_is_a_hard_error() {
			let dir = TempDir::new().expect("临时目录");
			let key_file = dir.path().join("key.bin");
			// 数据库已存在 → 走「解密既有密钥」分支
			std::fs::write(&key_file, "\u{0}\u{1}\u{2}\u{3}不是真的 DPAPI 数据").expect("写坏文件");

			let error = DpapiKey::new(&key_file, dir.path().join("daisy.db"))
				.master_key()
				.expect_err("坏掉的密钥文件必须报错，不能悄悄换密钥");
			let message = error.to_string();
			assert!(
				message.contains("无法解密主密钥文件"),
				"错误信息应指出是密钥文件的问题，实际：{message}"
			);
		}
	}
}
