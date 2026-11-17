//! 领域模型。
//!
//! ## 设计约定
//!
//! - **敏感信息不进入模型**。密码、私钥口令一律只以密文形式存在于存储层
//!   （`hosts.secret_enc` / `hosts.key_passphrase_enc`），模型里最多保留
//!   「是否已设置」这类布尔标记。这样模型可以安全地打印、序列化、写日志。
//! - 所有类型都支持 serde 往返，既用于落库（`extra` JSON 列），也用于配置读写。
//! - 取值范围在这里集中校验，存储层与界面层不必各自重复判断。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// 桌面宽度下限。
pub const MIN_DESKTOP_WIDTH: u32 = 320;
/// 桌面宽度上限。
pub const MAX_DESKTOP_WIDTH: u32 = 8192;
/// 桌面高度下限。
pub const MIN_DESKTOP_HEIGHT: u32 = 240;
/// 桌面高度上限。
pub const MAX_DESKTOP_HEIGHT: u32 = 8192;
/// 缩放比例下限。
pub const MIN_SCALE_FACTOR: f32 = 0.25;
/// 缩放比例上限。
pub const MAX_SCALE_FACTOR: f32 = 4.0;
/// 远端 DPI 缩放下限（MS-RDPEDISP 2.2.2.2 的规范约束）。
pub const MIN_DPI: u32 = 100;
/// 远端 DPI 缩放上限。
pub const MAX_DPI: u32 = 500;
/// DPI 档位步长——Windows 的 DPI 滑块就是 25 的倍数。
pub const DPI_STEP: u32 = 25;
/// 远端 DPI 缩放默认值，与 Windows 的默认显示缩放一致。
pub const DEFAULT_DPI: u32 = 100;

/// 校验远端 DPI 缩放取值（100–500 且为 25 的倍数）。
pub fn validate_dpi(dpi: u32) -> Result<()> {
	if !(MIN_DPI..=MAX_DPI).contains(&dpi) || !dpi.is_multiple_of(DPI_STEP) {
		return Err(CoreError::InvalidConfig(format!(
			"DPI 缩放需为 {MIN_DPI}-{MAX_DPI} 之间 {DPI_STEP} 的倍数，收到 {dpi}"
		)));
	}
	Ok(())
}

// ==================== 协议 ====================

/// 连接协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
	/// SSH，用于打开终端。
	Ssh,
	/// RDP，用于远程桌面。
	Rdp,
}

impl Protocol {
	/// 协议的默认端口。
	#[must_use]
	pub const fn default_port(self) -> u16 {
		match self {
			Self::Ssh => 22,
			Self::Rdp => 3389,
		}
	}

	/// 落库用的字符串形式，与 `hosts.protocol` 的 CHECK 约束一致。
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Ssh => "ssh",
			Self::Rdp => "rdp",
		}
	}
}

impl fmt::Display for Protocol {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

impl FromStr for Protocol {
	type Err = CoreError;

	fn from_str(raw: &str) -> Result<Self> {
		match raw {
			"ssh" => Ok(Self::Ssh),
			"rdp" => Ok(Self::Rdp),
			other => Err(CoreError::InvalidConfig(format!("未知协议：{other}"))),
		}
	}
}

// ==================== 认证方式 ====================

/// SSH 公钥认证的附加参数。
///
/// 私钥口令**不在这里**，它落在 `hosts.key_passphrase_enc` 密文列。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKeyAuth {
	/// 私钥文件路径。
	pub key_path: String,
	/// 该私钥是否设置了口令。仅用于界面提示与输入框显隐。
	pub has_passphrase: bool,
}

/// 认证方式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AuthMethod {
	/// 密码认证。密码本体存于 `hosts.secret_enc`，不在此处。
	Password,
	/// 公钥认证，当前只支持 RSA。
	PublicKey(PublicKeyAuth),
}

