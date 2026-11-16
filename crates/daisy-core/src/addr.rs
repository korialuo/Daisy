//! 主机地址：域名 / IPv4 / IPv6 的统一表示。
//!
//! SSH 与 RDP 两条路径**必须用同一套解析规则**，否则会出现「SSH 能连、
//! RDP 连不上」这类只在某个协议上复现的问题。因此规则放在 `daisy-core`，
//! 由两边共用。
//!
//! ## 几种写法与它们的真实含义
//!
//! | 用户输入 | 判定 | 交给底层的形式 |
//! | --- | --- | --- |
//! | `example.com` | 域名 | `example.com` |
//! | `192.168.10.20` | IPv4 字面量 | `192.168.10.20` |
//! | `::1`、`2001:db8::1` | IPv6 字面量 | 原样（**不带方括号**） |
//! | `[::1]`、`[2001:db8::1]` | IPv6 字面量 | 去掉方括号 |
//! | `fe80::1%en0`、`[fe80::1%en0]` | 带区域标识的 IPv6 | 原样（**带 `%区域`**） |
//!
//! ## 区域标识（`%en0`）
//!
//! 链路本地地址（`fe80::/10`）在 IPv6 里**不唯一**：同一台机器每个网卡都
//! 可能有一个 `fe80::` 开头的地址，光凭地址本身分不清要从哪块网卡出去。
//! 所以这类地址必须带区域标识——macOS / Linux 写接口名（`%en0`），
//! Windows 写接口索引（`%12`）。
//!
//! 这个后缀**必须原样传给 `getaddrinfo`**：它才是真正解出 `scope_id` 的地方。
//! 只在内部表示里保留、不往下传，就会得到「地址看着对、连不上」。
//! 因此 [`HostAddr::host`] 对这类地址返回的是带 `%` 的完整串，与
//! 「IPv6 不带方括号」并不冲突——去括号与保留区域是两件事。
//!
//! **方括号只在「地址与端口写在一起」时才需要**（`[::1]:22`），因为裸冒号
//! 有歧义。但 `getaddrinfo` 与托管的 `lookup_host` 都要求参数里
//! **不带**方括号——把 `[::1]` 直接喂进去会解析失败。所以这里统一在入口
//! 剥掉、在展示时加回。

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::error::CoreError;

/// 域名最大总长度（RFC 1035）。
const MAX_DOMAIN_LEN: usize = 253;

/// 单个标签最大长度。
const MAX_LABEL_LEN: usize = 63;

/// 主机地址的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostKind {
	/// 域名，需要 DNS 解析。
	Domain,
	/// IPv4 字面量。
	V4,
	/// IPv6 字面量。
	V6,
}

impl HostKind {
	/// 稳定的字符串表示，用于日志与界面展示。
	#[must_use]
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Domain => "域名",
			Self::V4 => "IPv4",
			Self::V6 => "IPv6",
		}
	}

	/// 是否为 IP 字面量（不需要 DNS）。
	#[must_use]
	pub fn is_literal(&self) -> bool {
		matches!(self, Self::V4 | Self::V6)
	}
}

impl fmt::Display for HostKind {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

/// 一个已规范化的主机地址。
///
/// 构造只能经 [`HostAddr::parse`]，因此内部不变量（`host` 不含方括号、
/// 非空、是合法域名或 IP 字面量）始终成立。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostAddr {
	host: String,
	kind: HostKind,
}

impl HostAddr {
	/// 解析并规范化。
	///
	/// 接受域名、IPv4、IPv6 字面量；IPv6 允许带方括号书写。
	pub fn parse(input: &str) -> Result<Self, CoreError> {
		let text = input.trim();
		if text.is_empty() {
			return Err(CoreError::InvalidConfig("主机地址为空".to_string()));
		}

		// 形式一：[IPv6]
		if let Some(inner) = text.strip_prefix('[') {
			let Some(inner) = inner.strip_suffix(']') else {
				return Err(CoreError::InvalidConfig(format!(
					"IPv6 地址缺少右方括号：{text}"
				)));
			};
			return Self::from_ipv6(inner, text);
		}

		// 形式二：裸 IPv6 或裸 IPv4
		if text.contains(':') {
			// 含冒号又没有方括号，只可能是裸 IPv6 字面量
			return Self::from_ipv6(text, text);
		}

		if let Ok(v4) = text.parse::<Ipv4Addr>() {
			return Ok(Self {
				host: v4.to_string(),
				kind: HostKind::V4,
			});
		}

		validate_domain(text)?;
		Ok(Self {
			host: text.to_ascii_lowercase(),
			kind: HostKind::Domain,
		})
	}

