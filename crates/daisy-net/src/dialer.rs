//! 连接抽象：把「怎么到达目标主机」这件事收成一个 trait。
//!
//! S6 只实现直连；S7 会加 SOCKS5 与 HTTP CONNECT 两个实现。
//! 两者共用本文件的接口，SSH 与 RDP 都不需要知道自己走的是哪种路径。
//!
//! ```text
//!   DirectDialer ─┐
//!   Socks5Dialer ─┼─▶ dyn Dialer ──▶ BoxStream ──▶ russh / FreeRDP 桥接
//!   HttpDialer  ──┘
//! ```
//!
//! ## 地址写法
//!
//! 主机部分的解析规则来自 [`daisy_core::HostAddr`]，**与 RDP 侧共用同一套**，
//! 因此域名 / IPv4 / IPv6 在两条路径上行为一致。IPv6 字面量在内部
//! 一律不带方括号（`getaddrinfo` 与托管解析函数都不接受方括号）。
//!
//! ## 为什么返回 `BoxFuture` 而不是 `async fn`
//!
//! trait 里的 `async fn` 目前不满足 dyn 兼容。代理要在运行时按配置切换，
//! 必须能用 `Box<dyn Dialer>` 持有，所以这里显式返回装箱 future。
//! 代价是每次拨号多一次堆分配——连接建立属于低频操作，可以接受。

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use daisy_core::{HostAddr, HostKind};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, lookup_host};

use crate::error::{NetError, Result};

/// 默认连接超时。
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 双向字节流。TCP、代理隧道、SSH 通道都满足它。
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> AsyncStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// 装箱后的双向字节流。
pub type BoxStream = Box<dyn AsyncStream>;

/// 拨号目标：一个已规范化的主机 + 端口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetAddr {
	addr: HostAddr,
	port: u16,
}

impl TargetAddr {
	/// 由已校验的主机与端口构造。
	///
	/// 主机必须先经 [`HostAddr::parse`]，因此这里不需要再校验——
	/// 非法地址在进入本层之前就已被拦下。
	#[must_use]
	pub fn new(addr: HostAddr, port: u16) -> Self {
		Self { addr, port }
	}

	/// 解析 `host:port`。
	///
	/// 支持三种写法：`example.com:22`、`192.168.10.20:22`、`[2001:db8::1]:22`。
	/// IPv6 **必须**用方括号，否则冒号有歧义——这也是 URL 的惯例。
	pub fn parse(input: &str) -> Result<Self> {
		let text = input.trim();
		if text.is_empty() {
			return Err(NetError::InvalidTarget("地址为空".to_string()));
		}

		// IPv6 带方括号：[host]:port
		if let Some(rest) = text.strip_prefix('[') {
			let (host, tail) = rest
				.split_once(']')
				.ok_or_else(|| NetError::InvalidTarget(format!("IPv6 地址缺少右方括号：{text}")))?;
			let port = tail
				.strip_prefix(':')
				.ok_or_else(|| NetError::InvalidTarget(format!("IPv6 地址缺少端口：{text}")))?;
			return Self::checked(host, port);
		}

		// 裸 IPv6（含多个冒号）不带端口——要带端口就必须加方括号
		if text.matches(':').count() > 1 {
			return Err(NetError::InvalidTarget(format!(
				"IPv6 地址必须用方括号包裹才能带端口，例如 [::1]:22：{text}"
			)));
		}

		let (host, port) = text.split_once(':').ok_or_else(|| {
			NetError::InvalidTarget(format!("地址缺少端口，应形如 host:port：{text}"))
		})?;
		Self::checked(host, port)
	}

	fn checked(host: &str, port: &str) -> Result<Self> {
		let addr = HostAddr::parse(host)
			.map_err(|error| NetError::InvalidTarget(format!("主机地址不合法：{error}")))?;

		let port: u16 = port
			.trim()
			.parse()
			.map_err(|_| NetError::InvalidTarget(format!("端口不是 0–65535 的整数：{port}")))?;
		if port == 0 {
			return Err(NetError::InvalidTarget("端口不能为 0".to_string()));
		}

		Ok(Self { addr, port })
	}

	/// 主机（IPv6 不带方括号，可直接交给解析函数）。
	#[must_use]
	pub fn host(&self) -> &str {
		self.addr.host()
	}

