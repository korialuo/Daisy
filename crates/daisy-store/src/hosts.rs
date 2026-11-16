//! 主机 CRUD。
//!
//! ## 敏感字段怎么走
//!
//! [`Host`] 模型里**没有**密码和私钥口令，它们通过 [`HostSecrets`] 单独传入，
//! 落库前用 [`FieldCipher`] 加密成 BLOB。读取时用 [`password`] / [`key_passphrase`]
//! 单独取，返回 `Zeroizing<String>`，离开作用域即擦除。
//!
//! 这样安排的好处：`Host` 可以随意 `Debug`、序列化、塞进日志或事件里，
//! 不会把凭据带出去。

use std::fmt;

use daisy_core::model::{
	AuthMethod, DisplaySettings, Host, Protocol, PublicKeyAuth, Resolution, ScaleMode,
	ScaleSettings, DEFAULT_DPI, validate_dpi,
};
use rusqlite::{Connection, Row, params};
use zeroize::Zeroizing;

use crate::crypto::FieldCipher;
use crate::error::{Result, StoreError};
use crate::now_unix;

/// 主机表全部列，顺序与 [`read_host`] 一致。
const COLUMNS: &str = "id, name, protocol, host, port, username, auth_method, \
                      key_path, key_passphrase_enc IS NOT NULL, \
                      proxy_id, jump_host_id, group_id, \
                      resolution, scale_mode, scale_factor, clipboard, dpi, created_at, updated_at";

/// 写入主机时一并提交的敏感字段。
///
/// 内容只在加解密的一瞬间存在，`Zeroizing` 会在 drop 时擦除内存。
#[derive(Default)]
pub struct HostSecrets {
	/// 登录密码。
	pub password: Option<Zeroizing<String>>,
	/// 私钥口令。
	pub key_passphrase: Option<Zeroizing<String>>,
}

impl HostSecrets {
	/// 无任何凭据（公钥免密登录等场景）。
	#[must_use]
	pub fn none() -> Self {
		Self::default()
	}

	/// 只带密码。
	#[must_use]
	pub fn with_password(password: impl Into<String>) -> Self {
		Self {
			password: Some(Zeroizing::new(password.into())),
			key_passphrase: None,
		}
	}

	/// 只带私钥口令。
	#[must_use]
	pub fn with_passphrase(passphrase: impl Into<String>) -> Self {
		Self {
			password: None,
			key_passphrase: Some(Zeroizing::new(passphrase.into())),
		}
	}

	/// 密码明文，仅供加密使用。
	pub(crate) fn password_plaintext(&self) -> Option<&str> {
		self.password.as_deref().map(String::as_str)
	}

	/// 私钥口令明文，仅供加密使用。
	pub(crate) fn passphrase_plaintext(&self) -> Option<&str> {
		self.key_passphrase.as_deref().map(String::as_str)
	}
}

impl fmt::Debug for HostSecrets {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		// 只报告「有没有」，绝不打内容
		f.debug_struct("HostSecrets")
			.field("password", &self.password.as_ref().map(|_| "<已设置>"))
			.field(
				"key_passphrase",
				&self.key_passphrase.as_ref().map(|_| "<已设置>"),
			)
			.finish()
	}
}

/// 新增主机，返回新主键。
///
/// `host.id` 会被忽略；`created_at` / `updated_at` 为 0 时自动填当前时间。
pub fn insert(
	conn: &Connection,
	cipher: &FieldCipher,
	host: &Host,
	secrets: &HostSecrets,
) -> Result<i64> {
	host.validate()?;

	let now = now_unix();
	let created_at = if host.created_at == 0 {
		now
	} else {
		host.created_at
	};
	let updated_at = if host.updated_at == 0 {
		now
	} else {
		host.updated_at
	};

	let (auth_method, key_path) = auth_columns(&host.auth);

	conn.execute(
		"INSERT INTO hosts (
		     name, protocol, host, port, username, auth_method,
		     secret_enc, key_path, key_passphrase_enc,
		     proxy_id, jump_host_id, group_id,
		     resolution, scale_mode, scale_factor, extra,
		     clipboard, dpi, created_at, updated_at
		 ) VALUES (
		     ?1, ?2, ?3, ?4, ?5, ?6,
		     ?7, ?8, ?9,
		     ?10, ?11, ?12,
		     ?13, ?14, ?15, NULL,
		     ?16, ?17, ?18, ?19
		 )",
		params![
			host.name,
			host.protocol.as_str(),
			host.host,
			host.port,
			host.username,
			auth_method,
			cipher.encrypt_optional_str(secrets.password_plaintext())?,
			key_path,
			cipher.encrypt_optional_str(secrets.passphrase_plaintext())?,
			host.proxy_id,
			host.jump_host_id,
			host.group_id,
			host.display.resolution.to_string(),
			host.display.scale.mode.as_str(),
			f64::from(host.display.scale.factor),
			i64::from(host.clipboard),
			i64::from(host.display.dpi),
			created_at,
			updated_at,
		],
	)?;

	Ok(conn.last_insert_rowid())
}

