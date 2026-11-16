//! 分组 CRUD。
//!
//! 分组只支持**父子两层**（PLAN.md 的约束）。[`insert`] 会校验：不能给一个
//! 已经有上级的分组再挂上级，也不能把分组的上级设成自己。

use daisy_core::model::Group;
use rusqlite::{Connection, Row, params};

use crate::error::{Result, StoreError};

/// 分组表全部列，顺序与 [`read_group`] 一致。
const COLUMNS: &str = "id, name, parent_id";

/// 新增顶层分组，返回新主键。
pub fn insert(conn: &Connection, name: &str) -> Result<i64> {
	insert_with_parent(conn, name, None)
}

/// 新增分组并指定上级。
///
/// 上级不存在、或上级自身已有上级（会形成三层）时返回错误。
pub fn insert_with_parent(conn: &Connection, name: &str, parent_id: Option<i64>) -> Result<i64> {
	let group = Group {
		id: None,
		name: name.to_string(),
		parent_id,
	};
	group.validate()?;

	if let Some(parent) = parent_id {
		ensure_can_be_parent(conn, parent, None)?;
	}

	conn.execute(
		"INSERT INTO groups (name, parent_id) VALUES (?1, ?2)",
		params![group.name, group.parent_id],
	)?;

	Ok(conn.last_insert_rowid())
}

/// 重命名分组。
pub fn rename(conn: &Connection, id: i64, name: &str) -> Result<()> {
	let group = Group {
		id: Some(id),
		name: name.to_string(),
		parent_id: None,
	};
	group.validate()?;

	let affected = conn.execute(
		"UPDATE groups SET name = ?1 WHERE id = ?2",
		params![name, id],
	)?;
	if affected == 0 {
		return Err(StoreError::NotFound {
			entity: "分组", id
		});
	}
	Ok(())
}

/// 调整上级。`None` 表示升为顶层。
pub fn set_parent(conn: &Connection, id: i64, parent_id: Option<i64>) -> Result<()> {
	if let Some(parent) = parent_id {
		ensure_can_be_parent(conn, parent, Some(id))?;
	}

	let affected = conn.execute(
		"UPDATE groups SET parent_id = ?1 WHERE id = ?2",
		params![parent_id, id],
	)?;
	if affected == 0 {
		return Err(StoreError::NotFound {
			entity: "分组", id
		});
	}
	Ok(())
}

/// 按主键查询。
pub fn find(conn: &Connection, id: i64) -> Result<Option<Group>> {
	let sql = format!("SELECT {COLUMNS} FROM groups WHERE id = ?1");
	let mut stmt = conn.prepare(&sql)?;
	let mut rows = stmt.query(params![id])?;

	match rows.next()? {
		Some(row) => Ok(Some(read_group(row)?)),
		None => Ok(None),
	}
}

/// 列出全部分组，先顶层后子级，各自按名称排序。
pub fn list(conn: &Connection) -> Result<Vec<Group>> {
	let sql = format!(
		"SELECT {COLUMNS} FROM groups
		 ORDER BY parent_id IS NOT NULL, parent_id, name COLLATE NOCASE"
	);
	let mut stmt = conn.prepare(&sql)?;
	let mut rows = stmt.query([])?;

	let mut out = Vec::new();
	while let Some(row) = rows.next()? {
		out.push(read_group(row)?);
	}
	Ok(out)
}

/// 删除分组。其子分组会被级联删除（外键 `ON DELETE CASCADE`），
/// 引用它的主机 `group_id` 会被置空（`ON DELETE SET NULL`）。
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
	let affected = conn.execute("DELETE FROM groups WHERE id = ?1", params![id])?;
	Ok(affected > 0)
}

// ==================== 内部实现 ====================

/// 校验某个分组能否作为上级。
///
/// `self_id` 是被调整的那个分组（新增时传 `None`），用于挡住「自己当自己上级」。
fn ensure_can_be_parent(conn: &Connection, candidate: i64, self_id: Option<i64>) -> Result<()> {
	if self_id == Some(candidate) {
		return Err(StoreError::Format("分组不能作为自己的上级".to_string()));
	}

	let parent = find(conn, candidate)?.ok_or(StoreError::NotFound {
		entity: "上级分组",
		id: candidate,
	})?;

	if parent.parent_id.is_some() {
		return Err(StoreError::Format(format!(
			"分组「{}」已有上级，只支持两层结构",
			parent.name
		)));
	}

	Ok(())
}

