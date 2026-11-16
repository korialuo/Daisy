//! Daisy 持久化层。
//!
//! 职责：SQLCipher 加密数据库的建库、schema 迁移、主机与代理的增删改查、
//! 敏感字段（密码、私钥口令）的行内加密。
//!
//! ## 加密分两层（见 PLAN.md §5.3）
//!
//! 1. **全库加密** —— SQLCipher，库密钥由主密钥经 HKDF 派生
//! 2. **字段级二次加密** —— 密码、私钥口令用 AES-256-GCM 单独加密，
//!    即使数据库文件被解开，也不会直接看到明文
//!
//! ## 数据库位置
//!
//! 路径由 [`daisy_core::config::AppPaths`] 提供，跨平台走 `directories` crate：
//!
//! | 平台 | 路径 |
//! | --- | --- |
//! | macOS | `~/Library/Application Support/Daisy/daisy.db` |
//! | Windows | `%APPDATA%\Daisy\daisy.db` |
//!
//! ## 用法
//!
//! ```no_run
//! use daisy_core::config::AppPaths;
//! use daisy_core::key::{EmbeddedKey, KeyProvider};
//! use daisy_store::{Database, HostSecrets};
//! use daisy_core::model::{Host, Protocol};
//!
//! let paths = AppPaths::discover()?;
//! let db = Database::open(&paths, &EmbeddedKey)?;
//!
//! let mut host = Host::draft(Protocol::Rdp);
//! host.name = "测试机".to_string();
//! host.host = "192.168.10.20".to_string();
//! host.username = Some("Administrator".to_string());
//!
//! let id = db.save_host(&host, &HostSecrets::with_password("pw"))?;
//! assert_eq!(db.host_password(id)?.expect("应有密码").as_str(), "pw");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::path::Path;

use daisy_core::config::AppPaths;
use daisy_core::key::KeyProvider;
use daisy_core::model::{Group, Host, Proxy};
use rusqlite::Connection;
use zeroize::Zeroizing;

pub mod crypto;
pub mod error;
pub mod group;
pub mod hosts;
pub mod known_hosts;
pub mod migrate;
pub mod path;
pub mod proxy;
pub mod rdp_cert;

#[cfg(test)]
mod test_support;

pub use crypto::{DerivedKeys, FieldCipher};
pub use error::{Result, StoreError};
pub use hosts::HostSecrets;
pub use path::DatabaseLocation;
pub use rdp_cert::RdpCert;

/// 当前 Unix 秒。用于 `created_at` / `updated_at`。
pub(crate) fn now_unix() -> i64 {
	use std::time::{SystemTime, UNIX_EPOCH};

	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|elapsed| elapsed.as_secs() as i64)
		.unwrap_or(0)
}

/// 已打开的数据库句柄。
///
/// 持有连接与字段加密器。业务代码通过它拿到 [`Connection`] 与 [`FieldCipher`]，
/// 不应自己去 open 数据库——密钥与迁移都在这条路径上完成。
pub struct Database {
	conn: Connection,
	cipher: FieldCipher,
}

impl Database {
	/// 按应用标准目录打开（必要时建库）。
	pub fn open(paths: &AppPaths, provider: &dyn KeyProvider) -> Result<Self> {
		let location = DatabaseLocation::prepare(paths)?;
		Self::open_file(&location.file, provider)
	}