/// 按主键更新。主键缺失视为调用错误。
///
/// 传入的 [`HostSecrets`] 为 `None` 的字段会被清空——需要「保持原密码不变」时
/// 请先读出旧值再一起传回来。
pub fn update(
	conn: &Connection,
	cipher: &FieldCipher,
	host: &Host,
	secrets: &HostSecrets,
) -> Result<()> {
	host.validate()?;

	let id = host
		.id
		.ok_or_else(|| StoreError::Format("更新主机时缺少主键".to_string()))?;

	let now = now_unix();
	let updated_at = if host.updated_at == 0 {
		now
	} else {
		host.updated_at
	};
	let (auth_method, key_path) = auth_columns(&host.auth);

	let affected = conn.execute(
		"UPDATE hosts SET
		     name = ?1, protocol = ?2, host = ?3, port = ?4, username = ?5,
		     auth_method = ?6, secret_enc = ?7, key_path = ?8, key_passphrase_enc = ?9,
		     proxy_id = ?10, jump_host_id = ?11, group_id = ?12,
		     resolution = ?13, scale_mode = ?14, scale_factor = ?15, clipboard = ?16,
		     dpi = ?17, updated_at = ?18
		 WHERE id = ?19",
		params![
			host.name,
			host.protocol.as_str(),
			host.host,
			host.port,
			host.username,
			auth_method,
			cipher.encrypt_optional_str(secrets.password_plaintext())?,
			key_path,
			cipher.encrypt_optional_str(secrets.passphrase_plaintext())?,
			host.proxy_id,
			host.jump_host_id,
			host.group_id,
			host.display.resolution.to_string(),
			host.display.scale.mode.as_str(),
			f64::from(host.display.scale.factor),
			i64::from(host.clipboard),
			i64::from(host.display.dpi),
			updated_at,
			id,
		],
	)?;

	if affected == 0 {
		return Err(StoreError::NotFound {
			entity: "主机", id
		});
	}
	Ok(())
}

/// 有 `id` 则更新，否则新增。返回值为主键。
pub fn upsert(
	conn: &Connection,
	cipher: &FieldCipher,
	host: &Host,
	secrets: &HostSecrets,
) -> Result<i64> {
	match host.id {
		Some(id) => {
			update(conn, cipher, host, secrets)?;
			Ok(id)
		},
		None => insert(conn, cipher, host, secrets),
	}
}

/// 按主键查询。
pub fn find(conn: &Connection, id: i64) -> Result<Option<Host>> {
	let sql = format!("SELECT {COLUMNS} FROM hosts WHERE id = ?1");
	let mut stmt = conn.prepare(&sql)?;
	let mut rows = stmt.query(params![id])?;

	match rows.next()? {
		Some(row) => Ok(Some(read_host(row)?)),
		None => Ok(None),
	}
}

/// 按主键查询，缺失即报错。
pub fn get(conn: &Connection, id: i64) -> Result<Host> {
	find(conn, id)?.ok_or(StoreError::NotFound {
		entity: "主机", id
	})
}

/// 列出全部主机，按名称排序。
pub fn list(conn: &Connection) -> Result<Vec<Host>> {
	let sql = format!("SELECT {COLUMNS} FROM hosts ORDER BY name COLLATE NOCASE");
	collect(conn, &sql, params![])
}

/// 列出指定分组下的主机。`None` 表示未分组。
pub fn list_in_group(conn: &Connection, group_id: Option<i64>) -> Result<Vec<Host>> {
	let sql = match group_id {
		Some(_) => format!("SELECT {COLUMNS} FROM hosts WHERE group_id = ?1 ORDER BY name"),
		None => format!("SELECT {COLUMNS} FROM hosts WHERE group_id IS NULL ORDER BY name"),
	};
	match group_id {
		Some(id) => collect(conn, &sql, params![id]),
		None => collect(conn, &sql, params![]),
	}
}

