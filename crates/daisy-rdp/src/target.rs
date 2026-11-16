//! RDP 目标：地址如何交给 FreeRDP。
//!
//! ## FreeRDP 支持到什么程度（读源码 + 实测确认）
//!
//! `libfreerdp/core/tcp.c` 的 `freerdp_tcp_resolve_host()`：
//!
//! ```c
//! struct addrinfo hints = { .ai_family = AF_UNSPEC,
//!                           .ai_socktype = SOCK_STREAM,
//!                           .ai_flags = ai_flags };
//! status = getaddrinfo(hostname, service, &hints, &result);
//! ```
//!
//! `AF_UNSPEC` 意味着**域名、IPv4、IPv6 三种写法它都能解析**；随后的连接循环
//! （`freerdp_tcp_connect()`）会**逐个尝试返回的地址直到连上**，
//! 所以双栈主机的回退是它自己做的。
//!
//! 相关的设置项：
//!
//! | 设置 | 作用 | Daisy 对应 |
//! | --- | --- | --- |
//! | `FreeRDP_ServerHostname` | 主机名或 IP 字面量 | [`RdpTarget::hostname`] |
//! | `FreeRDP_ServerPort` | 端口，**单独传** | [`RdpTarget::port`] |
//!
//! > `FreeRDP_PreferIPv6OverIPv4` 在 FreeRDP 3 里已被标成 `SETTINGS_DEPRECATED`
//! > （`settings_types_private.h`），替代者是 `FreeRDP_ForceIPvX`（4 / 6，对应
//! > `xfreerdp` 的 `-4` / `-6`：**只**用该地址族，而不是排到前面）。
//! > Daisy 两个都不设：连接循环本来就会逐个地址重试到连上，
//! > 而 `getaddrinfo` 的默认顺序已按 RFC 6724 处理（双栈主机通常先试 IPv6）。
//! > 真需要「强行走 IPv6」时再暴露 `ForceIPvX` 即可——现在加一个没人会设的
//! > 开关只会多一处要维护的界面。
//!
//! > `FreeRDP_IPv6Enabled` **不是输入项**——它由服务端返回的地址族推导出来
//! > （`info.c` 里 `settings->IPv6Enabled = (clientAddressFamily == ADDRESS_FAMILY_INET6)`），
//! > 客户端不该去设置它。
//!
//! ## Daisy 这边只需要做两件事
//!
//! 1. **把地址规范化**。`getaddrinfo` **不接受方括号**——`[::1]` 会直接解析失败。
//!    而用户在界面里很可能填 `[::1]`（那是 URL 的写法），所以入口处统一剥掉。
//!    规则复用 [`daisy_core::HostAddr`]，与 SSH 侧完全一致。
//!    **但 `%区域`（`fe80::1%en0`）必须保留**：链路本地地址靠它选网卡，
//!    剥掉就成了「地址看着对、连不上」。
//! 2. **端口不拼进 hostname**。拼成 `[::1]:3389` 会让 `getaddrinfo` 二次解析失败。

use daisy_core::{CoreError, HostAddr, HostKind};

/// RDP 默认端口。
pub const DEFAULT_RDP_PORT: u16 = 3389;

/// 一个 RDP 连接目标。
///
/// 构造即校验，因此内部不变量（主机已规范化、端口非 0）始终成立，
/// 后续写 FreeRDP 设置时不需要再判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdpTarget {
	addr: HostAddr,
	port: u16,
}

impl RdpTarget {
	/// 用默认端口 3389 构造。
	pub fn host_only(host: &str) -> Result<Self, CoreError> {
		Self::new(host, DEFAULT_RDP_PORT)
	}

	/// 构造并校验。
	///
	/// 接受域名、IPv4、IPv6；IPv6 可以带方括号书写。
	pub fn new(host: &str, port: u16) -> Result<Self, CoreError> {
		if port == 0 {
			return Err(CoreError::InvalidConfig("端口不能为 0".to_string()));
		}

		Ok(Self {
			addr: HostAddr::parse(host)?,
			port,
		})
	}