	/// 主机地址种类。
	#[must_use]
	pub fn host_kind(&self) -> HostKind {
		self.addr.kind()
	}

	/// 端口。
	#[must_use]
	pub fn port(&self) -> u16 {
		self.port
	}

	/// 是否需要 DNS 解析。
	#[must_use]
	pub fn needs_dns(&self) -> bool {
		self.addr.needs_dns()
	}

	/// 用于展示与日志的 `host:port`。IPv6 会加回方括号。
	#[must_use]
	pub fn display(&self) -> String {
		format!("{}:{}", self.addr.display(), self.port)
	}
}

impl std::fmt::Display for TargetAddr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.display())
	}
}

/// 拨号器：按配置的路径建立到目标的字节流。
pub trait Dialer: Send + Sync {
	/// 建立连接。
	fn dial<'a>(
		&'a self,
		target: &'a TargetAddr,
	) -> Pin<Box<dyn Future<Output = Result<BoxStream>> + Send + 'a>>;

	/// 人类可读的路径描述，用于连接日志与错误提示。
	fn describe(&self) -> String;
}

impl std::fmt::Debug for dyn Dialer {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str(&self.describe())
	}
}

/// 直连。不经过任何代理。
#[derive(Debug, Clone)]
pub struct DirectDialer {
	timeout: Duration,
	prefer_ipv6: bool,
}

impl Default for DirectDialer {
	fn default() -> Self {
		Self::new(DEFAULT_CONNECT_TIMEOUT)
	}
}

impl DirectDialer {
	/// 指定连接超时。默认不偏好任何地址族（沿用系统返回的顺序）。
	#[must_use]
	pub fn new(timeout: Duration) -> Self {
		Self {
			timeout,
			prefer_ipv6: false,
		}
	}

	/// 是否把 IPv6 地址排到前面。
	///
	/// 对应 FreeRDP 的 `PreferIPv6OverIPv4`，两侧语义保持一致。
	/// 默认 `false`：沿用 `getaddrinfo` 返回的顺序，与现代操作系统
	/// 的连接策略（RFC 6724）一致，双栈主机通常已经先试 IPv6。
	#[must_use]
	pub fn with_prefer_ipv6(mut self, prefer: bool) -> Self {
		self.prefer_ipv6 = prefer;
		self
	}

	/// 连接超时。
	#[must_use]
	pub fn timeout(&self) -> Duration {
		self.timeout
	}

	/// 是否偏好 IPv6。
	#[must_use]
	pub fn prefers_ipv6(&self) -> bool {
		self.prefer_ipv6
	}

	/// 具体实现。单独暴露是为了在不装箱的情况下直接 await。
	pub async fn connect(&self, target: &TargetAddr) -> Result<TcpStream> {
		// 先把解析与连接分开：解析失败和连不上对用户是两件事，
		// 混在一起就只能报「连接失败」，没法给建议。
		let resolved = lookup_host((target.host(), target.port))
			.await
			.map_err(|source| NetError::Resolve {
				host: target.host().to_string(),
				source,
			})?;

		let mut addrs: Vec<SocketAddr> = resolved.collect();
		if addrs.is_empty() {
			return Err(NetError::NoAddresses {
				host: target.host().to_string(),
			});
		}
		reorder_by_preference(&mut addrs, self.prefer_ipv6);

		// 整个尝试过程共用一个超时预算，而不是每个地址各算一份
		let attempt = async {
			let mut last: Option<std::io::Error> = None;

			for addr in addrs {
				match TcpStream::connect(addr).await {
					Ok(stream) => {
						// RDP 的交互帧大多很小，禁用 Nagle 能明显降低延迟
						if let Err(error) = stream.set_nodelay(true) {
							tracing::debug!("set_nodelay 失败（不影响功能）：{error}");
						}
						return Ok(stream);
					},
					Err(error) => {
						// 一个地址失败就试下一个：双栈主机常常同时解析出
						// A 与 AAAA 记录，只有一边在听是常态
						tracing::debug!("连接 {addr} 失败：{error}");
						last = Some(error);
					},
				}
			}

			Err(last)
		};

		match tokio::time::timeout(self.timeout, attempt).await {
			Err(_) => Err(NetError::Timeout {
				host: target.host().to_string(),
				port: target.port,
				timeout_ms: self.timeout.as_millis() as u64,
			}),
			Ok(Ok(stream)) => Ok(stream),
			Ok(Err(None)) => Err(NetError::NoAddresses {
				host: target.host().to_string(),
			}),
			Ok(Err(Some(error))) => Err(classify(target, error)),
		}
	}
}