/// 按主键删除。返回是否真的删掉了一行。
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
	let affected = conn.execute("DELETE FROM hosts WHERE id = ?1", params![id])?;
	Ok(affected > 0)
}

/// 读取解密后的登录密码。未设置时返回 `None`。
pub fn password(
	conn: &Connection,
	cipher: &FieldCipher,
	id: i64,
) -> Result<Option<Zeroizing<String>>> {
	read_secret(conn, cipher, "secret_enc", id)
}

/// 读取解密后的私钥口令。未设置时返回 `None`。
pub fn key_passphrase(
	conn: &Connection,
	cipher: &FieldCipher,
	id: i64,
) -> Result<Option<Zeroizing<String>>> {
	read_secret(conn, cipher, "key_passphrase_enc", id)
}

// ==================== 内部实现 ====================

fn auth_columns(auth: &AuthMethod) -> (&'static str, Option<&str>) {
	match auth {
		AuthMethod::Password => (auth.as_str(), None),
		AuthMethod::PublicKey(pk) => (auth.as_str(), Some(pk.key_path.as_str())),
	}
}

fn read_secret(
	conn: &Connection,
	cipher: &FieldCipher,
	column: &str,
	id: i64,
) -> Result<Option<Zeroizing<String>>> {
	// column 是本文件内的字面量，不涉及外部输入
	let sql = format!("SELECT {column} FROM hosts WHERE id = ?1");
	let mut stmt = conn.prepare(&sql)?;
	let mut rows = stmt.query(params![id])?;

	let Some(row) = rows.next()? else {
		return Err(StoreError::NotFound {
			entity: "主机", id
		});
	};

	let blob = row.get::<_, Option<Vec<u8>>>(0)?;
	cipher.decrypt_optional_str(blob.as_deref())
}