impl AuthMethod {
	/// 落库用的字符串形式，与 `hosts.auth_method` 的 CHECK 约束一致。
	#[must_use]
	pub const fn as_str(&self) -> &'static str {
		match self {
			Self::Password => "password",
			Self::PublicKey(_) => "publickey",
		}
	}

	/// 是否使用公钥认证。
	#[must_use]
	pub const fn is_public_key(&self) -> bool {
		matches!(self, Self::PublicKey(_))
	}
}

// ==================== 显示：分辨率与缩放 ====================

/// 远端桌面分辨率来源。
///
/// 序列化为字符串以匹配 `hosts.resolution` 列：
/// `"fit-window"`、`"fit-window:2820x1794"` 或 `"1920x1080"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(try_from = "String", into = "String")]
pub enum Resolution {
	/// 跟随本地窗口尺寸（配合防抖动态下发给远端）。
	#[default]
	FitWindow,
	/// 跟随窗口，且记着上一次的实际目标。
	///
	/// **建连尺寸直接用它**——否则每次连接都先以 1280×800 建会话、
	/// disp 再收口到目标，用户看到分辨率「跳一次」。View 每次跟随
	/// 下发后回写这个值，下次连接首帧就是正确尺寸。
	FitWindowAt {
		/// 宽度。
		width: u32,
		/// 高度。
		height: u32,
	},
	/// 固定分辨率。
	Fixed {
		/// 宽度。
		width: u32,
		/// 高度。
		height: u32,
	},
}

impl Resolution {
	/// 记住跟随窗口目标尺寸，并校验取值范围。
	pub fn fit_window_at(width: u32, height: u32) -> Result<Self> {
		Self::validate_size(width, height)?;
		Ok(Self::FitWindowAt { width, height })
	}

	/// 校验宽高在 disp 允许范围内（跟随目标与固定分辨率共用）。
	fn validate_size(width: u32, height: u32) -> Result<()> {
		if !(MIN_DESKTOP_WIDTH..=MAX_DESKTOP_WIDTH).contains(&width) {
			return Err(CoreError::InvalidConfig(format!(
				"宽度需在 {MIN_DESKTOP_WIDTH}-{MAX_DESKTOP_WIDTH} 之间，收到 {width}"
			)));
		}
		if !(MIN_DESKTOP_HEIGHT..=MAX_DESKTOP_HEIGHT).contains(&height) {
			return Err(CoreError::InvalidConfig(format!(
				"高度需在 {MIN_DESKTOP_HEIGHT}-{MAX_DESKTOP_HEIGHT} 之间，收到 {height}"
			)));
		}
		Ok(())
	}
}

impl Resolution {
	/// 构造固定分辨率，并校验取值范围。
	pub fn fixed(width: u32, height: u32) -> Result<Self> {
		if !(MIN_DESKTOP_WIDTH..=MAX_DESKTOP_WIDTH).contains(&width) {
			return Err(CoreError::InvalidConfig(format!(
				"宽度需在 {MIN_DESKTOP_WIDTH}-{MAX_DESKTOP_WIDTH} 之间，收到 {width}"
			)));
		}
		if !(MIN_DESKTOP_HEIGHT..=MAX_DESKTOP_HEIGHT).contains(&height) {
			return Err(CoreError::InvalidConfig(format!(
				"高度需在 {MIN_DESKTOP_HEIGHT}-{MAX_DESKTOP_HEIGHT} 之间，收到 {height}"
			)));
		}
		Ok(Self::Fixed { width, height })
	}
}

impl fmt::Display for Resolution {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::FitWindow => f.write_str("fit-window"),
			Self::FitWindowAt { width, height } => write!(f, "fit-window:{width}x{height}"),
			Self::Fixed { width, height } => write!(f, "{width}x{height}"),
		}
	}
}

impl FromStr for Resolution {
	type Err = CoreError;

