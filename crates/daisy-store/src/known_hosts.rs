//! `known_hosts` 读写。
//!
//! 用途：记住用户确认过的 SSH 主机公钥指纹，后续连接时做一致性校验，
//! 公钥变了要显著告警（可能是中间人，也可能只是重装了系统）。
//!
//! 键的构成：`host_key` 形如 `example.com:22`，与 OpenSSH 的习惯一致，
//! **带上端口**——同一台机器的不同端口可能是不同的服务。

use std::fmt;

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::now_unix;

/// 一条已确认的主机密钥记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHost {
	/// `host:port`。
	pub host_key: String,
	/// 公钥指纹，形如 `SHA256:xxxx`。
	pub fingerprint: String,
	/// 首次确认时间（Unix 秒）。
	pub added_at: i64,
}

/// 校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
	/// 首次见到这个主机，需要用户确认。
	Unknown,
	/// 指纹一致，可以继续连接。
	Match,
	/// 指纹与记录不符，必须告警并阻止连接。
	Mismatch {
		/// 之前记录的指纹。
		expected: String,
		/// 本次实际收到的指纹。
		actual: String,
	},
}

impl fmt::Display for Verification {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Unknown => f.write_str("首次连接，需要确认主机密钥"),
			Self::Match => f.write_str("主机密钥一致"),
			Self::Mismatch { expected, actual } => {
				write!(f, "主机密钥已变更！记录为 {expected}，实际为 {actual}")
			},
		}
	}
}

/// 用 `host` 与 `port` 拼出记录键。
#[must_use]
pub fn host_key(host: &str, port: u16) -> String {
	format!("{host}:{port}")
}

/// 查询某台主机的记录。
pub fn find(conn: &Connection, host_key: &str) -> Result<Option<KnownHost>> {
	let mut stmt = conn
		.prepare("SELECT host_key, fingerprint, added_at FROM known_hosts WHERE host_key = ?1")?;
	let mut rows = stmt.query(params![host_key])?;

	match rows.next()? {
		Some(row) => Ok(Some(KnownHost {
			host_key: row.get(0)?,
			fingerprint: row.get(1)?,
			added_at: row.get(2)?,
		})),
		None => Ok(None),
	}
}

/// 记录或覆盖一台主机的指纹。
///
/// 覆盖已存在的记录会把 `added_at` 刷新为当前时间——即使指纹没变。
pub fn remember(conn: &Connection, host_key: &str, fingerprint: &str) -> Result<()> {
	conn.execute(
		"INSERT INTO known_hosts (host_key, fingerprint, added_at) VALUES (?1, ?2, ?3)
		 ON CONFLICT(host_key) DO UPDATE SET fingerprint = excluded.fingerprint,
		                                     added_at = excluded.added_at",
		params![host_key, fingerprint, now_unix()],
	)?;
	Ok(())
}

/// 校验指纹：未记录返回 [`Verification::Unknown`]，不一致返回
/// [`Verification::Mismatch`]。**不会**自动写入。
pub fn verify(conn: &Connection, host_key: &str, fingerprint: &str) -> Result<Verification> {
	match find(conn, host_key)? {
		None => Ok(Verification::Unknown),
		Some(known) if known.fingerprint == fingerprint => Ok(Verification::Match),
		Some(known) => Ok(Verification::Mismatch {
			expected: known.fingerprint,
			actual: fingerprint.to_string(),
		}),
	}
}

/// 列出全部记录，按主机键排序。
pub fn list(conn: &Connection) -> Result<Vec<KnownHost>> {
	let mut stmt =
		conn.prepare("SELECT host_key, fingerprint, added_at FROM known_hosts ORDER BY host_key")?;
	let mut rows = stmt.query([])?;

	let mut out = Vec::new();
	while let Some(row) = rows.next()? {
		out.push(KnownHost {
			host_key: row.get(0)?,
			fingerprint: row.get(1)?,
			added_at: row.get(2)?,
		});
	}
	Ok(out)
}

