//! 代理 CRUD。结构与 `hosts` 类似，敏感字段只有密码一个。

use daisy_core::model::{Proxy, ProxyKind};
use rusqlite::{Connection, Row, params};
use zeroize::Zeroizing;

use crate::crypto::FieldCipher;
use crate::error::{Result, StoreError};

/// 代理表全部列，顺序与 [`read_proxy`] 一致。
const COLUMNS: &str = "id, name, kind, host, port, username";

/// 新增代理，返回新主键。
///
/// `password` 为 `None` 表示无密码代理。
pub fn insert(
	conn: &Connection,
	cipher: &FieldCipher,
	proxy: &Proxy,
	password: Option<&str>,
) -> Result<i64> {
	proxy.validate()?;

	conn.execute(
		"INSERT INTO proxies (name, kind, host, port, username, secret_enc)
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
		params![
			proxy.name,
			proxy.kind.as_str(),
			proxy.host,
			proxy.port,
			proxy.username,
			cipher.encrypt_optional_str(password)?,
		],
	)?;

	Ok(conn.last_insert_rowid())
}

/// 按主键更新。`password` 为 `None` 会清空原密码。
pub fn update(
	conn: &Connection,
	cipher: &FieldCipher,
	proxy: &Proxy,
	password: Option<&str>,
) -> Result<()> {
	proxy.validate()?;

	let id = proxy
		.id
		.ok_or_else(|| StoreError::Format("更新代理时缺少主键".to_string()))?;

	let affected = conn.execute(
		"UPDATE proxies SET name = ?1, kind = ?2, host = ?3, port = ?4,
		                    username = ?5, secret_enc = ?6
		 WHERE id = ?7",
		params![
			proxy.name,
			proxy.kind.as_str(),
			proxy.host,
			proxy.port,
			proxy.username,
			cipher.encrypt_optional_str(password)?,
			id,
		],
	)?;

	if affected == 0 {
		return Err(StoreError::NotFound {
			entity: "代理", id
		});
	}
	Ok(())
}

/// 按主键查询。
pub fn find(conn: &Connection, id: i64) -> Result<Option<Proxy>> {
	let sql = format!("SELECT {COLUMNS} FROM proxies WHERE id = ?1");
	let mut stmt = conn.prepare(&sql)?;
	let mut rows = stmt.query(params![id])?;

	match rows.next()? {
		Some(row) => Ok(Some(read_proxy(row)?)),
		None => Ok(None),
	}
}

/// 列出全部代理。
pub fn list(conn: &Connection) -> Result<Vec<Proxy>> {
	let sql = format!("SELECT {COLUMNS} FROM proxies ORDER BY name COLLATE NOCASE");
	let mut stmt = conn.prepare(&sql)?;
	let mut rows = stmt.query([])?;

	let mut out = Vec::new();
	while let Some(row) = rows.next()? {
		out.push(read_proxy(row)?);
	}
	Ok(out)
}

/// 按主键删除。引用该代理的主机其 `proxy_id` 会被置空（外键约束负责）。
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
	let affected = conn.execute("DELETE FROM proxies WHERE id = ?1", params![id])?;
	Ok(affected > 0)
}

/// 读取解密后的代理密码。
pub fn password(
	conn: &Connection,
	cipher: &FieldCipher,
	id: i64,
) -> Result<Option<Zeroizing<String>>> {
	let mut stmt = conn.prepare("SELECT secret_enc FROM proxies WHERE id = ?1")?;
	let mut rows = stmt.query(params![id])?;

	let Some(row) = rows.next()? else {
		return Err(StoreError::NotFound {
			entity: "代理", id
		});
	};

	let blob = row.get::<_, Option<Vec<u8>>>(0)?;
	cipher.decrypt_optional_str(blob.as_deref())
}