	fn from_str(raw: &str) -> Result<Self> {
		if raw == "fit-window" {
			return Ok(Self::FitWindow);
		}

		if let Some(size) = raw.strip_prefix("fit-window:") {
			let (width, height) = size.split_once('x').ok_or_else(|| {
				CoreError::InvalidConfig(format!("跟随窗口目标需为 fit-window:宽x高，收到 {raw}"))
			})?;
			let width = width
				.trim()
				.parse::<u32>()
				.map_err(|e| CoreError::InvalidConfig(format!("宽度不是整数：{width}（{e}）")))?;
			let height = height
				.trim()
				.parse::<u32>()
				.map_err(|e| CoreError::InvalidConfig(format!("高度不是整数：{height}（{e}）")))?;
			Self::validate_size(width, height)?;
			return Ok(Self::FitWindowAt { width, height });
		}

		let (width, height) = raw.split_once('x').ok_or_else(|| {
			CoreError::InvalidConfig(format!(
				"分辨率需为 \"fit-window\" 或 \"宽x高\"，收到 {raw}"
			))
		})?;

		let width = width
			.trim()
			.parse::<u32>()
			.map_err(|e| CoreError::InvalidConfig(format!("宽度不是整数：{width}（{e}）")))?;
		let height = height
			.trim()
			.parse::<u32>()
			.map_err(|e| CoreError::InvalidConfig(format!("高度不是整数：{height}（{e}）")))?;

		Self::fixed(width, height)
	}
}

impl TryFrom<String> for Resolution {
	type Error = CoreError;

	fn try_from(raw: String) -> Result<Self> {
		raw.parse()
	}
}

impl From<Resolution> for String {
	fn from(value: Resolution) -> Self {
		value.to_string()
	}
}

/// 本地画面缩放模式。**不改远端分辨率**，只影响本地显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScaleMode {
	/// 等比缩放铺满可用区域，可能留黑边。
	Fit,
	/// 等比缩放并裁切，不留黑边。
	Fill,
	/// 原始像素 1:1，超出部分滚动。
	Actual,
	/// 按 [`ScaleSettings::factor`] 指定比例。
	Custom,
}

impl ScaleMode {
	/// 落库用的字符串形式。
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Fit => "fit",
			Self::Fill => "fill",
			Self::Actual => "actual",
			Self::Custom => "custom",
		}
	}
}

impl fmt::Display for ScaleMode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

impl FromStr for ScaleMode {
	type Err = CoreError;

	fn from_str(raw: &str) -> Result<Self> {
		match raw {
			"fit" => Ok(Self::Fit),
			"fill" => Ok(Self::Fill),
			"actual" => Ok(Self::Actual),
			"custom" => Ok(Self::Custom),
			other => Err(CoreError::InvalidConfig(format!("未知缩放模式：{other}"))),
		}
	}
}

/// 缩放设置。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScaleSettings {
	/// 缩放模式。
	pub mode: ScaleMode,
	/// 缩放比例，仅当 `mode` 为 [`ScaleMode::Custom`] 时生效。
	pub factor: f32,
}

impl Default for ScaleSettings {
	fn default() -> Self {
		Self {
			mode: ScaleMode::Fit,
			factor: 1.0,
		}
	}
}

impl ScaleSettings {
	/// 构造并校验。
	pub fn new(mode: ScaleMode, factor: f32) -> Result<Self> {
		let value = Self { mode, factor };
		value.validate()?;
		Ok(value)
	}

	/// 校验比例是否落在允许区间内。
	pub fn validate(&self) -> Result<()> {
		if self.mode != ScaleMode::Custom {
			return Ok(());
		}
		if !self.factor.is_finite() || !(MIN_SCALE_FACTOR..=MAX_SCALE_FACTOR).contains(&self.factor)
		{
			return Err(CoreError::InvalidConfig(format!(
				"缩放比例需在 {MIN_SCALE_FACTOR}-{MAX_SCALE_FACTOR} 之间，收到 {}",
				self.factor
			)));
		}
		Ok(())
	}
}

