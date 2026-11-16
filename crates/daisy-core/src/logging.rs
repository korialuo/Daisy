//! 日志初始化：标准输出 + 按日滚动落盘。
//!
//! 落盘是为了「事后复盘」——界面上的 toast 只活 6 秒，
//! 用户回头想看清一条连接失败的具体原因时，得有个地方能翻。
//!
//! ## 级别优先级
//!
//! `DAISY_LOG` 环境变量 > 配置文件里的 `log_level`。调试时改配置才能看日志、
//! 而看日志本身又需要先知道问题出在哪——这个循环必须由环境变量打破。
//!
//! ## 为什么文件层要单独暴露
//!
//! [`file_layer`] 独立成函数是为了**可测**：全局 subscriber 只能注册一次，
//! 测试里用 `tracing::subscriber::with_default` 局部注册就能验证真的写了文件。

use std::path::Path;

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry};

use crate::config::AppPaths;
use crate::error::{CoreError, Result};

/// 日志环境变量名（覆盖配置文件里的级别）。
pub const LOG_ENV: &str = "DAISY_LOG";

/// 日志文件保留天数。
const KEEP_DAYS: usize = 7;

/// 写盘线程守卫。它的类型在 `tracing-appender` 里没有公开别名。
type Guard = WorkerGuard;

/// 日志初始化句柄。
///
/// 必须活到进程结束：`_guard` 一旦 drop，后台写盘线程收工，
/// 最后几条日志就丢了（退出时的报错恰恰是最需要留下的）。
pub struct Logging {
	/// 级别热替换句柄。设置页改级别即时生效靠它。
	reload: tracing_subscriber::reload::Handle<EnvFilter, Registry>,
	/// 写盘线程守卫，drop 时 flush。
	_guard: WorkerGuard,
}

impl Logging {
	/// 注册全局 subscriber：标准输出 + 日志文件。
	///
	/// 重复初始化（测试场景）只忽略，不报错。
	pub fn init(paths: &AppPaths, level: &str) -> Result<Self> {
		let filter = initial_filter(level);
		let (reload_layer, reload) = tracing_subscriber::reload::Layer::new(filter);
		let (writer, guard) = file_writer(&paths.log_dir())?;

		let subscriber = Registry::default()
			.with(reload_layer)
			.with(tracing_subscriber::fmt::layer().with_target(false))
			// 文件里不带 ANSI 转义——用编辑器打开时那些颜色码是噪音
			.with(
				tracing_subscriber::fmt::layer()
					.with_target(false)
					.with_ansi(false)
					.with_writer(writer),
			);

		let _ = subscriber.try_init();

		Ok(Self {
			reload,
			_guard: guard,
		})
	}

	/// 切换日志级别。句柄失效（极早期调用）时静默跳过。
	pub fn set_level(&self, level: &str) {
		if let Err(error) = self.reload.reload(EnvFilter::new(level)) {
			tracing::warn!("切换日志级别失败：{error}");
		}
	}

	/// `DAISY_LOG` 是否在启动时指定了级别。
	///
	/// 指定了就以环境变量为准，不被配置里的 `log_level` 覆盖。
	#[must_use]
	pub fn env_override() -> bool {
		std::env::var(LOG_ENV).is_ok_and(|raw| !raw.trim().is_empty())
	}
}

/// 构造日志文件写入器（非阻塞，后台线程写盘）与守卫。
///
/// 返回 writer 而不是 layer：`fmt::Layer<S>` 只对同一个 `S` 实现 `Layer`，
/// 预先写死订阅者类型就没法再组合进分层订阅者。交给调用点用
/// `.with_writer(writer)` 构造，类型由推断决定。
///
/// 独立出来也是为了测试能用 `with_default` 局部注册——见模块文档。
pub fn file_writer(dir: &Path) -> Result<(NonBlocking, Guard)> {
	std::fs::create_dir_all(dir).map_err(|error| {
		CoreError::InvalidConfig(format!("无法创建日志目录 {}：{error}", dir.display()))
	})?;

	let appender = tracing_appender::rolling::Builder::new()
		.rotation(tracing_appender::rolling::Rotation::DAILY)
		.filename_prefix(crate::APP_NAME.to_lowercase())
		// 保留最近 N 天：日志是排查用的，不是档案
		.max_log_files(KEEP_DAYS)
		.build(dir)
		.map_err(|error| CoreError::InvalidConfig(format!("初始化日志文件失败：{error}")))?;

	let (writer, guard) = tracing_appender::non_blocking(appender);
	Ok((writer, guard))
}