fn read_group(row: &Row<'_>) -> Result<Group> {
	Ok(Group {
		id: Some(row.get(0)?),
		name: row.get(1)?,
		parent_id: row.get(2)?,
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::test_support::memory_db;

	#[test]
	fn insert_and_find() {
		let (conn, _cipher) = memory_db();
		let id = insert(&conn, "生产环境").expect("插入失败");

		let group = find(&conn, id).expect("查询失败").expect("应存在");
		assert_eq!(group.name, "生产环境");
		assert!(group.parent_id.is_none());
	}

	#[test]
	fn two_level_hierarchy() {
		let (conn, _cipher) = memory_db();
		let root = insert(&conn, "顶层").expect("插入失败");
		let child = insert_with_parent(&conn, "子级", Some(root)).expect("插入失败");

		assert_eq!(
			find(&conn, child).expect("查询失败").unwrap().parent_id,
			Some(root)
		);
	}

	#[test]
	fn third_level_is_rejected() {
		let (conn, _cipher) = memory_db();
		let root = insert(&conn, "顶层").expect("插入失败");
		let child = insert_with_parent(&conn, "子级", Some(root)).expect("插入失败");

		let err = insert_with_parent(&conn, "孙级", Some(child)).expect_err("三层应被拒绝");
		assert!(
			matches!(err, StoreError::Format(_)),
			"错误类型不符：{err:?}"
		);
	}

	#[test]
	fn group_cannot_be_its_own_parent() {
		let (conn, _cipher) = memory_db();
		let id = insert(&conn, "自环").expect("插入失败");

		let err = set_parent(&conn, id, Some(id)).expect_err("自环应被拒绝");
		assert!(
			matches!(err, StoreError::Format(_)),
			"错误类型不符：{err:?}"
		);
	}

	#[test]
	fn missing_parent_is_reported() {
		let (conn, _cipher) = memory_db();
		let err = insert_with_parent(&conn, "孤儿", Some(999)).expect_err("上级不存在应报错");
		assert!(
			matches!(err, StoreError::NotFound { .. }),
			"错误类型不符：{err:?}"
		);
	}

	#[test]
	fn rename_and_set_parent() {
		let (conn, _cipher) = memory_db();
		let root = insert(&conn, "顶层").expect("插入失败");
		let child = insert(&conn, "旧名").expect("插入失败");

		rename(&conn, child, "新名").expect("重命名失败");
		assert_eq!(find(&conn, child).expect("查询失败").unwrap().name, "新名");

		set_parent(&conn, child, Some(root)).expect("设上级失败");
		assert_eq!(
			find(&conn, child).expect("查询失败").unwrap().parent_id,
			Some(root)
		);

		set_parent(&conn, child, None).expect("升为顶层失败");
		assert!(
			find(&conn, child)
				.expect("查询失败")
				.unwrap()
				.parent_id
				.is_none()
		);
	}

	#[test]
	fn rename_missing_row_reports_not_found() {
		let (conn, _cipher) = memory_db();
		let err = rename(&conn, 404, "x").expect_err("应报错");
		assert!(
			matches!(err, StoreError::NotFound { .. }),
			"错误类型不符：{err:?}"
		);
	}

	#[test]
	fn empty_name_is_rejected() {
		let (conn, _cipher) = memory_db();
		let err = insert(&conn, "   ").expect_err("空名应被拒绝");
		assert!(matches!(err, StoreError::Core(_)), "错误类型不符：{err:?}");
	}

	#[test]
	fn delete_cascades_to_children() {
		let (conn, _cipher) = memory_db();
		let root = insert(&conn, "顶层").expect("插入失败");
		let child = insert_with_parent(&conn, "子级", Some(root)).expect("插入失败");

		assert!(delete(&conn, root).expect("删除失败"));
		assert!(
			find(&conn, child).expect("查询失败").is_none(),
			"子分组应被级联删除"
		);
	}

	#[test]
	fn list_puts_top_level_first() {
		let (conn, _cipher) = memory_db();
		let root = insert(&conn, "z顶层").expect("插入失败");
		insert_with_parent(&conn, "a子级", Some(root)).expect("插入失败");

		let names: Vec<String> = list(&conn)
			.expect("列表失败")
			.into_iter()
			.map(|g| g.name)
			.collect();
		assert_eq!(names, vec!["z顶层", "a子级"], "顶层应排在子级之前");
	}
}