	fn from_ipv6(candidate: &str, original: &str) -> Result<Self, CoreError> {
		// 区域标识：`fe80::1%en0`。分开解析——`Ipv6Addr::from_str` 不认 `%`
		let (addr_text, zone) = match candidate.split_once('%') {
			Some((addr, zone)) => (addr, Some(zone)),
			None => (candidate, None),
		};

		let addr: Ipv6Addr = addr_text
			.trim()
			.parse()
			.map_err(|_| CoreError::InvalidConfig(format!("IPv6 地址格式不正确：{original}")))?;

		// 用标准写法归一（压缩零段），避免 `2001:0db8::1` 与 `2001:db8::1` 被当成两个地址
		let mut host = addr.to_string();
		if let Some(zone) = zone {
			validate_zone(zone, original)?;
			host.push('%');
			// 接口名大小写归一：同一块网卡不该因为写法不同被当成两台主机
			host.push_str(&zone.to_ascii_lowercase());
		}

		Ok(Self {
			host,
			kind: HostKind::V6,
		})
	}

	/// 交给底层解析函数的形式（IPv6 **不带**方括号，但**保留** `%区域`）。
	///
	/// `%en0` 这类区域标识必须原样交给 `getaddrinfo`——它才是解出
	/// `scope_id` 的地方，剥掉就连不上链路本地地址。
	#[must_use]
	pub fn host(&self) -> &str {
		&self.host
	}

	/// 地址种类。
	#[must_use]
	pub fn kind(&self) -> HostKind {
		self.kind
	}

	/// 是否需要 DNS 解析。
	#[must_use]
	pub fn needs_dns(&self) -> bool {
		self.kind == HostKind::Domain
	}

	/// 用于界面与日志的写法：IPv6 加回方括号，一眼能看出是 IPv6。
	#[must_use]
	pub fn display(&self) -> String {
		match self.kind {
			HostKind::V6 => format!("[{}]", self.host),
			_ => self.host.clone(),
		}
	}

	/// 取出内部的规范化字符串。
	#[must_use]
	pub fn into_string(self) -> String {
		self.host
	}
}

/// 拼「地址:端口」给人看的形式。
///
/// IPv6 必须加方括号：`::1:3389` 里分不清哪一段是端口，而 `[::1]:3389` 一眼
/// 就明白（这也是 URL 与 `ssh` 的写法）。判断只看有没有冒号——不需要重新解析，
/// 库里存的老数据也不会因为格式问题显示不出来。
#[must_use]
pub fn endpoint(host: &str, port: u16) -> String {
	if host.contains(':') {
		format!("[{host}]:{port}")
	} else {
		format!("{host}:{port}")
	}
}

impl fmt::Display for HostAddr {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.display())
	}
}

impl std::str::FromStr for HostAddr {
	type Err = CoreError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Self::parse(s)
	}
}

/// 校验 IPv6 区域标识（`%` 后面那段）。
///
/// 只做语法检查，**不校验它是否真的存在**：接口名由系统解析，平台差异大
/// （`en0` / `eth0` / 数字索引），在这里猜等于把「地址写错」和「网卡没插」
/// 混成一句话。写错了会在连接时报解析失败，那才是能定位问题的地方。
fn validate_zone(zone: &str, original: &str) -> Result<(), CoreError> {
	if zone.is_empty() {
		return Err(CoreError::InvalidConfig(format!(
			"IPv6 区域标识为空（`%` 后面要写网卡名，例如 fe80::1%en0）：{original}"
		)));
	}
	if zone.len() > 64 {
		return Err(CoreError::InvalidConfig(format!(
			"IPv6 区域标识过长（{} 字符）：{original}",
			zone.len()
		)));
	}
	if let Some(bad) = zone
		.chars()
		.find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
	{
		return Err(CoreError::InvalidConfig(format!(
			"IPv6 区域标识里有非法字符 {bad:?}：{original}"
		)));
	}
	Ok(())
}