/// 启动时的级别过滤器：环境变量优先，其次配置。
fn initial_filter(configured_level: &str) -> EnvFilter {
	match std::env::var(LOG_ENV) {
		Ok(raw) if !raw.trim().is_empty() => EnvFilter::new(raw),
		_ => EnvFilter::new(configured_level),
	}
}

/// 在系统文件管理器里打开日志目录。
///
/// 打不开只是不方便，不该拖累别的功能——调用方拿到错误弹个提示即可。
pub fn open_log_dir(dir: &Path) -> Result<()> {
	open_in_file_manager(dir)
		.map_err(|error| CoreError::InvalidConfig(format!("打开日志目录失败：{error}")))
}

#[cfg(target_os = "macos")]
fn open_in_file_manager(dir: &Path) -> std::io::Result<()> {
	std::process::Command::new("open")
		.arg(dir)
		.spawn()
		.map(|_| ())
}

#[cfg(target_os = "windows")]
fn open_in_file_manager(dir: &Path) -> std::io::Result<()> {
	std::process::Command::new("explorer")
		.arg(dir)
		.spawn()
		.map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_in_file_manager(dir: &Path) -> std::io::Result<()> {
	std::process::Command::new("xdg-open")
		.arg(dir)
		.spawn()
		.map(|_| ())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::AppPaths;

	/// 局部注册 subscriber 打一条日志，验证真的落到了文件里。
	///
	/// 不用 `Logging::init`：全局 subscriber 只能注册一次，测试并行跑会互相打架。
	#[test]
	fn writes_log_to_file() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let paths = AppPaths::from_base(dir.path());

		let (writer, guard) = file_writer(&paths.log_dir()).expect("构造写入器失败");
		let subscriber = tracing_subscriber::registry().with(
			tracing_subscriber::fmt::layer()
				.with_ansi(false)
				.with_writer(writer),
		);

		tracing::subscriber::with_default(subscriber, || {
			tracing::info!("落盘验证：唯一标记 DAISY_LOG_MARKER");
		});
		// guard drop 会 flush 后台线程
		drop(guard);

		let content = read_log_dir(&paths.log_dir());
		assert!(
			content.contains("DAISY_LOG_MARKER"),
			"日志文件应包含刚打的这条，实际内容：{content:?}"
		);
	}

	#[test]
	fn log_file_has_no_ansi_escapes() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let paths = AppPaths::from_base(dir.path());

		let (writer, guard) = file_writer(&paths.log_dir()).expect("构造写入器失败");
		let subscriber = tracing_subscriber::registry().with(
			tracing_subscriber::fmt::layer()
				.with_ansi(false)
				.with_writer(writer),
		);
		tracing::subscriber::with_default(subscriber, || {
			tracing::warn!("带颜色的告警");
		});
		drop(guard);

		let content = read_log_dir(&paths.log_dir());
		assert!(
			!content.contains('\u{1b}'),
			"文件里不该有 ANSI 转义序列：{content:?}"
		);
	}

	#[test]
	fn file_writer_creates_missing_directory() {
		let dir = tempfile::TempDir::new().expect("创建临时目录失败");
		let nested = dir.path().join("还没有").join("这层目录");

		let (_writer, _guard) = file_writer(&nested).expect("应自动创建目录");
		assert!(nested.is_dir(), "日志目录应被创建");
	}

	/// 读出日志目录下所有文件的内容拼起来。
	fn read_log_dir(dir: &Path) -> String {
		let mut out = String::new();
		for entry in std::fs::read_dir(dir).expect("读日志目录失败") {
			let path = entry.expect("目录项").path();
			if path.is_file() {
				out.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
			}
		}
		out
	}
}