	/// 打开指定文件。
	///
	/// 顺序很关键：`PRAGMA key` 必须在**任何**其他语句之前执行，否则 SQLCipher
	/// 会按未加密库去解析文件头。这里紧接着做一次读，把密钥错误当场暴露，
	/// 而不是拖到后面某条查询才报一个语焉不详的错。
	pub fn open_file(file: &Path, provider: &dyn KeyProvider) -> Result<Self> {
		let master = provider.master_key()?;
		let keys = DerivedKeys::from_master(&master)?;

		let conn = Connection::open(file)?;

		// SQLCipher 官方的原始密钥字面量写法：PRAGMA key = "x'<hex>'"
		let literal = keys.sqlcipher_key_literal();
		let key_statement = format!("PRAGMA key = \"{}\";", literal.as_str());
		conn.execute_batch(&key_statement)?;

		conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
			row.get::<_, i64>(0)
		})
		.map_err(|_| StoreError::Crypto("数据库密钥不正确，无法打开".to_string()))?;

		// 外键约束默认关闭，必须逐连接打开
		conn.execute_batch("PRAGMA foreign_keys = ON;")?;
		// journal_mode 会返回一行，因此不能用 execute_batch
		let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;

		let mut conn = conn;
		migrate::migrate(&mut conn)?;

		Ok(Self {
			conn,
			cipher: FieldCipher::new(*keys.field()),
		})
	}

	/// 底层连接。
	#[must_use]
	pub fn connection(&self) -> &Connection {
		&self.conn
	}

	/// 字段加密器。
	#[must_use]
	pub fn cipher(&self) -> &FieldCipher {
		&self.cipher
	}

	// ==================== 常用便捷方法 ====================
	// 只是把 hosts/proxy/group 模块的调用收拢一下，界面层用这几个就够日常。

	/// 新增或更新主机，返回主键。
	pub fn save_host(&self, host: &Host, secrets: &HostSecrets) -> Result<i64> {
		hosts::upsert(&self.conn, &self.cipher, host, secrets)
	}

	/// 列出全部主机。
	pub fn hosts(&self) -> Result<Vec<Host>> {
		hosts::list(&self.conn)
	}

	/// 按主键读取主机。
	pub fn host(&self, id: i64) -> Result<Host> {
		hosts::get(&self.conn, id)
	}

	/// 删除主机。
	pub fn delete_host(&self, id: i64) -> Result<bool> {
		hosts::delete(&self.conn, id)
	}

	/// 读取解密后的主机密码。
	pub fn host_password(&self, id: i64) -> Result<Option<Zeroizing<String>>> {
		hosts::password(&self.conn, &self.cipher, id)
	}

	/// 读取解密后的私钥口令。
	pub fn host_key_passphrase(&self, id: i64) -> Result<Option<Zeroizing<String>>> {
		hosts::key_passphrase(&self.conn, &self.cipher, id)
	}

	/// 这台机器记录过的 RDP 证书指纹（规范化形式）。`None` 表示首连。
	pub fn rdp_cert_fingerprint(&self, host: &str, port: u16) -> Result<Option<String>> {
		let key = rdp_cert::host_key(host, port);
		Ok(rdp_cert::find(&self.conn, &key)?.map(|record| record.fingerprint))
	}

	/// 记住一台机器的 RDP 证书指纹（首连成功后调用）。
	pub fn remember_rdp_cert(
		&self,
		host: &str,
		port: u16,
		fingerprint: &str,
		subject: Option<&str>,
		issuer: Option<&str>,
	) -> Result<()> {
		rdp_cert::remember(
			&self.conn,
			&rdp_cert::host_key(host, port),
			fingerprint,
			subject,
			issuer,
		)
	}

	/// 忘掉一台机器的 RDP 证书（机器重装后「重新信任」用）。
	pub fn forget_rdp_cert(&self, host: &str, port: u16) -> Result<bool> {
		rdp_cert::forget(&self.conn, &rdp_cert::host_key(host, port))
	}

	/// 列出全部代理。
	pub fn proxies(&self) -> Result<Vec<Proxy>> {
		proxy::list(&self.conn)
	}

	/// 新增或更新代理，返回主键。
	///
	/// `password` 为 `None` 会清空已存密码（整列覆盖）——
	/// 「留空即保持原值」的语义由调用方读回原值兑现。
	pub fn save_proxy(&self, proxy: &Proxy, password: Option<&str>) -> Result<i64> {
		match proxy.id {
			Some(id) => {
				proxy::update(&self.conn, &self.cipher, proxy, password)?;
				Ok(id)
			},
			None => proxy::insert(&self.conn, &self.cipher, proxy, password),
		}
	}

	/// 删除代理。
	pub fn delete_proxy(&self, id: i64) -> Result<bool> {
		proxy::delete(&self.conn, id)
	}

	/// 读取解密后的代理密码。
	pub fn proxy_password(&self, id: i64) -> Result<Option<Zeroizing<String>>> {
		proxy::password(&self.conn, &self.cipher, id)
	}

	/// 列出全部分组。
	pub fn groups(&self) -> Result<Vec<Group>> {
		group::list(&self.conn)
	}

	/// 新建分组。`parent` 为 `None` 时建顶层分组，否则挂为它的子分组。
	///
	/// 只支持两层：`parent` 自身已有上级时报错（见 [`group::insert_with_parent`]）。
	pub fn create_group(&self, name: &str, parent: Option<i64>) -> Result<i64> {
		group::insert_with_parent(&self.conn, name, parent)
	}

	/// 重命名分组。
	pub fn rename_group(&self, id: i64, name: &str) -> Result<()> {
		group::rename(&self.conn, id, name)
	}

	/// 删除分组。子分组级联删除，组内主机退回「未分组」（外键 `SET NULL`）。
	pub fn delete_group(&self, id: i64) -> Result<bool> {
		group::delete(&self.conn, id)
	}
}