	/// 写进 `FreeRDP_ServerHostname` 的字符串。
	///
	/// **IPv6 不带方括号，但保留 `%区域`**（`fe80::1%en0`）——区域标识交给
	/// `getaddrinfo` 才能解出 `scope_id`。域名已转小写，IP 字面量已压缩归一。
	#[must_use]
	pub fn hostname(&self) -> &str {
		self.addr.host()
	}

	/// 写进 `FreeRDP_ServerPort` 的端口。不要拼进 hostname。
	#[must_use]
	pub fn port(&self) -> u16 {
		self.port
	}

	/// 主机地址种类。
	#[must_use]
	pub fn host_kind(&self) -> HostKind {
		self.addr.kind()
	}

	/// 是否需要 DNS 解析。
	#[must_use]
	pub fn needs_dns(&self) -> bool {
		self.addr.needs_dns()
	}

	/// 规范化后的主机写法（IPv6 带方括号，**不含端口**）。
	///
	/// 与 [`Self::hostname`] 的区别：这个用于**给人看**的界面回显，
	/// 那个用于**喂给 FreeRDP**。
	#[must_use]
	pub fn host_display(&self) -> String {
		self.addr.display()
	}

	/// 规范化后的地址（IPv6 带方括号），用于日志与界面回显。
	#[must_use]
	pub fn display(&self) -> String {
		format!("{}:{}", self.host_display(), self.port)
	}

	/// 一句人话描述，用于连接日志。
	#[must_use]
	pub fn describe(&self) -> String {
		let mut text = format!("{}（{}）", self.display(), self.host_kind());
		if self.needs_dns() {
			text.push_str("，需 DNS 解析");
		}
		text
	}
}

impl std::fmt::Display for RdpTarget {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.display())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn accepts_ipv4_literal() {
		let target = RdpTarget::host_only("192.168.10.20").expect("应能构造");
		assert_eq!(target.hostname(), "192.168.10.20");
		assert_eq!(target.port(), DEFAULT_RDP_PORT);
		assert_eq!(target.host_kind(), HostKind::V4);
		assert!(!target.needs_dns());