/// 删除某台主机的记录。返回是否真的删掉了。
pub fn forget(conn: &Connection, host_key: &str) -> Result<bool> {
	let affected = conn.execute(
		"DELETE FROM known_hosts WHERE host_key = ?1",
		params![host_key],
	)?;
	Ok(affected > 0)
}

/// 清空全部记录。
pub fn forget_all(conn: &Connection) -> Result<usize> {
	let affected = conn.execute("DELETE FROM known_hosts", [])?;
	Ok(affected)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::memory_db;

	#[test]
	fn host_key_includes_port() {
		assert_eq!(host_key("example.com", 22), "example.com:22");
		assert_eq!(host_key("10.0.0.1", 2222), "10.0.0.1:2222");
	}

	#[test]
	fn unknown_host_is_reported_as_unknown() {
		let (conn, _cipher) = memory_db();
		let result = verify(&conn, "new-host:22", "SHA256:abc").expect("校验失败");
		assert_eq!(result, Verification::Unknown);
	}

	#[test]
	fn remember_then_match() {
		let (conn, _cipher) = memory_db();
		let key = host_key("example.com", 22);

		remember(&conn, &key, "SHA256:abc").expect("记录失败");
		assert_eq!(
			verify(&conn, &key, "SHA256:abc").expect("校验失败"),
			Verification::Match
		);
	}

	#[test]
	fn changed_fingerprint_is_mismatch() {
		let (conn, _cipher) = memory_db();
		let key = host_key("example.com", 22);

		remember(&conn, &key, "SHA256:old").expect("记录失败");

		let result = verify(&conn, &key, "SHA256:new").expect("校验失败");
		assert_eq!(
			result,
			Verification::Mismatch {
				expected: "SHA256:old".to_string(),
				actual: "SHA256:new".to_string()
			}
		);
	}

	#[test]
	fn remember_is_idempotent_and_refreshes() {
		let (conn, _cipher) = memory_db();
		let key = host_key("example.com", 22);

		remember(&conn, &key, "SHA256:abc").expect("记录失败");
		let first = find(&conn, &key).expect("查询失败").unwrap();

		remember(&conn, &key, "SHA256:abc").expect("重复记录失败");
		let second = find(&conn, &key).expect("查询失败").unwrap();

		assert_eq!(first.fingerprint, second.fingerprint);
		assert_eq!(list(&conn).expect("列表失败").len(), 1, "不应产生重复行");
	}

	#[test]
	fn different_ports_are_different_records() {
		let (conn, _cipher) = memory_db();

		remember(&conn, &host_key("example.com", 22), "SHA256:a").expect("记录失败");
		remember(&conn, &host_key("example.com", 2222), "SHA256:b").expect("记录失败");

		assert_eq!(list(&conn).expect("列表失败").len(), 2);
	}

	#[test]
	fn forget_removes_record() {
		let (conn, _cipher) = memory_db();
		let key = host_key("example.com", 22);
		remember(&conn, &key, "SHA256:abc").expect("记录失败");

		assert!(forget(&conn, &key).expect("删除失败"));
		assert!(!forget(&conn, &key).expect("重复删除应返回 false"));
		assert_eq!(
			verify(&conn, &key, "SHA256:abc").expect("校验失败"),
			Verification::Unknown
		);
	}

	#[test]
	fn forget_all_clears_everything() {
		let (conn, _cipher) = memory_db();
		remember(&conn, "a:22", "SHA256:a").expect("记录失败");
		remember(&conn, "b:22", "SHA256:b").expect("记录失败");

		assert_eq!(forget_all(&conn).expect("清空失败"), 2);
		assert!(list(&conn).expect("列表失败").is_empty());
	}

	#[test]
	fn verification_message_is_actionable() {
		let mismatch = Verification::Mismatch {
			expected: "SHA256:old".to_string(),
			actual: "SHA256:new".to_string(),
		};
		let text = mismatch.to_string();
		assert!(text.contains("变更"));
		assert!(text.contains("SHA256:old") && text.contains("SHA256:new"));
	}
}