impl std::fmt::Debug for Database {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Database")
			.field("cipher", &self.cipher)
			.finish_non_exhaustive()
	}
}

#[cfg(test)]
mod sqlcipher_smoke {
	//! 步骤 19 的构建验证：确认 `rusqlite` 真的链上了 SQLCipher 而非普通 SQLite。
	//!
	//! 光看「编译通过」不够——`bundled-sqlcipher-vendored-openssl` 若被别的特性
	//! 覆盖成普通 bundled sqlite，代码照样能编过，但库文件是明文的。
	//! 所以这里直接读 `PRAGMA cipher_version` 来验明正身。

	use rusqlite::Connection;

	/// 读取 SQLCipher 版本。普通 SQLite 上该 PRAGMA 不存在，返回 `None`。
	fn cipher_version(conn: &Connection) -> Option<String> {
		conn.query_row("PRAGMA cipher_version", [], |row| row.get::<_, String>(0))
			.ok()
	}

	#[test]
	fn sqlcipher_is_really_linked() {
		let conn = Connection::open_in_memory().expect("打开内存库失败");
		let version = cipher_version(&conn)
			.expect("PRAGMA cipher_version 不可用 —— 当前链接的是普通 SQLite 而非 SQLCipher");

		assert!(!version.trim().is_empty(), "cipher_version 为空");
		println!("SQLCipher 版本：{version}");
	}

	#[test]
	fn in_memory_db_works_with_key() {
		let conn = Connection::open_in_memory().expect("打开内存库失败");
		conn.pragma_update(None, "key", "correct horse battery staple")
			.expect("设置密钥失败");

		conn.execute_batch(
			"CREATE TABLE t (v TEXT NOT NULL);
			 INSERT INTO t (v) VALUES ('hello');",
		)
		.expect("建表与写入失败");

		let value: String = conn
			.query_row("SELECT v FROM t", [], |row| row.get(0))
			.expect("查询失败");
		assert_eq!(value, "hello");
	}

	#[test]
	fn wrong_key_cannot_open_existing_file() {
		use tempfile::TempDir;

		let dir = TempDir::new().expect("创建临时目录失败");
		let path = dir.path().join("encrypted.db");

		{
			let conn = Connection::open(&path).expect("建库失败");
			conn.pragma_update(None, "key", "key-one")
				.expect("设置密钥失败");
			conn.execute_batch(
				"CREATE TABLE secret (v TEXT NOT NULL);
				 INSERT INTO secret VALUES ('top');",
			)
			.expect("写入失败");
		}

		{
			let conn = Connection::open(&path).expect("重开失败");
			conn.pragma_update(None, "key", "key-one")
				.expect("设置密钥失败");
			let value: String = conn
				.query_row("SELECT v FROM secret", [], |row| row.get(0))
				.expect("正确密钥应能读取");
			assert_eq!(value, "top");
		}

		// 错误密钥打不开：SQLCipher 惰性验证，首次真正读页面时才报错
		{
			let conn = Connection::open(&path).expect("打开文件本身会成功");
			conn.pragma_update(None, "key", "key-two")
				.expect("设置密钥失败");
			let outcome = conn.query_row("SELECT v FROM secret", [], |row| row.get::<_, String>(0));
			assert!(outcome.is_err(), "错误密钥竟然读出了数据，说明库没被加密");
		}
	}
}

#[cfg(test)]
mod acceptance {
	//! 步骤 28 的验收：库里落盘的数据必须是加密的，应用内读写必须正常。
	//!
	//! 与 `sqlcipher_smoke` 的区别：这里走完整的 [`Database`] 路径
	//! （HKDF 派生 + PRAGMA key + 迁移 + 字段加密），而不是手工设密钥。

