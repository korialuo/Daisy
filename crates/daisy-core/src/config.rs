//! 应用配置与应用数据目录定位。
//!
//! ## 路径解析
//!
//! 所有路径都经 `directories` crate 解析，**不硬编码任何平台路径、不拼接分隔符**，
//! Windows 适配阶段无需改动（对应 PLAN.md 8.1 的 D2）。
//!
//! | 平台 | 数据目录 |
//! | --- | --- |
//! | macOS | `~/Library/Application Support/Daisy` |
//! | Windows | `%APPDATA%\Daisy` |
//!
//! ## 配置文件
//!
//! `config.toml` 存放界面与终端偏好。**凭据绝不写在这里**，
//! 它们只存在于加密数据库。

use std::fs;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::APP_NAME;
use crate::error::{CoreError, Result};

/// 配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// 数据库文件名。
pub const DATABASE_FILE_NAME: &str = "daisy.db";

/// 日志子目录名。
pub const LOG_DIR_NAME: &str = "logs";

/// 主密钥文件名（仅 Windows 的 DPAPI 方案使用）。
///
/// 文件内容是被 DPAPI 加密过的 32 字节随机密钥，**不是**明文密钥。
pub const KEY_FILE_NAME: &str = "key.bin";

// ==================== 路径 ====================

/// 应用的数据目录布局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
	base: PathBuf,
}

impl AppPaths {
	/// 按当前平台解析标准目录。
	///
	/// 若系统无法给出标准目录（极少数受限环境），回退到当前工作目录下的 `.daisy`。
	pub fn discover() -> Result<Self> {
		if let Some(dirs) = ProjectDirs::from("", "", APP_NAME) {
			return Ok(Self {
				base: dirs.data_dir().to_path_buf(),
			});
		}

		tracing::warn!("系统未提供标准应用目录，回退到当前目录下的 .daisy");
		Ok(Self {
			base: PathBuf::from(".daisy"),
		})
	}

	/// 指定根目录构造。测试与便携模式使用。
	#[must_use]
	pub fn from_base(base: impl Into<PathBuf>) -> Self {
		Self { base: base.into() }
	}

	/// 根目录。
	#[must_use]
	pub fn base(&self) -> &Path {
		&self.base
	}

	/// 配置文件路径。
	#[must_use]
	pub fn config_file(&self) -> PathBuf {
		self.base.join(CONFIG_FILE_NAME)
	}

	/// 数据库文件路径。
	#[must_use]
	pub fn database_file(&self) -> PathBuf {
		self.base.join(DATABASE_FILE_NAME)
	}

	/// 日志目录。
	#[must_use]
	pub fn log_dir(&self) -> PathBuf {
		self.base.join(LOG_DIR_NAME)
	}

	/// 主密钥文件路径（Windows DPAPI 方案）。
	///
	/// macOS 上用内置密钥，这个路径不会被创建；接口留着是为了
	/// 「密钥来源」这一层的调用方不必分平台。
	#[must_use]
	pub fn key_file(&self) -> PathBuf {
		self.base.join(KEY_FILE_NAME)
	}

	/// 确保目录存在。不会创建数据库或配置文件本身。
	pub fn ensure(&self) -> Result<()> {
		fs::create_dir_all(&self.base).map_err(|e| {
			CoreError::InvalidConfig(format!("无法创建数据目录 {}：{e}", self.base.display()))
		})?;
		fs::create_dir_all(self.log_dir()).map_err(|e| {
			CoreError::InvalidConfig(format!(
				"无法创建日志目录 {}：{e}",
				self.log_dir().display()
			))
		})?;
		Ok(())
	}
}

// ==================== 配置 ====================

/// 界面主题偏好。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
	/// 跟随系统。
	#[default]
	System,
	/// 浅色。
	Light,
	/// 深色。
	Dark,
}

/// 终端外观与行为偏好。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
	/// 等宽字体族。`None` 表示用系统默认等宽字体。
	pub font_family: Option<String>,
	/// 字号（逻辑点）。
	pub font_size: f32,
	/// 配色方案名。
	pub color_scheme: String,
	/// 回滚缓冲行数。
	pub scrollback: u32,
	/// 光标形状：`block` / `bar` / `underline`。
	pub cursor_style: String,
}