		let custom = RdpTarget::new("192.168.10.20", 13389).expect("应能构造");
		assert_eq!(custom.port(), 13389);
	}

	#[test]
	fn accepts_domain_name() {
		let target = RdpTarget::new("win.example.com", DEFAULT_RDP_PORT).expect("应能构造");
		assert_eq!(target.hostname(), "win.example.com");
		assert_eq!(target.host_kind(), HostKind::Domain);
		assert!(target.needs_dns());

		// 统一小写，避免同一个主机在库里存成两条
		let upper = RdpTarget::host_only("WIN.Example.COM").expect("应能构造");
		assert_eq!(upper.hostname(), "win.example.com");
		assert_eq!(
			upper,
			RdpTarget::host_only("win.example.com").expect("应能构造")
		);
	}

	#[test]
	fn strips_brackets_from_ipv6() {
		// 这是最要紧的一条：getaddrinfo 不接受方括号，
		// 带着方括号喂进去会直接解析失败
		let bracketed = RdpTarget::host_only("[::1]").expect("应能构造");
		assert_eq!(
			bracketed.hostname(),
			"::1",
			"写进 ServerHostname 的必须不带方括号"
		);
		assert_eq!(bracketed.host_kind(), HostKind::V6);
		assert!(!bracketed.needs_dns());
		assert_eq!(bracketed.display(), "[::1]:3389", "展示时加回方括号");

		let bare = RdpTarget::new("2001:db8::1", 3389).expect("应能构造");
		assert_eq!(bare.hostname(), "2001:db8::1");

		let same = RdpTarget::new("[2001:db8::1]", 3389).expect("应能构造");
		assert_eq!(bare, same, "两种 IPv6 写法应归一");
	}

	#[test]
	fn port_is_never_baked_into_the_hostname() {
		// 拼成 [::1]:13389 会让 getaddrinfo 二次解析失败
		let target = RdpTarget::new("[::1]", 13389).expect("应能构造");
		assert_eq!(target.hostname(), "::1");
		assert_eq!(target.port(), 13389, "端口应单独通过 ServerPort 传");

		// IPv6 字面量本身含冒号，所以不能简单断言「不含冒号」；
		// 要断言的是「不含端口后缀」与「不带方括号」
		assert!(!target.hostname().ends_with(":13389"));
		assert!(
			!target.hostname().starts_with('['),
			"喂给 getaddrinfo 的不能带方括号"
		);
		assert!(target.display().ends_with(":13389"), "展示形式应带端口");
	}

	#[test]
	fn rejects_bad_input() {
		assert!(RdpTarget::new("", 3389).is_err(), "空主机名应被拒绝");
		assert!(
			RdpTarget::new("win.example.com", 0).is_err(),
			"0 端口应被拒绝"
		);
		assert!(
			RdpTarget::new("1.2.3.999", 3389).is_err(),
			"畸形 IP 应被拒绝"
		);
		assert!(
			RdpTarget::new("[::1", 3389).is_err(),
			"方括号不配对应被拒绝"
		);
	}

	#[test]
	fn keeps_zone_id_for_link_local_addresses() {
		// 链路本地地址靠 `%en0` 选网卡，丢了就「地址看着对、连不上」
		let link = RdpTarget::host_only("fe80::1%en0").expect("应能构造");
		assert_eq!(link.hostname(), "fe80::1%en0", "区域标识必须原样保留");
		assert_eq!(link.host_kind(), HostKind::V6);
		assert_eq!(link.display(), "[fe80::1%en0]:3389");

		// 方括号写法同样支持，且与裸写法归一
		let bracketed = RdpTarget::host_only("[fe80::1%en0]").expect("应能构造");
		assert_eq!(bracketed, link);

		// 端口照样分开传
		let custom = RdpTarget::new("[fe80::1%en0]", 13389).expect("应能构造");
		assert!(!custom.hostname().contains("13389"));
		assert_eq!(custom.display(), "[fe80::1%en0]:13389");
	}

	#[test]
	fn describe_mentions_dns_only_for_domains() {
		let domain = RdpTarget::host_only("win.example.com").expect("应能构造");
		assert!(domain.describe().contains("需 DNS 解析"));

		let literal = RdpTarget::host_only("10.0.0.1").expect("应能构造");
		assert!(!literal.describe().contains("需 DNS 解析"));
	}

	#[test]
	fn display_round_trips() {
		for raw in ["10.0.0.1", "win.example.com", "[::1]", "[2001:db8::1]"] {
			let target = RdpTarget::host_only(raw).expect("应能构造");
			// 用不含端口的主机写法回灌，验证展示形式仍可解析
			let reparsed =
				RdpTarget::new(&target.host_display(), target.port()).expect("应能再解析");
			assert_eq!(target, reparsed, "{raw} 往返后不一致");
		}
	}

	#[test]
	fn host_display_and_hostname_differ_only_for_ipv6() {
		let v4 = RdpTarget::host_only("10.0.0.1").expect("应能构造");
		assert_eq!(v4.host_display(), v4.hostname(), "IPv4 两者应一致");

		let domain = RdpTarget::host_only("win.example.com").expect("应能构造");
		assert_eq!(domain.host_display(), domain.hostname(), "域名两者应一致");

		let v6 = RdpTarget::host_only("[::1]").expect("应能构造");
		assert_eq!(v6.host_display(), "[::1]", "给人看的带方括号");
		assert_eq!(v6.hostname(), "::1", "给 FreeRDP 的不带方括号");
	}
}