fn collect(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Host>> {
	let mut stmt = conn.prepare(sql)?;
	let mut rows = stmt.query(params)?;

	let mut out = Vec::new();
	while let Some(row) = rows.next()? {
		out.push(read_host(row)?);
	}
	Ok(out)
}

/// 把一行映射成 [`Host`]。
///
/// 枚举列统一走 `daisy-core` 的 `FromStr`，因此库里的取值与模型的取值
/// 只有一处定义，不会两边漂移。
fn read_host(row: &Row<'_>) -> Result<Host> {
	let id: i64 = row.get(0)?;
	let name: String = row.get(1)?;
	let protocol_raw: String = row.get(2)?;
	let host: String = row.get(3)?;
	let port_raw: i64 = row.get(4)?;
	let username: Option<String> = row.get(5)?;
	let auth_raw: String = row.get(6)?;
	let key_path: Option<String> = row.get(7)?;
	let has_passphrase: bool = row.get(8)?;
	let proxy_id: Option<i64> = row.get(9)?;
	let jump_host_id: Option<i64> = row.get(10)?;
	let group_id: Option<i64> = row.get(11)?;
	let resolution_raw: Option<String> = row.get(12)?;
	let scale_mode_raw: Option<String> = row.get(13)?;
	let scale_factor_raw: Option<f64> = row.get(14)?;
	// clipboard 紧跟在 scale_factor 之后（迁移 002 新增的列）
	let clipboard_raw: Option<i64> = row.get(15)?;
	// dpi 紧跟在 clipboard 之后（迁移 004 新增的列）
	let dpi_raw: Option<i64> = row.get(16)?;
	let created_at: i64 = row.get(17)?;
	let updated_at: i64 = row.get(18)?;

	let protocol: Protocol = protocol_raw
		.parse()
		.map_err(|e| StoreError::Format(format!("hosts.protocol 取值非法：{e}")))?;

	let auth = match auth_raw.as_str() {
		"password" => AuthMethod::Password,
		"publickey" => {
			let key_path = key_path.ok_or_else(|| {
				StoreError::Format("auth_method=publickey 但 key_path 为空".to_string())
			})?;
			AuthMethod::PublicKey(PublicKeyAuth {
				key_path,
				has_passphrase,
			})
		},
		other => {
			return Err(StoreError::Format(format!(
				"hosts.auth_method 取值非法：{other}"
			)));
		},
	};

	let resolution: Resolution = match resolution_raw.as_deref() {
		Some(raw) => raw
			.parse()
			.map_err(|e| StoreError::Format(format!("hosts.resolution 取值非法：{e}")))?,
		None => Resolution::default(),
	};

	let scale_mode: ScaleMode = match scale_mode_raw.as_deref() {
		Some(raw) => raw
			.parse()
			.map_err(|e| StoreError::Format(format!("hosts.scale_mode 取值非法：{e}")))?,
		None => ScaleMode::Fit,
	};

	let scale = ScaleSettings {
		mode: scale_mode,
		factor: scale_factor_raw.map_or(1.0, |v| v as f32),
	};
	scale
		.validate()
		.map_err(|e| StoreError::Format(format!("hosts.scale_factor 取值非法：{e}")))?;

	let dpi = dpi_raw.unwrap_or(i64::from(DEFAULT_DPI));
	let dpi = u32::try_from(dpi).map_err(|_| StoreError::Format(format!("hosts.dpi 超出范围：{dpi}")))?;
	validate_dpi(dpi).map_err(|e| StoreError::Format(format!("hosts.dpi 取值非法：{e}")))?;

	let port = u16::try_from(port_raw)
		.map_err(|_| StoreError::Format(format!("hosts.port 超出范围：{port_raw}")))?;

	Ok(Host {
		id: Some(id),
		name,
		protocol,
		host,
		port,
		username,
		auth,
		proxy_id,
		jump_host_id,
		group_id,
		display: DisplaySettings {
			resolution,
			scale,
			dpi,
		},
		// NULL 视为开启：迁移 002 给的是 DEFAULT 1，只有手工改过才是 0
		clipboard: clipboard_raw != Some(0),
		created_at,
		updated_at,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::memory_db;

	/// 迁移 004 给 dpi 列 `NOT NULL DEFAULT 100`：迁移前的老行升级后
	/// 必须直接落到默认值（列上带 DEFAULT，老行就是 100），不需要读取兜底。
	#[test]
	fn migrated_rows_get_default_dpi() {
		let (conn, _cipher) = memory_db();
		let dpi: i64 = conn
			.query_row("SELECT dpi FROM hosts LIMIT 1", [], |row| row.get(0))
			.unwrap_or(100);
		// 库是空的时查询会失败，那也说明表结构在——单独验列默认值：
		let _ = dpi;
		conn.execute(
			"INSERT INTO hosts (name, protocol, host, port, auth_method, created_at, updated_at)
			 VALUES ('迁移前老行', 'rdp', '10.0.0.9', 3389, 'password', 0, 0)",
			[],
		)
		.expect("按最小列集插行失败");
		let dpi: i64 = conn
			.query_row("SELECT dpi FROM hosts WHERE name = '迁移前老行'", [], |row| row.get(0))
			.expect("读取 dpi 失败");
		assert_eq!(dpi, 100, "迁移默认值必须是 100");
	}

	fn sample_host(name: &str) -> Host {
		let mut host = Host::draft(Protocol::Ssh);
		host.name = name.to_string();
		host.host = "10.0.0.1".to_string();
		host.username = Some("root".to_string());
		host
	}

	#[test]
	fn insert_and_get_round_trip() {
		let (conn, cipher) = memory_db();
		let mut host = sample_host("生产机");
		host.display.resolution = Resolution::fixed(1920, 1080).expect("分辨率非法");
		host.display.scale = ScaleSettings::new(ScaleMode::Custom, 1.5).expect("缩放非法");
		host.display.dpi = 200;

		let id =
			insert(&conn, &cipher, &host, &HostSecrets::with_password("s3cret")).expect("插入失败");
		let loaded = get(&conn, id).expect("读取失败");

		assert_eq!(loaded.display.dpi, 200, "DPI 缩放必须原样往返");

		assert_eq!(loaded.name, "生产机");
		assert_eq!(loaded.host, "10.0.0.1");
		assert_eq!(loaded.port, 22);
		assert_eq!(loaded.username.as_deref(), Some("root"));
		assert_eq!(loaded.auth, AuthMethod::Password);
		assert_eq!(
			loaded.display.resolution,
			Resolution::fixed(1920, 1080).unwrap()
		);
		assert_eq!(loaded.display.scale.mode, ScaleMode::Custom);
		assert!((loaded.display.scale.factor - 1.5).abs() < f32::EPSILON);
		assert!(loaded.created_at > 0);
	}

	/// 剪贴板开关要能落库并读回（S8-63）。
	///
	/// `hosts::update` 是整列覆盖，所以这个开关也必须跟着写进去——
	/// 只在 INSERT 里给默认值的话，用户改完保存再重开就丢回去了。
	#[test]
	fn clipboard_flag_round_trips_and_survives_update() {
		let (conn, cipher) = memory_db();
		let mut host = sample_host("剪贴板开关");
		host.clipboard = false;
		let id =
			insert(&conn, &cipher, &host, &HostSecrets::with_password("pw")).expect("插入失败");
		let mut loaded = get(&conn, id).expect("读取失败");
		assert!(!loaded.clipboard, "关闭状态应能读回");

		// 整列覆盖后再读
		loaded.clipboard = true;
		update(&conn, &cipher, &loaded, &HostSecrets::with_password("pw")).expect("更新失败");
		let mut reloaded = get(&conn, id).expect("二次读取失败");
		assert!(reloaded.clipboard, "开启状态应能读回");

		// 另一个方向：开着→关掉
		reloaded.clipboard = false;
		update(&conn, &cipher, &reloaded, &HostSecrets::with_password("pw")).expect("更新失败");
		let again = get(&conn, id).expect("三次读取失败");
		assert!(!again.clipboard, "关闭状态应能持久化");
	}

	#[test]
	fn password_is_encrypted_and_recoverable() {
		let (conn, cipher) = memory_db();
		let host = sample_host("密码主机");
		let id = insert(
			&conn,
			&cipher,
			&host,
			&HostSecrets::with_password("hunter2"),
		)
		.expect("插入失败");

		// 库里存的是密文，且不含明文子串
		let raw: Vec<u8> = conn
			.query_row(
				"SELECT secret_enc FROM hosts WHERE id = ?1",
				params![id],
				|row| row.get(0),
			)
			.expect("读取密文失败");
		assert!(!raw.is_empty());
		assert!(
			!raw.windows(7).any(|w| w == b"hunter2"),
			"密文里出现了明文子串"
		);

		let recovered = password(&conn, &cipher, id)
			.expect("解密失败")
			.expect("应有密码");
		assert_eq!(recovered.as_str(), "hunter2");
	}

	#[test]
	fn public_key_auth_round_trip() {
		let (conn, cipher) = memory_db();
		let mut host = sample_host("密钥主机");
		host.auth = AuthMethod::PublicKey(PublicKeyAuth {
			key_path: "~/.ssh/id_rsa".to_string(),
			has_passphrase: true,
		});

		let id =
			insert(&conn, &cipher, &host, &HostSecrets::with_passphrase("pp")).expect("插入失败");
		let loaded = get(&conn, id).expect("读取失败");

		match &loaded.auth {
			AuthMethod::PublicKey(pk) => {
				assert_eq!(pk.key_path, "~/.ssh/id_rsa");
				assert!(pk.has_passphrase, "口令存在性应从列推导出来");
			},
			other => panic!("认证方式不符：{other:?}"),
		}

		let recovered = key_passphrase(&conn, &cipher, id)
			.expect("解密失败")
			.expect("应有口令");
		assert_eq!(recovered.as_str(), "pp");
	}

	#[test]
	fn has_passphrase_is_false_without_secret() {
		let (conn, cipher) = memory_db();
		let mut host = sample_host("免密主机");
		host.auth = AuthMethod::PublicKey(PublicKeyAuth {
			key_path: "~/.ssh/id_ed25519".to_string(),
			has_passphrase: false,
		});

		let id = insert(&conn, &cipher, &host, &HostSecrets::none()).expect("插入失败");
		let loaded = get(&conn, id).expect("读取失败");

		match &loaded.auth {
			AuthMethod::PublicKey(pk) => assert!(!pk.has_passphrase),
			other => panic!("认证方式不符：{other:?}"),
		}
		assert!(
			key_passphrase(&conn, &cipher, id)
				.expect("读取失败")
				.is_none()
		);
	}

	#[test]
	fn update_replaces_fields_and_secrets() {
		let (conn, cipher) = memory_db();
		let mut host = sample_host("待改");
		let id =
			insert(&conn, &cipher, &host, &HostSecrets::with_password("old")).expect("插入失败");

		host.id = Some(id);
		host.name = "已改".to_string();
		host.port = 2222;
		update(&conn, &cipher, &host, &HostSecrets::with_password("new")).expect("更新失败");

		let loaded = get(&conn, id).expect("读取失败");
		assert_eq!(loaded.name, "已改");
		assert_eq!(loaded.port, 2222);
		assert_eq!(
			password(&conn, &cipher, id)
				.expect("读取失败")
				.unwrap()
				.as_str(),
			"new"
		);
	}

	#[test]
	fn update_without_id_is_rejected() {
		let (conn, cipher) = memory_db();
		let host = sample_host("无主键");
		let err = update(&conn, &cipher, &host, &HostSecrets::none()).expect_err("应报错");
		assert!(
			matches!(err, StoreError::Format(_)),
			"错误类型不符：{err:?}"
		);
	}

	#[test]
	fn update_missing_row_reports_not_found() {
		let (conn, cipher) = memory_db();
		let mut host = sample_host("不存在");
		host.id = Some(9999);

		let err = update(&conn, &cipher, &host, &HostSecrets::none()).expect_err("应报错");
		assert!(
			matches!(err, StoreError::NotFound { .. }),
			"错误类型不符：{err:?}"
		);
	}

	#[test]
	fn upsert_inserts_then_updates() {
		let (conn, cipher) = memory_db();
		let mut host = sample_host("upsert");

		let id =
			upsert(&conn, &cipher, &host, &HostSecrets::with_password("p1")).expect("插入失败");
		host.id = Some(id);
		host.name = "upsert-2".to_string();
		let same_id =
			upsert(&conn, &cipher, &host, &HostSecrets::with_password("p2")).expect("更新失败");

		assert_eq!(id, same_id);
		assert_eq!(get(&conn, id).expect("读取失败").name, "upsert-2");
	}

	#[test]
	fn list_and_delete() {
		let (conn, cipher) = memory_db();
		for name in ["b机", "a机", "c机"] {
			insert(&conn, &cipher, &sample_host(name), &HostSecrets::none()).expect("插入失败");
		}

		let all = list(&conn).expect("列表失败");
		assert_eq!(all.len(), 3);
		// 按名称排序：a机 < b机 < c机
		assert_eq!(all[0].name, "a机");
		assert_eq!(all[2].name, "c机");

		let target = all[1].id.expect("应有主键");
		assert!(delete(&conn, target).expect("删除失败"));
		assert!(!delete(&conn, target).expect("重复删除应返回 false"));
		assert_eq!(list(&conn).expect("列表失败").len(), 2);
	}

	#[test]
	fn list_in_group_filters() {
		let (conn, cipher) = memory_db();
		crate::group::insert(&conn, "组一").expect("建组失败");

		let mut grouped = sample_host("组内");
		grouped.group_id = Some(1);
		insert(&conn, &cipher, &grouped, &HostSecrets::none()).expect("插入失败");
		insert(&conn, &cipher, &sample_host("组外"), &HostSecrets::none()).expect("插入失败");

		assert_eq!(list_in_group(&conn, Some(1)).expect("查询失败").len(), 1);
		assert_eq!(list_in_group(&conn, None).expect("查询失败").len(), 1);
	}

	#[test]
	fn validation_runs_before_insert() {
		let (conn, cipher) = memory_db();
		let mut host = sample_host("坏主机");
		host.host = "  ".to_string();

		let err = insert(&conn, &cipher, &host, &HostSecrets::none()).expect_err("应校验失败");
		assert!(matches!(err, StoreError::Core(_)), "错误类型不符：{err:?}");
	}

	#[test]
	fn secrets_debug_does_not_leak() {
		let secrets = HostSecrets::with_password("top-secret");
		let rendered = format!("{secrets:?}");
		assert!(!rendered.contains("top-secret"), "Debug 泄露了密码");
		assert!(rendered.contains("已设置"));
	}

	#[test]
	fn deleting_proxy_sets_host_reference_to_null() {
		let (conn, cipher) = memory_db();
		let proxy_id = crate::proxy::insert(
			&conn,
			&cipher,
			&daisy_core::model::Proxy {
				id: None,
				name: "p".to_string(),
				kind: daisy_core::model::ProxyKind::Socks5,
				host: "127.0.0.1".to_string(),
				port: 1080,
				username: None,
			},
			None,
		)
		.expect("建代理失败");

		let mut host = sample_host("走代理");
		host.proxy_id = Some(proxy_id);
		let id = insert(&conn, &cipher, &host, &HostSecrets::none()).expect("插入失败");

		crate::proxy::delete(&conn, proxy_id).expect("删代理失败");

		let loaded = get(&conn, id).expect("读取失败");
		assert!(loaded.proxy_id.is_none(), "外键 ON DELETE SET NULL 未生效");
	}
}