impl Dialer for DirectDialer {
	fn dial<'a>(
		&'a self,
		target: &'a TargetAddr,
	) -> Pin<Box<dyn Future<Output = Result<BoxStream>> + Send + 'a>> {
		Box::pin(async move {
			let stream = self.connect(target).await?;
			Ok(Box::new(stream) as BoxStream)
		})
	}

	fn describe(&self) -> String {
		if self.prefer_ipv6 {
			"直连（优先 IPv6）".to_string()
		} else {
			"直连".to_string()
		}
	}
}

/// 按地址族偏好重排。
///
/// 稳定排序：同类地址之间保持解析器给出的顺序（RFC 6724 已经排过一次，
/// 不该被我们打乱）。
fn reorder_by_preference(addrs: &mut [SocketAddr], prefer_ipv6: bool) {
	if !prefer_ipv6 {
		return;
	}
	addrs.sort_by_key(|addr| u8::from(!addr.is_ipv6()));
}

/// 把 `io::Error` 归到具体的错误变体上。
fn classify(target: &TargetAddr, error: std::io::Error) -> NetError {
	match error.kind() {
		std::io::ErrorKind::ConnectionRefused => NetError::Refused {
			host: target.host().to_string(),
			port: target.port,
		},
		std::io::ErrorKind::TimedOut => NetError::Timeout {
			host: target.host().to_string(),
			port: target.port,
			timeout_ms: 0,
		},
		_ => NetError::Connect {
			host: target.host().to_string(),
			port: target.port,
			source: error,
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
	use tokio::io::{AsyncReadExt, AsyncWriteExt};

	/// 测试里最常用的构造：主机字符串应当是合法的。
	fn target(host: &str, port: u16) -> TargetAddr {
		TargetAddr::new(HostAddr::parse(host).expect("测试地址应合法"), port)
	}

	// ---------- 地址解析 ----------

	#[test]
	fn parse_accepts_all_three_forms() {
		let domain = TargetAddr::parse("example.com:22").expect("应能解析域名");
		assert_eq!(domain.host(), "example.com");
		assert_eq!(domain.host_kind(), HostKind::Domain);
		assert!(domain.needs_dns());

		let v4 = TargetAddr::parse("10.0.0.1:22").expect("应能解析 IPv4");
		assert_eq!(v4.host_kind(), HostKind::V4);
		assert!(!v4.needs_dns());

		let v6 = TargetAddr::parse("[2001:db8::1]:2222").expect("应能解析 IPv6");
		assert_eq!(v6.host_kind(), HostKind::V6);
		assert_eq!(v6.host(), "2001:db8::1", "交给底层的形式不带方括号");
		assert_eq!(v6.port(), 2222);
		assert_eq!(v6.display(), "[2001:db8::1]:2222", "展示时加回方括号");

		// 前后空白要容忍——从配置文件读出来常常带空格
		let padded = TargetAddr::parse("  10.0.0.1:65535  ").expect("应能解析");
		assert_eq!(padded.port(), 65535);
	}

	#[test]
	fn parse_rejects_bad_input() {
		for bad in [
			"",
			"example.com",
			"example.com:0",
			"example.com:abc",
			":22",
			// 裸 IPv6 带端口有歧义，必须加方括号
			"::1:22",
			// 方括号不配对
			"[::1:22",
		] {
			assert!(
				TargetAddr::parse(bad).is_err(),
				"{bad:?} 应被拒绝，实际通过了"
			);
		}
	}

	#[test]
	fn bare_ipv6_port_error_explains_the_bracket_rule() {
		let error = TargetAddr::parse("::1:22").expect_err("应被拒绝");
		let text = error.to_string();
		assert!(text.contains("[::1]:22"), "应给出正确写法：{text}");
	}

	#[test]
	fn display_round_trips() {
		for raw in ["10.0.0.1:22", "[::1]:22", "example.com:3389"] {
			let parsed = TargetAddr::parse(raw).expect("应能解析");
			let reparsed = TargetAddr::parse(&parsed.display()).expect("展示形式应能再解析");
			assert_eq!(parsed, reparsed, "{raw} 往返后不一致");
		}
	}

	// ---------- 地址族偏好 ----------

	#[test]
	fn reorder_puts_ipv6_first_when_asked() {
		let v4 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 80));
		let v6 = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 80, 0, 0));

		let mut addrs = vec![v4, v6];
		reorder_by_preference(&mut addrs, true);
		assert_eq!(addrs, vec![v6, v4], "开启偏好后 IPv6 应排前面");

		// 关闭偏好时不动顺序：解析器已经按 RFC 6724 排过
		let mut addrs = vec![v4, v6];
		reorder_by_preference(&mut addrs, false);
		assert_eq!(addrs, vec![v4, v6], "关闭偏好时不应改动顺序");
	}

	#[test]
	fn reorder_is_stable_for_same_family() {
		let a = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 80));
		let b = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 80));

		let mut addrs = vec![a, b];
		reorder_by_preference(&mut addrs, true);
		assert_eq!(addrs, vec![a, b], "同类地址之间不应被打乱");
	}

	#[test]
	fn dialer_describes_the_preference() {
		let plain = DirectDialer::default();
		assert!(!plain.prefers_ipv6());
		assert_eq!(plain.describe(), "直连");

		let v6_first = DirectDialer::default().with_prefer_ipv6(true);
		assert!(v6_first.prefers_ipv6());
		assert!(v6_first.describe().contains("IPv6"));
	}

	// ---------- 真实连接 ----------

	/// 起一个只做回显的监听器，返回它的端口与任务句柄。
	///
	/// `accepts` 指定接受多少个连接后收工。写成固定次数而不是无限循环，
	/// 是为了让测试能 `await` 它，从而确认回显真的发生过——
	/// 而不是只证明「连上了」。
	async fn echo_listener_on(bind: &str, accepts: usize) -> (u16, tokio::task::JoinHandle<()>) {
		let listener = tokio::net::TcpListener::bind(bind)
			.await
			.unwrap_or_else(|error| panic!("绑定 {bind} 失败：{error}"));
		let port = listener.local_addr().expect("取地址失败").port();

		let handle = tokio::spawn(async move {
			for _ in 0..accepts {
				let Ok((mut socket, _)) = listener.accept().await else {
					return;
				};
				let mut buffer = [0u8; 64];
				if let Ok(n) = socket.read(&mut buffer).await {
					let _ = socket.write_all(&buffer[..n]).await;
				}
			}
		});

		(port, handle)
	}

	async fn round_trip(dialer: &DirectDialer, target: &TargetAddr) -> String {
		let mut stream = dialer.dial(target).await.expect("拨号失败");
		stream.write_all(b"ping").await.expect("写入失败");

		let mut buffer = [0u8; 4];
		stream.read_exact(&mut buffer).await.expect("读取失败");
		String::from_utf8_lossy(&buffer).into_owned()
	}

	#[tokio::test]
	async fn dials_ipv4_literal() {
		let (port, server) = echo_listener_on("127.0.0.1:0", 1).await;
		let dialer = DirectDialer::new(Duration::from_secs(5));
		assert_eq!(
			round_trip(&dialer, &target("127.0.0.1", port)).await,
			"ping"
		);
		server.await.expect("服务端任务失败");
	}

	#[tokio::test]
	async fn dials_ipv6_literal() {
		let (port, server) = echo_listener_on("[::1]:0", 2).await;
		let dialer = DirectDialer::new(Duration::from_secs(5));

		// 裸写法与方括号写法都要能连上
		assert_eq!(round_trip(&dialer, &target("::1", port)).await, "ping");
		assert_eq!(
			round_trip(&dialer, &target("[::1]", port)).await,
			"ping",
			"方括号写法应被规范化"
		);
		server.await.expect("服务端任务失败");
	}

	#[tokio::test]
	async fn dials_domain_name() {
		// localhost 在 /etc/hosts 里同时指向 127.0.0.1 与 ::1，
		// 因此这条用例同时验证了「域名解析」与「一个地址失败后换下一个」
		let (port, server) = echo_listener_on("127.0.0.1:0", 1).await;
		let dialer = DirectDialer::new(Duration::from_secs(5));

		assert_eq!(
			round_trip(&dialer, &target("localhost", port)).await,
			"ping"
		);
		server.await.expect("服务端任务失败");
	}

	#[tokio::test]
	async fn prefers_ipv6_when_dual_stack_is_available() {
		// 同一个端口上分别起 IPv4 与 IPv6 监听器，用 localhost 拨号，
		// 看请求落在哪一边——这直接验证了地址族偏好是否生效。
		let (v4_port, v4_server) = echo_listener_on("127.0.0.1:0", 1).await;
		let Ok(v6_listener) = tokio::net::TcpListener::bind(format!("[::1]:{v4_port}")).await
		else {
			// 端口两端不能同时拿到（极少数情况），跳过而不是让测试假失败
			v4_server.abort();
			return;
		};
		let v6_server = tokio::spawn(async move {
			if let Ok((mut socket, _)) = v6_listener.accept().await {
				let _ = socket.write_all(b"v6").await;
			}
		});

		let dialer = DirectDialer::new(Duration::from_secs(5)).with_prefer_ipv6(true);
		let mut stream = dialer
			.dial(&target("localhost", v4_port))
			.await
			.expect("拨号失败");

		let mut buffer = [0u8; 2];
		stream.read_exact(&mut buffer).await.expect("读取失败");
		assert_eq!(
			&buffer, b"v6",
			"开启偏好后应连到 IPv6 监听器，实际连到了 IPv4"
		);

		v4_server.abort();
		v6_server.await.expect("IPv6 服务端任务失败");
	}

	#[tokio::test]
	async fn refused_connection_is_classified() {
		// 绑一个端口再立刻释放，拿到一个几乎肯定没人听的端口号
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
			.await
			.expect("绑定失败");
		let port = listener.local_addr().expect("取地址失败").port();
		drop(listener);

		let dialer = DirectDialer::new(Duration::from_secs(5));
		let error = dialer
			.connect(&target("127.0.0.1", port))
			.await
			.expect_err("连空端口不应成功");

		assert!(
			matches!(error, NetError::Refused { .. }),
			"应归类为「被拒绝」，实际：{error:?}"
		);
	}

	#[tokio::test]
	async fn refused_ipv6_connection_is_classified() {
		let listener = tokio::net::TcpListener::bind("[::1]:0")
			.await
			.expect("绑定失败");
		let port = listener.local_addr().expect("取地址失败").port();
		drop(listener);

		let dialer = DirectDialer::new(Duration::from_secs(5));
		let error = dialer
			.connect(&target("::1", port))
			.await
			.expect_err("连空端口不应成功");

		assert!(
			matches!(error, NetError::Refused { .. }),
			"IPv6 路径也应能正确归类，实际：{error:?}"
		);
	}

	#[tokio::test]
	async fn unresolvable_host_is_classified() {
		// .invalid 是 RFC 2606 保留的顶级域，永远不会解析成功
		let dialer = DirectDialer::new(Duration::from_secs(5));
		let error = dialer
			.connect(&target("daisy-no-such-host.invalid", 22))
			.await
			.expect_err("不存在的域名不应连上");

		assert!(
			matches!(error, NetError::Resolve { .. }),
			"应归类为「解析失败」，实际：{error:?}"
		);
	}

	#[tokio::test]
	async fn slow_target_hits_timeout() {
		// 192.0.2.0/24 是 RFC 5737 保留的测试网段，永远不会被路由
		let dialer = DirectDialer::new(Duration::from_millis(300));
		let error = dialer
			.connect(&target("192.0.2.1", 22))
			.await
			.expect_err("测试网段不应连上");

		assert!(
			matches!(error, NetError::Timeout { .. }),
			"应归类为「超时」，实际：{error:?}"
		);
	}

	#[tokio::test]
	async fn dialer_can_be_used_as_trait_object() {
		let dialer: Box<dyn Dialer> = Box::new(DirectDialer::default());
		assert_eq!(dialer.describe(), "直连");

		let (port, server) = echo_listener_on("127.0.0.1:0", 1).await;
		let mut stream = dialer
			.dial(&target("127.0.0.1", port))
			.await
			.expect("拨号失败");
		stream.write_all(b"boxed").await.expect("写入失败");
		drop(stream);

		server.await.expect("服务端任务失败");
	}
}