/// 校验域名写法的合法性。
///
/// 刻意**不**做得过严：合法主机名里允许下划线（某些内网环境确实在用），
/// 也允许单标签（`nas`、`router`）。但明显的错误要拦住——
/// 尤其是「看起来像 IP 却写错了」这一类，它最容易让人困惑。
fn validate_domain(text: &str) -> Result<(), CoreError> {
	if text.len() > MAX_DOMAIN_LEN {
		return Err(CoreError::InvalidConfig(format!(
			"主机名过长（{} 字符，上限 {MAX_DOMAIN_LEN}）",
			text.len()
		)));
	}

	for forbidden in [
		'/', '\\', '@', ' ', '\t', ',', '?', '#', '*', '\'', '"', '%',
	] {
		if text.contains(forbidden) {
			return Err(CoreError::InvalidConfig(format!(
				"主机名不能包含 {forbidden:?}：{text}"
			)));
		}
	}

	let labels: Vec<&str> = text.split('.').collect();

	// 全数字的标签组合，通常是想写 IP 却写错了——直接点出来比让它去走 DNS 好得多
	if labels
		.iter()
		.all(|label| !label.is_empty() && label.chars().all(|c| c.is_ascii_digit()))
	{
		return Err(CoreError::InvalidConfig(format!(
			"{text} 看起来是 IP 地址，但格式不正确（IPv4 每段应为 0–255）"
		)));
	}

	for label in &labels {
		if label.is_empty() {
			return Err(CoreError::InvalidConfig(format!(
				"主机名里有空的标签（是不是多写了点号）：{text}"
			)));
		}
		if label.len() > MAX_LABEL_LEN {
			return Err(CoreError::InvalidConfig(format!(
				"主机名的单个标签过长（{} 字符，上限 {MAX_LABEL_LEN}）：{label}",
				label.len()
			)));
		}
		if let Some(bad) = label
			.chars()
			.find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
		{
			return Err(CoreError::InvalidConfig(format!(
				"主机名里有非法字符 {bad:?}：{label}"
			)));
		}
	}

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_domains() {
		let addr = HostAddr::parse("example.com").expect("应能解析");
		assert_eq!(addr.kind(), HostKind::Domain);
		assert_eq!(addr.host(), "example.com");
		assert!(addr.needs_dns());
		assert!(!addr.kind().is_literal());

		// 前后空白要容忍：从配置文件读出来常常带空格
		let padded = HostAddr::parse("  nas.local  ").expect("应能解析");
		assert_eq!(padded.host(), "nas.local");

		// 域名不区分大小写，统一小写便于比较与查重
		let upper = HostAddr::parse("Example.COM").expect("应能解析");
		assert_eq!(upper.host(), "example.com");

		// 单标签与下划线都要放行（内网主机名常见）
		assert!(HostAddr::parse("router").is_ok());
		assert!(HostAddr::parse("my_host.lan").is_ok());
	}

	#[test]
	fn parses_ipv4_literals() {
		let addr = HostAddr::parse("192.168.10.20").expect("应能解析");
		assert_eq!(addr.kind(), HostKind::V4);
		assert_eq!(addr.host(), "192.168.10.20");
		assert!(!addr.needs_dns());
		assert_eq!(addr.display(), "192.168.10.20");
	}

	#[test]
	fn parses_ipv6_literals_in_both_forms() {
		// 裸写法
		let bare = HostAddr::parse("::1").expect("应能解析");
		assert_eq!(bare.kind(), HostKind::V6);
		assert_eq!(bare.host(), "::1", "交给底层的形式不带方括号");
		assert!(!bare.needs_dns());

		// 带方括号写法：内部必须剥掉
		let bracketed = HostAddr::parse("[::1]").expect("应能解析");
		assert_eq!(bracketed.host(), "::1");
		assert_eq!(bracketed, bare, "两种写法应归一成同一个地址");

		// 展示时加回方括号
		assert_eq!(bracketed.display(), "[::1]");

		let long = HostAddr::parse("2001:db8::1").expect("应能解析");
		assert_eq!(long.kind(), HostKind::V6);
		assert_eq!(long.display(), "[2001:db8::1]");

		let with_brackets = HostAddr::parse("[2001:db8::1]").expect("应能解析");
		assert_eq!(with_brackets, long);
	}

	#[test]
	fn normalizes_ipv6_zero_compression() {
		// 同一个地址的不同写法必须归一，否则「同一主机」的判重会失效
		let padded = HostAddr::parse("2001:0db8:0000:0000:0000:0000:0000:0001").expect("应能解析");
		let compressed = HostAddr::parse("2001:db8::1").expect("应能解析");
		assert_eq!(padded, compressed);
		assert_eq!(padded.host(), "2001:db8::1");
	}

	#[test]
	fn rejects_bad_input() {
		for bad in [
			"",
			"   ",
			"[::1",
			"::1]",
			"1.2.3.999",
			"a b.com",
			"host/name",
			"a..b",
		] {
			assert!(
				HostAddr::parse(bad).is_err(),
				"{bad:?} 应被拒绝，实际通过了"
			);
		}
	}

	#[test]
	fn miswritten_ip_gets_a_helpful_message() {
		// 「看着像 IP 却写错了」是最容易让人困惑的一类，文案要点出来
		let error = HostAddr::parse("1.2.3.999").expect_err("应被拒绝");
		let text = error.to_string();
		assert!(text.contains("IP 地址"), "应指出像 IP：{text}");
		assert!(text.contains("0–255"), "应说明正确范围：{text}");
	}

	#[test]
	fn ipv6_error_message_is_specific() {
		let error = HostAddr::parse("[not-an-ip]").expect_err("应被拒绝");
		assert!(
			error.to_string().contains("IPv6"),
			"应指明是 IPv6 问题：{error}"
		);
	}

	#[test]
	fn host_kind_labels_are_stable() {
		assert_eq!(HostKind::Domain.as_str(), "域名");
		assert_eq!(HostKind::V4.as_str(), "IPv4");
		assert_eq!(HostKind::V6.as_str(), "IPv6");
		assert!(HostKind::V4.is_literal());
		assert!(HostKind::V6.is_literal());
		assert!(!HostKind::Domain.is_literal());
	}

	#[test]
	fn keeps_ipv6_zone_id() {
		// 链路本地地址必须带区域标识，否则「同一台机器的不同网卡」分不开
		let link = HostAddr::parse("fe80::1%en0").expect("应能解析");
		assert_eq!(link.kind(), HostKind::V6);
		assert_eq!(
			link.host(),
			"fe80::1%en0",
			"区域标识必须原样交给 getaddrinfo"
		);
		assert_eq!(link.display(), "[fe80::1%en0]", "展示时加回方括号");

		// 方括号写法同样支持
		let bracketed = HostAddr::parse("[fe80::1%en0]").expect("应能解析");
		assert_eq!(bracketed, link, "两种写法应归一成同一个地址");

		// 地址部分照常压缩零段，区域部分转小写
		let padded =
			HostAddr::parse("FE80:0000:0000:0000:0000:0000:0000:0001%EN0").expect("应能解析");
		assert_eq!(padded.host(), "fe80::1%en0");

		// Windows 用数字索引
		let numeric = HostAddr::parse("fe80::1%12").expect("应能解析");
		assert_eq!(numeric.host(), "fe80::1%12");

		// 带区域与不带区域是两台「不同的」地址，不能判等
		assert_ne!(numeric, HostAddr::parse("fe80::1").expect("应能解析"));
	}

	#[test]
	fn rejects_bad_zone_id() {
		for bad in ["fe80::1%", "fe80::1%en 0", "fe80::1%en:0", "fe80::1%[en0]"] {
			assert!(
				HostAddr::parse(bad).is_err(),
				"{bad:?} 的区域标识应被拒绝，实际通过了"
			);
		}
	}

	#[test]
	fn percent_sign_is_not_a_domain_character() {
		// 「像地址却写错」时不能悄悄当成域名去查 DNS
		let error = HostAddr::parse("192.168.1.1%en0").expect_err("应被拒绝");
		assert!(
			error.to_string().contains('%'),
			"应指出百分号不合法：{error}"
		);
	}

	#[test]
	fn endpoint_brackets_ipv6() {
		// `::1:3389` 分不清哪段是端口；`[::1]:3389` 一眼就明白
		assert_eq!(endpoint("::1", 3389), "[::1]:3389");
		assert_eq!(endpoint("fe80::1%en0", 3389), "[fe80::1%en0]:3389");
		assert_eq!(endpoint("192.168.10.20", 3389), "192.168.10.20:3389");
		assert_eq!(endpoint("win.example.com", 13389), "win.example.com:13389");
	}

	#[test]
	fn round_trips_through_string() {
		use std::str::FromStr;

		for raw in ["example.com", "10.0.0.1", "::1", "[2001:db8::1]"] {
			let addr = HostAddr::parse(raw).expect("应能解析");
			let reparsed = HostAddr::from_str(&addr.display()).expect("展示形式应能再解析回去");
			assert_eq!(addr, reparsed, "{raw} 往返后不一致");
		}
	}

	#[test]
	fn into_string_drops_brackets() {
		let addr = HostAddr::parse("[::1]").expect("应能解析");
		assert_eq!(addr.into_string(), "::1");
	}
}