/// 显示相关设置的合集，对应 `hosts` 表的显示列。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DisplaySettings {
	/// 远端分辨率。
	pub resolution: Resolution,
	/// 本地缩放。
	pub scale: ScaleSettings,
	/// 远端 DPI 缩放（百分比，100–500）。走 disp 通道的
	/// `DesktopScaleFactor` / `DeviceScaleFactor`，**影响远端怎么渲染**，
	/// 与只影响本地显示的 [`ScaleSettings`] 是两回事。
	#[serde(default = "default_dpi")]
	pub dpi: u32,
}

fn default_dpi() -> u32 {
	DEFAULT_DPI
}

impl Default for DisplaySettings {
	fn default() -> Self {
		Self {
			resolution: Resolution::default(),
			scale: ScaleSettings::default(),
			dpi: DEFAULT_DPI,
		}
	}
}

// ==================== 代理与分组 ====================

/// 代理类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyKind {
	/// SOCKS5。
	Socks5,
	/// HTTP CONNECT。
	Http,
}

impl ProxyKind {
	/// 落库用的字符串形式。
	#[must_use]
	pub const fn as_str(self) -> &'static str {
		match self {
			Self::Socks5 => "socks5",
			Self::Http => "http",
		}
	}
}

impl fmt::Display for ProxyKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

impl FromStr for ProxyKind {
	type Err = CoreError;

	fn from_str(raw: &str) -> Result<Self> {
		match raw {
			"socks5" => Ok(Self::Socks5),
			"http" => Ok(Self::Http),
			other => Err(CoreError::InvalidConfig(format!("未知代理类型：{other}"))),
		}
	}
}

/// 代理配置。密码不在模型中，存于 `proxies.secret_enc`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proxy {
	/// 主键，未入库时为 `None`。
	pub id: Option<i64>,
	/// 显示名。
	pub name: String,
	/// 协议类型。
	pub kind: ProxyKind,
	/// 代理主机。
	pub host: String,
	/// 代理端口。
	pub port: u16,
	/// 代理用户名，可选。
	pub username: Option<String>,
}

impl Proxy {
	/// 校验必填项与端口。
	pub fn validate(&self) -> Result<()> {
		require_non_empty("代理名称", &self.name)?;
		require_non_empty("代理地址", &self.host)?;
		require_port(self.port)?;
		Ok(())
	}
}

/// 主机分组，支持一层父子（对应 PLAN.md 的「父子两层」约束）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
	/// 主键，未入库时为 `None`。
	pub id: Option<i64>,
	/// 分组名。
	pub name: String,
	/// 上级分组。顶层分组为 `None`。
	pub parent_id: Option<i64>,
}

impl Group {
	/// 校验分组名。
	pub fn validate(&self) -> Result<()> {
		require_non_empty("分组名称", &self.name)
	}
}

// ==================== 主机 ====================

/// 一台可连接的远程主机。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Host {
	/// 主键，未入库时为 `None`。
	pub id: Option<i64>,
	/// 显示名。
	pub name: String,
	/// 协议。
	pub protocol: Protocol,
	/// 主机名或 IP。
	///
	/// 支持域名、IPv4、IPv6 三种写法。存库前应经 [`crate::addr::HostAddr`]
	/// 校验并规范化——否则同一个主机会因写法不同存成两条
	/// （`Example.COM` 与 `example.com`、`2001:0db8::1` 与 `2001:db8::1`）。
	pub host: String,
	/// 端口。
	pub port: u16,
	/// 登录用户名。RDP 下通常为 `Administrator` 之类。
	pub username: Option<String>,
	/// 认证方式。密钥口令与密码本体不在此。
	pub auth: AuthMethod,
	/// 关联代理。
	pub proxy_id: Option<i64>,
	/// 跳板机（仅 SSH 有意义）。
	pub jump_host_id: Option<i64>,
	/// 所属分组。
	pub group_id: Option<i64>,
	/// 分辨率与缩放。
	pub display: DisplaySettings,
	/// 是否映射剪贴板（仅 RDP 有意义）。
	///
	/// 对应 Windows 自带 mstsc 连接前那个「剪贴板」勾选项。
	/// 默认开——远程桌面的主要用途就是跨机搬文本，关掉反而是少数派。
	/// 关掉时连 `cliprdr` 通道都不装，远端连问都不会问。
	pub clipboard: bool,
	/// 创建时间（Unix 秒）。
	pub created_at: i64,
	/// 最后修改时间（Unix 秒）。
	pub updated_at: i64,
}

