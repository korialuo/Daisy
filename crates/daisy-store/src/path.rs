//! 数据库文件定位与首次建库。
//!
//! 路径的来源是 [`daisy_core::config::AppPaths`]，它内部走 `directories` crate，
//! 因此这里**不出现任何平台判断**（PLAN.md 8.1 的 D2）：
//!
//! | 平台 | 路径 |
//! | --- | --- |
//! | macOS | `~/Library/Application Support/Daisy/daisy.db` |
//! | Windows | `%APPDATA%\Daisy\daisy.db` |

use std::path::PathBuf;

use daisy_core::config::AppPaths;

use crate::error::Result;

/// 数据库文件的位置，以及它是否在本次解析时还不存在。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseLocation {
	/// 数据库文件路径。
	pub file: PathBuf,
	/// `true` 表示这是一次全新建库。
	pub is_new: bool,
}

impl DatabaseLocation {
	/// 按应用标准目录解析，不触碰磁盘。
	#[must_use]
	pub fn resolve(paths: &AppPaths) -> Self {
		let file = paths.database_file();
		let is_new = !file.exists();
		Self { file, is_new }
	}

	/// 按应用标准目录解析，并确保所在目录已存在。
	pub fn prepare(paths: &AppPaths) -> Result<Self> {
		paths.ensure()?;
		Ok(Self::resolve(paths))
	}

	/// 指定任意文件路径，并确保其父目录存在。测试与便携模式使用。
	pub fn at(file: impl Into<PathBuf>) -> Result<Self> {
		let file = file.into();

		if let Some(parent) = file.parent()
			&& !parent.as_os_str().is_empty()
		{
			std::fs::create_dir_all(parent)?;
		}

		let is_new = !file.exists();
		Ok(Self { file, is_new })
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn temp_base(tag: &str) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("daisy-path-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		dir
	}

	#[test]
	fn resolve_uses_app_paths() {
		let base = temp_base("resolve");
		let paths = AppPaths::from_base(&base);

		let location = DatabaseLocation::resolve(&paths);
		assert_eq!(location.file, base.join("daisy.db"));
		assert!(location.is_new, "目录都不存在时应视为新建");
	}

	#[test]
	fn prepare_creates_parent_directory() {
		let base = temp_base("prepare");
		let paths = AppPaths::from_base(&base);

		let location = DatabaseLocation::prepare(&paths).expect("准备目录失败");
		assert_eq!(location.file, base.join("daisy.db"));
		assert!(base.is_dir(), "父目录应被创建");
		assert!(location.is_new);

		let _ = std::fs::remove_dir_all(&base);
	}

	#[test]
	fn existing_file_is_not_new() {
		let base = temp_base("existing");
		let paths = AppPaths::from_base(&base);
		paths.ensure().expect("创建目录失败");

		std::fs::write(paths.database_file(), b"placeholder").expect("写入失败");

		let location = DatabaseLocation::resolve(&paths);
		assert!(!location.is_new, "文件已存在时不应视为新建");

		let _ = std::fs::remove_dir_all(&base);
	}

	#[test]
	fn at_creates_nested_parent() {
		let base = temp_base("nested");
		let file = base.join("a").join("b").join("daisy.db");

		let location = DatabaseLocation::at(&file).expect("准备路径失败");
		assert_eq!(location.file, file);
		assert!(base.join("a").join("b").is_dir());
		assert!(location.is_new);

		let _ = std::fs::remove_dir_all(&base);
	}
}
