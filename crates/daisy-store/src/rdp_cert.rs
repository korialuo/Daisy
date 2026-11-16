//! `rdp_certs` 读写：记住确认过的 RDP 服务端证书指纹。
//!
//! 与 [`crate::known_hosts`] 平行但独立成表——SSH 存的是公钥指纹（`SHA256:…`），
//! RDP 存的是证书的 sha256 十六进制串，来源与清理时机都不同。
//!
//! 键的构成与 SSH 一致：`host:port`。同一台机器的不同端口是不同的服务。

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::now_unix;

/// 一条已记录的 RDP 证书。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdpCert {
	/// `host:port`。
	pub host_key: String,
	/// 规范化指纹（小写、无分隔符）。
	pub fingerprint: String,
	/// 证书使用者，可能为空。
	pub subject: Option<String>,
	/// 签发者，可能为空。
	pub issuer: Option<String>,
	/// 首次记录时间（Unix 秒）。
	pub added_at: i64,
}

/// 用 `host` 与 `port` 拼出记录键。
///
/// 与 SSH 的 `known_hosts` 保持同一规则（**不**给 IPv6 加方括号）：
/// 这个键只用于查表，从不反向解析，`::1:3389` 不会产生歧义。
#[must_use]
pub fn host_key(host: &str, port: u16) -> String {
	format!("{host}:{port}")
}

/// 查询某台主机的记录。
pub fn find(conn: &Connection, host_key: &str) -> Result<Option<RdpCert>> {
	let mut stmt = conn.prepare(
		"SELECT host_key, fingerprint, subject, issuer, added_at FROM rdp_certs WHERE host_key = ?1",
	)?;
	let mut rows = stmt.query(params![host_key])?;

	match rows.next()? {
		Some(row) => Ok(Some(RdpCert {
			host_key: row.get(0)?,
			fingerprint: row.get(1)?,
			subject: row.get(2)?,
			issuer: row.get(3)?,
			added_at: row.get(4)?,
		})),
		None => Ok(None),
	}
}

/// 记录或覆盖一台主机的证书指纹。
///
/// 只应在**用户确认过**之后调用（本项目的语义是「首次连接自动记录」，
/// 指纹变更则拒绝，不自动覆盖）。
pub fn remember(
	conn: &Connection,
	host_key: &str,
	fingerprint: &str,
	subject: Option<&str>,
	issuer: Option<&str>,
) -> Result<()> {
	conn.execute(
		"INSERT INTO rdp_certs (host_key, fingerprint, subject, issuer, added_at)
		 VALUES (?1, ?2, ?3, ?4, ?5)
		 ON CONFLICT(host_key) DO UPDATE SET fingerprint = excluded.fingerprint,
		                                     subject = excluded.subject,
		                                     issuer = excluded.issuer,
		                                     added_at = excluded.added_at",
		params![host_key, fingerprint, subject, issuer, now_unix()],
	)?;
	Ok(())
}

/// 删除某台主机的记录。返回是否真的删掉了。
///
/// 用途：机器重装后指纹必然变，界面上要有一条「重新信任这台机器」的路。
pub fn forget(conn: &Connection, host_key: &str) -> Result<bool> {
	let affected = conn.execute(
		"DELETE FROM rdp_certs WHERE host_key = ?1",
		params![host_key],
	)?;
	Ok(affected > 0)
}

/// 列出全部记录，按主机键排序。
pub fn list(conn: &Connection) -> Result<Vec<RdpCert>> {
	let mut stmt = conn.prepare(
		"SELECT host_key, fingerprint, subject, issuer, added_at FROM rdp_certs ORDER BY host_key",
	)?;
	let mut rows = stmt.query([])?;

	let mut out = Vec::new();
	while let Some(row) = rows.next()? {
		out.push(RdpCert {
			host_key: row.get(0)?,
			fingerprint: row.get(1)?,
			subject: row.get(2)?,
			issuer: row.get(3)?,
			added_at: row.get(4)?,
		});
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::memory_db;

	#[test]
	fn host_key_includes_port() {
		assert_eq!(host_key("192.168.10.20", 3389), "192.168.10.20:3389");
		assert_eq!(host_key("win.test", 13389), "win.test:13389");
	}

	#[test]
	fn unknown_host_has_no_record() {
		let (conn, _cipher) = memory_db();
		assert!(
			find(&conn, &host_key("10.0.0.9", 3389))
				.expect("查询失败")
				.is_none()
		);
	}

	#[test]
	fn remember_then_find_roundtrip() {
		let (conn, _cipher) = memory_db();
		let key = host_key("10.0.0.9", 3389);

		remember(
			&conn,
			&key,
			"aabbccdd",
			Some("CN=win.test"),
			Some("CN=win.test"),
		)
		.expect("记录失败");

		let stored = find(&conn, &key).expect("查询失败").expect("应当有记录");
		assert_eq!(stored.fingerprint, "aabbccdd");
		assert_eq!(stored.subject.as_deref(), Some("CN=win.test"));
		assert_eq!(stored.issuer.as_deref(), Some("CN=win.test"));
		assert!(stored.added_at > 0);
	}

	#[test]
	fn remember_overwrites_single_row() {
		let (conn, _cipher) = memory_db();
		let key = host_key("10.0.0.9", 3389);

		remember(&conn, &key, "old", None, None).expect("记录失败");
		remember(&conn, &key, "new", None, None).expect("覆盖失败");

		assert_eq!(list(&conn).expect("列表失败").len(), 1, "不应产生重复行");
		assert_eq!(
			find(&conn, &key).expect("查询失败").unwrap().fingerprint,
			"new"
		);
	}

	#[test]
	fn different_ports_are_different_records() {
		let (conn, _cipher) = memory_db();
		remember(&conn, &host_key("10.0.0.9", 3389), "a", None, None).expect("记录失败");
		remember(&conn, &host_key("10.0.0.9", 13389), "b", None, None).expect("记录失败");

		assert_eq!(list(&conn).expect("列表失败").len(), 2);
	}

	#[test]
	fn forget_removes_record() {
		let (conn, _cipher) = memory_db();
		let key = host_key("10.0.0.9", 3389);
		remember(&conn, &key, "aabb", None, None).expect("记录失败");

		assert!(forget(&conn, &key).expect("删除失败"));
		assert!(!forget(&conn, &key).expect("重复删除应返回 false"));
		assert!(find(&conn, &key).expect("查询失败").is_none());
	}

	/// 迁移必须真的建出这张表——`rdp_certs` 是 v3 才加的，
	/// 老库升级后要能用（这条测试在 `memory_db()` 里跑全量迁移）。
	#[test]
	fn migration_creates_table() {
		let (conn, _cipher) = memory_db();
		let count: i64 = conn
			.query_row(
				"SELECT count(*) FROM sqlite_master WHERE name = 'rdp_certs'",
				[],
				|row| row.get(0),
			)
			.expect("查询失败");
		assert_eq!(count, 1);
	}
}