impl Host {
	/// 校验全部字段。比 `validate` 更细的逐字段错误由界面层负责展示。
	pub fn validate(&self) -> Result<()> {
		require_non_empty("主机名称", &self.name)?;
		require_non_empty("主机地址", &self.host)?;
		require_port(self.port)?;

		match &self.auth {
			AuthMethod::Password => {},
			AuthMethod::PublicKey(pk) => {
				require_non_empty("私钥路径", &pk.key_path)?;
			},
		}

		// 显示设置自身的一致性
		match self.display.resolution {
			Resolution::Fixed { width, height } => {
				Resolution::fixed(width, height)?;
			},
			Resolution::FitWindowAt { width, height } => {
				Resolution::fit_window_at(width, height)?;
			},
			Resolution::FitWindow => {},
		}
		self.display.scale.validate()?;
		validate_dpi(self.display.dpi)?;

		Ok(())
	}

	/// 用协议默认端口构造一台最小可用的主机，便于「新建主机」表单预填。
	#[must_use]
	pub fn draft(protocol: Protocol) -> Self {
		Self {
			id: None,
			name: String::new(),
			protocol,
			host: String::new(),
			port: protocol.default_port(),
			username: None,
			auth: AuthMethod::Password,
			proxy_id: None,
			jump_host_id: None,
			group_id: None,
			display: DisplaySettings::default(),
			// 剪贴板默认开：远程桌面的主要用途就是跨机搬文本
			clipboard: true,
			created_at: 0,
			updated_at: 0,
		}
	}
}

// ==================== 校验辅助 ====================

fn require_non_empty(field: &str, value: &str) -> Result<()> {
	if value.trim().is_empty() {
		return Err(CoreError::InvalidConfig(format!("{field}不能为空")));
	}
	Ok(())
}