impl Default for TerminalConfig {
	fn default() -> Self {
		Self {
			font_family: None,
			font_size: 14.0,
			color_scheme: "daisy-dark".to_string(),
			scrollback: 10_000,
			cursor_style: "block".to_string(),
		}
	}
}

/// 窗口尺寸（逻辑点）。
///
/// 存在配置里而不是交给系统的「恢复上次位置」：
/// ① 三个平台的口径一致，不依赖各自的窗口服务器；
/// ② 用户能手工改、能被备份带过去；
/// ③ 窗口位置仍由系统管（多屏插拔时系统比我们更清楚该摆哪）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
	/// 宽度（逻辑点）。
	pub width: f32,
	/// 高度（逻辑点）。
	pub height: f32,
}

impl Default for WindowConfig {
	fn default() -> Self {
		Self {
			width: 1180.0,
			height: 760.0,
		}
	}
}

impl WindowConfig {
	/// 尺寸是否可用（有限且为正）。
	#[must_use]
	pub fn is_usable(&self) -> bool {
		self.width.is_finite() && self.height.is_finite() && self.width > 0.0 && self.height > 0.0
	}
}

/// 应用配置。**不含任何凭据。**
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
	/// 日志级别：trace / debug / info / warn / error。
	pub log_level: String,
	/// 主题偏好。
	pub theme: Theme,
	/// 界面语言。
	pub language: String,
	/// 终端偏好。
	pub terminal: TerminalConfig,
	/// 界面缩放比例，1.0 为默认。
	pub ui_scale: f32,
	/// 上次退出时的窗口尺寸。
	pub window: WindowConfig,
}

impl Default for AppConfig {
	fn default() -> Self {
		Self {
			log_level: "info".to_string(),
			theme: Theme::System,
			language: "zh-CN".to_string(),
			terminal: TerminalConfig::default(),
			ui_scale: 1.0,
			window: WindowConfig::default(),
		}
	}
}

/// 允许的日志级别。
const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

/// 允许的光标形状。
const CURSOR_STYLES: [&str; 3] = ["block", "bar", "underline"];

/// 界面缩放的允许区间。
const UI_SCALE_RANGE: std::ops::RangeInclusive<f32> = 0.5..=3.0;

/// 终端字号的允许区间。
const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 6.0..=48.0;

impl AppConfig {
	/// 从 TOML 文本解析。
	pub fn from_toml_str(raw: &str) -> Result<Self> {
		let config: Self = toml::from_str(raw)
			.map_err(|e| CoreError::InvalidConfig(format!("配置文件解析失败：{e}")))?;
		config.validate()?;
		Ok(config)
	}

	/// 序列化为 TOML 文本。
	pub fn to_toml_string(&self) -> Result<String> {
		toml::to_string_pretty(self)
			.map_err(|e| CoreError::Internal(format!("配置序列化失败：{e}")))
	}

	/// 读取配置。文件不存在时返回默认值，不算错误。
	pub fn load(path: &Path) -> Result<Self> {
		match fs::read_to_string(path) {
			Ok(raw) => Self::from_toml_str(&raw),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
			Err(e) => Err(CoreError::InvalidConfig(format!(
				"无法读取配置文件 {}：{e}",
				path.display()
			))),
		}
	}