	use super::*;
	use daisy_core::key::EmbeddedKey;
	use daisy_core::model::Protocol;
	use tempfile::TempDir;

	fn open_temp(dir: &TempDir) -> Database {
		Database::open_file(&dir.path().join("daisy.db"), &EmbeddedKey).expect("打开数据库失败")
	}

	#[test]
	fn full_round_trip_survives_reopen() {
		let dir = TempDir::new().expect("创建临时目录失败");

		let id = {
			let db = open_temp(&dir);
			let mut host = Host::draft(Protocol::Rdp);
			host.name = "验收机".to_string();
			host.host = "192.168.10.20".to_string();
			host.username = Some("Administrator".to_string());

			let id = db
				.save_host(&host, &HostSecrets::with_password("very-secret-password"))
				.expect("保存失败");

			// 首次建库应已建表
			assert_eq!(db.hosts().expect("列表失败").len(), 1);
			id
		};

		// 重开：密钥与 schema 都应恢复
		let db = open_temp(&dir);
		let host = db.host(id).expect("重开后读取失败");
		assert_eq!(host.name, "验收机");
		assert_eq!(host.host, "192.168.10.20");
		assert_eq!(
			db.host_password(id)
				.expect("解密失败")
				.expect("应有密码")
				.as_str(),
			"very-secret-password"
		);
	}

	#[test]
	fn database_file_is_encrypted_on_disk() {
		let dir = TempDir::new().expect("创建临时目录失败");
		let file = dir.path().join("daisy.db");

		{
			let db = Database::open_file(&file, &EmbeddedKey).expect("打开失败");
			let mut host = Host::draft(Protocol::Ssh);
			host.name = "PLAINTEXT_NAME_MARKER".to_string();
			host.host = "10.1.2.3".to_string();
			db.save_host(
				&host,
				&HostSecrets::with_password("PLAINTEXT_PASSWORD_MARKER"),
			)
			.expect("保存失败");
		}

		let bytes = std::fs::read(&file).expect("读取库文件失败");
		assert!(!bytes.is_empty());

		for marker in [
			b"PLAINTEXT_PASSWORD_MARKER".as_slice(),
			b"PLAINTEXT_NAME_MARKER".as_slice(),
		] {
			assert!(
				!bytes.windows(marker.len()).any(|window| window == marker),
				"库文件里出现了明文标记：{}",
				String::from_utf8_lossy(marker)
			);
		}
	}

	#[test]
	fn sqlite_header_is_not_plaintext() {
		// 未加密的 SQLite 文件以 "SQLite format 3\0" 开头，加密后不应出现
		let dir = TempDir::new().expect("创建临时目录失败");
		let file = dir.path().join("daisy.db");

		Database::open_file(&file, &EmbeddedKey).expect("打开失败");

		let bytes = std::fs::read(&file).expect("读取失败");
		assert!(
			!bytes.starts_with(b"SQLite format 3\0"),
			"库文件带有明文 SQLite 文件头，说明没有被加密"
		);
	}

	#[test]
	fn reopen_with_different_key_fails() {
		use daisy_core::key::MasterKey;

		/// 与 `EmbeddedKey` 不同的一次性密钥提供者。
		#[derive(Debug)]
		struct OtherKey;
		impl KeyProvider for OtherKey {
			fn master_key(&self) -> daisy_core::Result<MasterKey> {
				Ok(MasterKey::from_bytes([0xABu8; 32]))
			}
			fn describe(&self) -> &'static str {
				"测试用错误密钥"
			}
		}

		let dir = TempDir::new().expect("创建临时目录失败");
		let file = dir.path().join("daisy.db");

		Database::open_file(&file, &EmbeddedKey).expect("首次打开失败");

		let err = Database::open_file(&file, &OtherKey).expect_err("错误密钥应当打不开");
		assert!(
			matches!(err, StoreError::Crypto(_)),
			"错误类型不符：{err:?}"
		);
	}

	#[test]
	fn debug_output_hides_cipher_internals() {
		let dir = TempDir::new().expect("创建临时目录失败");
		let db = open_temp(&dir);
		let rendered = format!("{db:?}");
		assert!(rendered.contains("已脱敏"), "Debug 未脱敏：{rendered}");
	}
}