fn require_port(port: u16) -> Result<()> {
	if port == 0 {
		return Err(CoreError::InvalidConfig("端口不能为 0".to_string()));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn protocol_round_trip() {
		for protocol in [Protocol::Ssh, Protocol::Rdp] {
			let json = serde_json::to_string(&protocol).expect("序列化失败");
			let back: Protocol = serde_json::from_str(&json).expect("反序列化失败");
			assert_eq!(protocol, back);
		}
	}

	#[test]
	fn protocol_default_ports() {
		assert_eq!(Protocol::Ssh.default_port(), 22);
		assert_eq!(Protocol::Rdp.default_port(), 3389);
	}

	#[test]
	fn protocol_matches_db_check_constraint() {
		assert_eq!(Protocol::Ssh.as_str(), "ssh");
		assert_eq!(Protocol::Rdp.as_str(), "rdp");
		assert!("ssh".parse::<Protocol>().is_ok());
		assert!("telnet".parse::<Protocol>().is_err());
	}

	#[test]
	fn resolution_round_trip() {
		let cases = [
			Resolution::FitWindow,
			Resolution::fixed(1920, 1080).unwrap(),
			Resolution::fit_window_at(2820, 1794).unwrap(),
		];

		for value in cases {
			// 字符串形式（落库）
			let raw = value.to_string();
			assert_eq!(raw.parse::<Resolution>().unwrap(), value);

			// serde 往返（配置文件 / JSON 列）
			let json = serde_json::to_string(&value).expect("序列化失败");
			let back: Resolution = serde_json::from_str(&json).expect("反序列化失败");
			assert_eq!(value, back);
		}
	}

	#[test]
	fn resolution_string_forms() {
		assert_eq!(Resolution::FitWindow.to_string(), "fit-window");
		// 跟随目标的三种字符串形态
		assert_eq!(
			Resolution::FitWindowAt {
				width: 2820,
				height: 1794
			}
			.to_string(),
			"fit-window:2820x1794"
		);
		assert_eq!(
			"fit-window:2820x1794".parse::<Resolution>().unwrap(),
			Resolution::FitWindowAt {
				width: 2820,
				height: 1794
			}
		);
		assert!(
			"fit-window:abc".parse::<Resolution>().is_err(),
			"跟随目标缺尺寸应被拒绝"
		);
		assert!(
			"fit-window:100x720".parse::<Resolution>().is_err(),
			"跟随目标越界应被拒绝"
		);
		assert!("fit-window:".parse::<Resolution>().is_err());
		assert_eq!(
			Resolution::fixed(1920, 1080).unwrap().to_string(),
			"1920x1080"
		);
	}

	/// DPI 缩放只接受 100–500 且为 25 的倍数——disp 规范 + Windows 档位。
	#[test]
	fn dpi_validation() {
		assert!(validate_dpi(100).is_ok());
		assert!(validate_dpi(200).is_ok());
		assert!(validate_dpi(500).is_ok());
		assert!(validate_dpi(99).is_err(), "低于 100 应被拒绝");
		assert!(validate_dpi(501).is_err(), "高于 500 应被拒绝");
		assert!(validate_dpi(130).is_err(), "不是 25 的倍数应被拒绝");
		assert!(validate_dpi(0).is_err());

		// 落库默认值必须合法，否则新主机会在 validate 上翻车
		assert!(validate_dpi(DEFAULT_DPI).is_ok());
		assert_eq!(DEFAULT_DPI, 100);
	}

	/// 旧数据（无 dpi 字段）反序列化应落回默认值 100。
	#[test]
	fn display_settings_without_dpi_defaults_to_100() {
		let raw = r#"{"resolution":"fit-window","scale":{"mode":"fit","factor":1.0}}"#;
		let settings: DisplaySettings = serde_json::from_str(raw).expect("反序列化失败");
		assert_eq!(settings.dpi, DEFAULT_DPI);
	}

	#[test]
	fn resolution_rejects_out_of_range() {
		assert!(Resolution::fixed(100, 720).is_err(), "宽度过小应被拒绝");
		assert!(Resolution::fixed(1920, 100).is_err(), "高度过小应被拒绝");
		assert!(Resolution::fixed(9000, 1080).is_err(), "宽度过大应被拒绝");
		assert!(Resolution::fixed(1920, 9000).is_err(), "高度过大应被拒绝");
		assert!("garbage".parse::<Resolution>().is_err());
		assert!("1920".parse::<Resolution>().is_err());
		assert!("axb".parse::<Resolution>().is_err());
	}

	#[test]
	fn auth_method_round_trip() {
		let password = AuthMethod::Password;
		let json = serde_json::to_string(&password).expect("序列化失败");
		assert_eq!(serde_json::from_str::<AuthMethod>(&json).unwrap(), password);

		let public_key = AuthMethod::PublicKey(PublicKeyAuth {
			key_path: "~/.ssh/id_rsa".to_string(),
			has_passphrase: true,
		});
		let json = serde_json::to_string(&public_key).expect("序列化失败");
		assert_eq!(
			serde_json::from_str::<AuthMethod>(&json).unwrap(),
			public_key
		);

		assert_eq!(password.as_str(), "password");
		assert_eq!(public_key.as_str(), "publickey");
		assert!(public_key.is_public_key());
	}

	#[test]
	fn scale_settings_validation() {
		assert!(
			ScaleSettings::new(ScaleMode::Fit, 0.0).is_ok(),
			"非 custom 模式不校验比例"
		);
		assert!(ScaleSettings::new(ScaleMode::Custom, 1.5).is_ok());
		assert!(ScaleSettings::new(ScaleMode::Custom, 0.1).is_err());
		assert!(ScaleSettings::new(ScaleMode::Custom, 5.0).is_err());
		assert!(ScaleSettings::new(ScaleMode::Custom, f32::NAN).is_err());
	}

	#[test]
	fn scale_mode_round_trip() {
		for mode in [
			ScaleMode::Fit,
			ScaleMode::Fill,
			ScaleMode::Actual,
			ScaleMode::Custom,
		] {
			assert_eq!(mode.to_string().parse::<ScaleMode>().unwrap(), mode);
		}
		assert!("stretch".parse::<ScaleMode>().is_err());
	}

	#[test]
	fn host_round_trip() {
		let host = Host {
			id: Some(7),
			name: "测试机".to_string(),
			protocol: Protocol::Rdp,
			host: "192.168.10.20".to_string(),
			port: 3389,
			username: Some("Administrator".to_string()),
			auth: AuthMethod::Password,
			proxy_id: None,
			jump_host_id: None,
			group_id: Some(1),
			display: DisplaySettings {
				resolution: Resolution::fixed(1920, 1080).unwrap(),
				scale: ScaleSettings::new(ScaleMode::Custom, 1.5).unwrap(),
				dpi: 150,
			},
			clipboard: true,
			created_at: 1_700_000_000,
			updated_at: 1_700_000_100,
		};

		let json = serde_json::to_string(&host).expect("序列化失败");
		let back: Host = serde_json::from_str(&json).expect("反序列化失败");
		assert_eq!(host, back);
		assert!(host.validate().is_ok());
	}

	#[test]
	fn host_validation_rejects_bad_input() {
		let mut host = Host::draft(Protocol::Ssh);
		assert!(host.validate().is_err(), "空名称应被拒绝");

		host.name = "ok".to_string();
		assert!(host.validate().is_err(), "空地址应被拒绝");

		host.host = "example.com".to_string();
		host.port = 0;
		assert!(host.validate().is_err(), "端口 0 应被拒绝");

		host.port = 22;
		assert!(host.validate().is_ok());

		host.auth = AuthMethod::PublicKey(PublicKeyAuth {
			key_path: "  ".to_string(),
			has_passphrase: false,
		});
		assert!(host.validate().is_err(), "空白私钥路径应被拒绝");
	}

	#[test]
	fn host_draft_uses_protocol_default_port() {
		assert_eq!(Host::draft(Protocol::Ssh).port, 22);
		assert_eq!(Host::draft(Protocol::Rdp).port, 3389);
	}

	#[test]
	fn proxy_and_group_round_trip() {
		let proxy = Proxy {
			id: Some(3),
			name: "本地代理".to_string(),
			kind: ProxyKind::Socks5,
			host: "127.0.0.1".to_string(),
			port: 1080,
			username: Some("user".to_string()),
		};
		let json = serde_json::to_string(&proxy).expect("序列化失败");
		assert_eq!(serde_json::from_str::<Proxy>(&json).unwrap(), proxy);
		assert!(proxy.validate().is_ok());

		let group = Group {
			id: None,
			name: "生产".to_string(),
			parent_id: Some(1),
		};
		let json = serde_json::to_string(&group).expect("序列化失败");
		assert_eq!(serde_json::from_str::<Group>(&json).unwrap(), group);
		assert!(group.validate().is_ok());
	}

	#[test]
	fn proxy_validation_rejects_bad_input() {
		let base = Proxy {
			id: None,
			name: "p".to_string(),
			kind: ProxyKind::Http,
			host: "h".to_string(),
			port: 8080,
			username: None,
		};

		let mut empty_name = base.clone();
		empty_name.name = "   ".to_string();
		assert!(empty_name.validate().is_err());

		let mut bad_port = base;
		bad_port.port = 0;
		assert!(bad_port.validate().is_err());
	}

	#[test]
	fn proxy_kind_matches_db_check_constraint() {
		assert_eq!(ProxyKind::Socks5.as_str(), "socks5");
		assert_eq!(ProxyKind::Http.as_str(), "http");
		assert!("https".parse::<ProxyKind>().is_err());
	}
}