	/// 写入配置。先写临时文件再改名，避免中途崩溃留下半截文件。
	pub fn save(&self, path: &Path) -> Result<()> {
		self.validate()?;
		let raw = self.to_toml_string()?;

		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent).map_err(|e| {
				CoreError::InvalidConfig(format!("无法创建目录 {}：{e}", parent.display()))
			})?;
		}

		let tmp = path.with_extension("toml.tmp");
		fs::write(&tmp, raw.as_bytes()).map_err(|e| {
			CoreError::InvalidConfig(format!("无法写入临时配置 {}：{e}", tmp.display()))
		})?;

		if let Err(e) = fs::rename(&tmp, path) {
			// 改名失败就别把临时文件留在磁盘上
			let _ = fs::remove_file(&tmp);
			return Err(CoreError::InvalidConfig(format!(
				"无法保存配置到 {}：{e}",
				path.display()
			)));
		}

		Ok(())
	}

	/// 校验各字段取值范围。错误的文案可直接展示给用户。
	pub fn validate(&self) -> Result<()> {
		if !LOG_LEVELS.contains(&self.log_level.as_str()) {
			return Err(CoreError::InvalidConfig(format!(
				"日志级别需为 {} 之一，收到 {}",
				LOG_LEVELS.join(" / "),
				self.log_level
			)));
		}

		if !self.ui_scale.is_finite() || !UI_SCALE_RANGE.contains(&self.ui_scale) {
			return Err(CoreError::InvalidConfig(format!(
				"界面缩放需在 {}-{} 之间，收到 {}",
				UI_SCALE_RANGE.start(),
				UI_SCALE_RANGE.end(),
				self.ui_scale
			)));
		}

		if !self.terminal.font_size.is_finite()
			|| !FONT_SIZE_RANGE.contains(&self.terminal.font_size)
		{
			return Err(CoreError::InvalidConfig(format!(
				"终端字号需在 {}-{} 之间，收到 {}",
				FONT_SIZE_RANGE.start(),
				FONT_SIZE_RANGE.end(),
				self.terminal.font_size
			)));
		}

		if !CURSOR_STYLES.contains(&self.terminal.cursor_style.as_str()) {
			return Err(CoreError::InvalidConfig(format!(
				"光标形状需为 {} 之一，收到 {}",
				CURSOR_STYLES.join(" / "),
				self.terminal.cursor_style
			)));
		}

		if self.terminal.scrollback == 0 {
			return Err(CoreError::InvalidConfig("回滚行数不能为 0".to_string()));
		}

		// 窗口尺寸只挡明显坏值：真正的大小由窗口系统再兜一层（最小尺寸、
		// 屏幕分辨率）。这里拦「0 或 NaN」是为了让坏配置在加载时就报出来，
		// 而不是变成一个看不见的窗口。
		if !self.window.is_usable() {
			return Err(CoreError::InvalidConfig(format!(
				"窗口尺寸需为正数，收到 {}×{}",
				self.window.width, self.window.height
			)));
		}

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn discover_ends_with_app_name() {
		let paths = AppPaths::discover().expect("解析目录失败");
		assert_eq!(
			paths.base().file_name().and_then(|n| n.to_str()),
			Some(APP_NAME),
			"数据目录应以应用名结尾：{}",
			paths.base().display()
		);
	}

	#[test]
	fn paths_are_derived_from_base() {
		let paths = AppPaths::from_base("/tmp/daisy-test");
		assert_eq!(
			paths.config_file(),
			PathBuf::from("/tmp/daisy-test/config.toml")
		);
		assert_eq!(
			paths.database_file(),
			PathBuf::from("/tmp/daisy-test/daisy.db")
		);
		assert_eq!(paths.log_dir(), PathBuf::from("/tmp/daisy-test/logs"));
	}

	#[test]
	fn ensure_creates_directories() {
		let base = std::env::temp_dir().join("daisy-paths-test");
		let _ = fs::remove_dir_all(&base);

		let paths = AppPaths::from_base(&base);
		paths.ensure().expect("创建目录失败");
		assert!(base.is_dir());
		assert!(paths.log_dir().is_dir());

		let _ = fs::remove_dir_all(&base);
	}

	#[test]
	fn config_toml_round_trip() {
		let config = AppConfig {
			log_level: "debug".to_string(),
			theme: Theme::Dark,
			ui_scale: 1.25,
			terminal: TerminalConfig {
				font_family: Some("JetBrains Mono".to_string()),
				font_size: 16.0,
				scrollback: 5000,
				..TerminalConfig::default()
			},
			..AppConfig::default()
		};

		let raw = config.to_toml_string().expect("序列化失败");
		let back = AppConfig::from_toml_str(&raw).expect("反序列化失败");
		assert_eq!(config, back);
	}

	#[test]
	fn config_file_round_trip() {
		let base = std::env::temp_dir().join("daisy-config-test");
		let _ = fs::remove_dir_all(&base);

		let path = AppPaths::from_base(&base).config_file();
		let config = AppConfig {
			log_level: "warn".to_string(),
			..AppConfig::default()
		};

		config.save(&path).expect("保存失败");
		let loaded = AppConfig::load(&path).expect("读取失败");
		assert_eq!(config, loaded);

		// 临时文件不应残留
		assert!(!path.with_extension("toml.tmp").exists(), "临时文件残留");

		let _ = fs::remove_dir_all(&base);
	}

	#[test]
	fn missing_config_falls_back_to_default() {
		let path = std::env::temp_dir().join("daisy-nonexistent/config.toml");
		let loaded = AppConfig::load(&path).expect("缺失文件应回退默认值");
		assert_eq!(loaded, AppConfig::default());
	}

	#[test]
	fn invalid_config_is_rejected() {
		let with_log_level = |level: &str| AppConfig {
			log_level: level.to_string(),
			..AppConfig::default()
		};
		assert!(
			with_log_level("verbose").validate().is_err(),
			"未知日志级别应被拒绝"
		);
		assert!(with_log_level("info").validate().is_ok());

		let with_ui_scale = |scale: f32| AppConfig {
			ui_scale: scale,
			..AppConfig::default()
		};
		assert!(
			with_ui_scale(0.1).validate().is_err(),
			"过小的界面缩放应被拒绝"
		);
		assert!(with_ui_scale(1.0).validate().is_ok());
		assert!(with_ui_scale(f32::NAN).validate().is_err(), "NaN 应被拒绝");

		let with_font_size = |size: f32| AppConfig {
			terminal: TerminalConfig {
				font_size: size,
				..TerminalConfig::default()
			},
			..AppConfig::default()
		};
		assert!(
			with_font_size(2.0).validate().is_err(),
			"过小的字号应被拒绝"
		);

		let with_cursor = |style: &str| AppConfig {
			terminal: TerminalConfig {
				cursor_style: style.to_string(),
				..TerminalConfig::default()
			},
			..AppConfig::default()
		};
		assert!(
			with_cursor("sparkle").validate().is_err(),
			"未知光标形状应被拒绝"
		);

		let with_scrollback = |lines: u32| AppConfig {
			terminal: TerminalConfig {
				scrollback: lines,
				..TerminalConfig::default()
			},
			..AppConfig::default()
		};
		assert!(
			with_scrollback(0).validate().is_err(),
			"回滚行数 0 应被拒绝"
		);
	}

	#[test]
	fn partial_toml_uses_defaults() {
		let raw = "log_level = \"debug\"\n";
		let config = AppConfig::from_toml_str(raw).expect("反序列化失败");
		assert_eq!(config.log_level, "debug");
		assert_eq!(config.terminal, TerminalConfig::default());
		assert_eq!(config.theme, Theme::System);
		// 老配置文件里没有 [window]：必须落到默认尺寸，而不是 0×0
		assert_eq!(config.window, WindowConfig::default());
	}

	#[test]
	fn window_size_round_trips_through_toml() {
		let config = AppConfig {
			window: WindowConfig {
				width: 1440.0,
				height: 900.0,
			},
			..AppConfig::default()
		};
		let raw = config.to_toml_string().expect("序列化失败");
		assert!(raw.contains("[window]"), "应写出 window 段：{raw}");
		let back = AppConfig::from_toml_str(&raw).expect("反序列化失败");
		assert_eq!(back.window, config.window);
	}

	#[test]
	fn zero_window_size_is_rejected() {
		let config = AppConfig {
			window: WindowConfig {
				width: 0.0,
				height: 900.0,
			},
			..AppConfig::default()
		};
		assert!(config.validate().is_err(), "0 宽应被拒绝");
	}

	#[test]
	fn broken_toml_reports_error() {
		assert!(AppConfig::from_toml_str("this is not toml = = =").is_err());
	}
}