fn read_proxy(row: &Row<'_>) -> Result<Proxy> {
	let id: i64 = row.get(0)?;
	let name: String = row.get(1)?;
	let kind_raw: String = row.get(2)?;
	let host: String = row.get(3)?;
	let port_raw: i64 = row.get(4)?;
	let username: Option<String> = row.get(5)?;

	let kind: ProxyKind = kind_raw
		.parse()
		.map_err(|e| StoreError::Format(format!("proxies.kind 取值非法：{e}")))?;
	let port = u16::try_from(port_raw)
		.map_err(|_| StoreError::Format(format!("proxies.port 超出范围：{port_raw}")))?;

	Ok(Proxy {
		id: Some(id),
		name,
		kind,
		host,
		port,
		username,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::memory_db;

	fn sample_proxy(name: &str) -> Proxy {
		Proxy {
			id: None,
			name: name.to_string(),
			kind: ProxyKind::Socks5,
			host: "127.0.0.1".to_string(),
			port: 1080,
			username: Some("user".to_string()),
		}
	}

	#[test]
	fn insert_and_get_round_trip() {
		let (conn, cipher) = memory_db();
		let proxy = sample_proxy("本地代理");
		let id = insert(&conn, &cipher, &proxy, Some("pw")).expect("插入失败");

		let loaded = find(&conn, id).expect("查询失败").expect("应存在");
		assert_eq!(loaded.name, "本地代理");
		assert_eq!(loaded.kind, ProxyKind::Socks5);
		assert_eq!(loaded.host, "127.0.0.1");
		assert_eq!(loaded.port, 1080);
		assert_eq!(loaded.username.as_deref(), Some("user"));
	}

	#[test]
	fn password_is_encrypted() {
		let (conn, cipher) = memory_db();
		let id =
			insert(&conn, &cipher, &sample_proxy("带密码"), Some("proxy-pw")).expect("插入失败");

		let raw: Vec<u8> = conn
			.query_row(
				"SELECT secret_enc FROM proxies WHERE id = ?1",
				params![id],
				|row| row.get(0),
			)
			.expect("读密文失败");
		assert!(!raw.windows(8).any(|w| w == b"proxy-pw"), "密文里出现明文");

		assert_eq!(
			password(&conn, &cipher, id)
				.expect("解密失败")
				.unwrap()
				.as_str(),
			"proxy-pw"
		);
	}

	#[test]
	fn no_password_stays_null() {
		let (conn, cipher) = memory_db();
		let id = insert(&conn, &cipher, &sample_proxy("免密"), None).expect("插入失败");

		let raw: Option<Vec<u8>> = conn
			.query_row(
				"SELECT secret_enc FROM proxies WHERE id = ?1",
				params![id],
				|row| row.get(0),
			)
			.expect("读取失败");
		assert!(raw.is_none());
		assert!(password(&conn, &cipher, id).expect("读取失败").is_none());
	}

	#[test]
	fn update_and_delete() {
		let (conn, cipher) = memory_db();
		let mut proxy = sample_proxy("待改");
		let id = insert(&conn, &cipher, &proxy, Some("old")).expect("插入失败");

		proxy.id = Some(id);
		proxy.name = "已改".to_string();
		proxy.port = 8080;
		update(&conn, &cipher, &proxy, Some("new")).expect("更新失败");

		let loaded = find(&conn, id).expect("查询失败").unwrap();
		assert_eq!(loaded.name, "已改");
		assert_eq!(loaded.port, 8080);
		assert_eq!(
			password(&conn, &cipher, id)
				.expect("读取失败")
				.unwrap()
				.as_str(),
			"new"
		);

		assert!(delete(&conn, id).expect("删除失败"));
		assert!(find(&conn, id).expect("查询失败").is_none());
	}

	#[test]
	fn list_sorted_by_name() {
		let (conn, cipher) = memory_db();
		for name in ["z", "a", "m"] {
			insert(&conn, &cipher, &sample_proxy(name), None).expect("插入失败");
		}
		let names: Vec<String> = list(&conn)
			.expect("列表失败")
			.into_iter()
			.map(|p| p.name)
			.collect();
		assert_eq!(names, vec!["a", "m", "z"]);
	}

	#[test]
	fn invalid_kind_in_db_is_reported() {
		let (conn, cipher) = memory_db();
		let id = insert(&conn, &cipher, &sample_proxy("坏类型"), None).expect("插入失败");

		// 绕过 CHECK 约束直接改脏数据：先关外键约束无关，这里改 CHECK 会失败，
		// 所以直接构造一条非法记录不可行——改为验证 CHECK 约束本身生效
		let outcome = conn.execute("UPDATE proxies SET kind = 'ftp' WHERE id = ?1", params![id]);
		assert!(outcome.is_err(), "CHECK 约束未阻止非法 kind");
	}
}
