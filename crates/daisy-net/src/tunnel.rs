//! 统一的代理隧道抽象。
//!
//! 对应 STEPS.md 步骤 53。
//!
//! ## 为什么用「套一层 Dialer」而不是独立的 Tunnel 类型
//!
//! S6 已经把「怎么到达目标」抽象成了 [`Dialer`]。代理要做的只是**换个到达方式**，
//! 所以最自然的表达就是再实现一个 [`Dialer`]：内部持有另一个 `Dialer` 去连代理，
//! 握手之后把同一条流转交给上层。
//!
//! ```text
//!   ProxyDialer ──inner──▶ DirectDialer ──▶ 代理地址
//!        │
//!        └── SOCKS5 / HTTP CONNECT 握手，请求连到目标 ──▶ 透明字节管道
//! ```
//!
//! 三个好处：
//!
//! 1. SSH 与 RDP **都不用改**——它们本来就是收 `Box<dyn Dialer>` 的
//! 2. 「代理的代理」不需要额外代码（`inner` 换成另一个 `ProxyDialer` 即可）
//! 3. 认证、超时、错误分类只在这一层出现一次
//!
//! ## 凭据
//!
//! 代理密码用 [`Zeroizing`] 包裹，离开作用域即清零；
//! 并且 [`ProxyAuth`] 的 `Debug` 输出经过脱敏，不会因日志泄漏。

use std::fmt;
use std::time::Duration;

use daisy_core::HostAddr;
use daisy_core::model::ProxyKind;
use zeroize::Zeroizing;

use crate::dialer::{BoxStream, Dialer, TargetAddr};
use crate::error::{NetError, Result};
use crate::{http_proxy, socks5};

/// 代理握手阶段的默认超时。
///
/// 与「连到代理」的超时分开：连上代理并不代表握手会顺利，
/// 一个坏掉的代理可能建立 TCP 之后就不吭声了。
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// 代理认证凭据。
pub struct ProxyAuth {
	/// 用户名。
	pub username: String,
	/// 密码。离开作用域时自动清零。
	pub password: Zeroizing<String>,
}

impl ProxyAuth {
	/// 构造。
	#[must_use]
	pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
		Self {
			username: username.into(),
			password: Zeroizing::new(password.into()),
		}
	}
}

impl fmt::Debug for ProxyAuth {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		// 密码绝不进日志
		f.debug_struct("ProxyAuth")
			.field("username", &self.username)
			.field("password", &"<已脱敏>")
			.finish()
	}
}

/// 一个已校验的代理端点。
///
/// 构造即校验，因此内部不变量（主机已规范化、端口非 0）始终成立。
pub struct ProxyEndpoint {
	kind: ProxyKind,
	host: HostAddr,
	port: u16,
	auth: Option<ProxyAuth>,
}

impl fmt::Debug for ProxyEndpoint {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		// 用 label() 而不是派生的字段展开：这样日志里是
		// `socks5://10.0.0.1:1080` 而不是散落的 host/port/kind
		f.write_str(&self.label())
	}
}

impl ProxyEndpoint {
	/// 构造并校验。
	///
	/// 代理地址同样支持域名 / IPv4 / IPv6（与目标地址一套规则）。
	pub fn new(kind: ProxyKind, host: &str, port: u16) -> Result<Self> {
		if port == 0 {
			return Err(NetError::InvalidTarget("代理端口不能为 0".to_string()));
		}
		Ok(Self {
			kind,
			host: HostAddr::parse(host)
				.map_err(|error| NetError::InvalidTarget(format!("代理地址不合法：{error}")))?,
			port,
			auth: None,
		})
	}

	/// 附上认证凭据。
	#[must_use]
	pub fn with_auth(mut self, auth: ProxyAuth) -> Self {
		self.auth = Some(auth);
		self
	}

	/// 协议类型。
	#[must_use]
	pub fn kind(&self) -> ProxyKind {
		self.kind
	}

	/// 代理主机（IPv6 不带方括号）。
	#[must_use]
	pub fn host(&self) -> &str {
		self.host.host()
	}

	/// 代理端口。
	#[must_use]
	pub fn port(&self) -> u16 {
		self.port
	}

	/// 认证凭据。
	#[must_use]
	pub fn auth(&self) -> Option<&ProxyAuth> {
		self.auth.as_ref()
	}

	/// `socks5://10.0.0.1:1080` 形式的标签，用于日志与错误文案。
	#[must_use]
	pub fn label(&self) -> String {
		format!(
			"{}://{}:{}",
			self.kind.as_str(),
			self.host.display(),
			self.port
		)
	}

	/// 拨号到代理本身所需的目标地址。
	#[must_use]
	pub fn target(&self) -> TargetAddr {
		TargetAddr::new(self.host.clone(), self.port)
	}
}

/// 经代理转发的拨号器。
pub struct ProxyDialer {
	endpoint: ProxyEndpoint,
	/// 怎么到达代理本身。通常是一个 [`crate::DirectDialer`]。
	inner: Box<dyn Dialer>,
	handshake_timeout: Duration,
}

impl ProxyDialer {
	/// 用指定的内层拨号器构造。
	#[must_use]
	pub fn new(endpoint: ProxyEndpoint, inner: Box<dyn Dialer>) -> Self {
		Self {
			endpoint,
			inner,
			handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
		}
	}

	/// 代理端点。
	#[must_use]
	pub fn endpoint(&self) -> &ProxyEndpoint {
		&self.endpoint
	}
}

impl Dialer for ProxyDialer {
	fn dial<'a>(
		&'a self,
		target: &'a TargetAddr,
	) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<BoxStream>> + Send + 'a>> {
		Box::pin(async move {
			let label = self.endpoint.label();

			// 第一步：连到代理。失败要明确说是「代理连不上」，
			// 而不是笼统的「连接失败」——排查方向完全不同。
			let mut stream = self
				.inner
				.dial(&self.endpoint.target())
				.await
				.map_err(|source| NetError::ProxyConnect {
					// 用 TargetAddr 的展示形式：IPv6 会加方括号，
					// 否则 `::1:1080` 分不清哪段是端口
					proxy: self.endpoint.target().display(),
					reason: source.short_reason(),
				})?;

			// 第二步：握手。单独设超时——TCP 建立成功不代表代理会正常应答。
			let handshake = async {
				match self.endpoint.kind() {
					ProxyKind::Socks5 => {
						socks5::negotiate(&mut stream, target, self.endpoint.auth(), &label).await
					},
					ProxyKind::Http => {
						http_proxy::negotiate(&mut stream, target, self.endpoint.auth(), &label)
							.await
					},
				}
			};

			match tokio::time::timeout(self.handshake_timeout, handshake).await {
				Ok(result) => result?,
				Err(_) => {
					return Err(NetError::ProxyProtocol {
						proxy: label,
						reason: format!(
							"代理在 {} 秒内没有完成握手",
							self.handshake_timeout.as_secs()
						),
					});
				},
			}

			tracing::info!("经代理建立隧道：{label} → {}", target.display());
			Ok(stream)
		})
	}

	fn describe(&self) -> String {
		format!("经 {}", self.endpoint.label())
	}
}

impl fmt::Debug for ProxyDialer {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("ProxyDialer")
			.field("endpoint", &self.endpoint)
			.field("inner", &self.inner.describe())
			.field("handshake_timeout", &self.handshake_timeout)
			.finish()
	}
}

/// 按配置构造拨号器。
///
/// - 没有代理 → 直连
/// - 有代理 → 直连套一层 [`ProxyDialer`]
///
/// SSH 与 RDP 都从这里拿 `Box<dyn Dialer>`，因此「有没有代理」对它们是透明的。
pub fn build_dialer(proxy: Option<ProxyEndpoint>, connect_timeout: Duration) -> Box<dyn Dialer> {
	let inner: Box<dyn Dialer> = Box::new(crate::DirectDialer::new(connect_timeout));
	match proxy {
		Some(endpoint) => Box::new(ProxyDialer::new(endpoint, inner)),
		None => inner,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn endpoint_validates_input() {
		assert!(
			ProxyEndpoint::new(ProxyKind::Socks5, "10.0.0.1", 1080).is_ok(),
			"合法代理应通过"
		);
		assert!(
			ProxyEndpoint::new(ProxyKind::Socks5, "", 1080).is_err(),
			"空主机应被拒绝"
		);
		assert!(
			ProxyEndpoint::new(ProxyKind::Socks5, "10.0.0.1", 0).is_err(),
			"0 端口应被拒绝"
		);
		assert!(
			ProxyEndpoint::new(ProxyKind::Http, "1.2.3.999", 8080).is_err(),
			"畸形 IP 应被拒绝"
		);
	}

	#[test]
	fn endpoint_normalizes_ipv6() {
		let bare = ProxyEndpoint::new(ProxyKind::Socks5, "::1", 1080).expect("应通过");
		let bracketed = ProxyEndpoint::new(ProxyKind::Socks5, "[::1]", 1080).expect("应通过");

		assert_eq!(bare.host(), "::1", "交给底层的不带方括号");
		assert_eq!(bracketed.host(), "::1");
		assert_eq!(
			bare.label(),
			"socks5://[::1]:1080",
			"标签里加回方括号便于阅读"
		);
	}

	#[test]
	fn label_names_the_protocol() {
		let socks = ProxyEndpoint::new(ProxyKind::Socks5, "10.0.0.1", 1080).expect("应通过");
		assert_eq!(socks.label(), "socks5://10.0.0.1:1080");

		let http = ProxyEndpoint::new(ProxyKind::Http, "proxy.corp", 8080).expect("应通过");
		assert_eq!(http.label(), "http://proxy.corp:8080");
	}

	#[test]
	fn target_reaches_the_proxy_itself() {
		let endpoint = ProxyEndpoint::new(ProxyKind::Socks5, "10.0.0.1", 1080).expect("应通过");
		let target = endpoint.target();
		assert_eq!(target.host(), "10.0.0.1");
		assert_eq!(target.port(), 1080);
	}

	#[test]
	fn auth_debug_never_prints_the_password() {
		let auth = ProxyAuth::new("alice", "s3cret-proxy-password");
		let text = format!("{auth:?}");

		assert!(text.contains("alice"), "用户名可以出现：{text}");
		assert!(
			!text.contains("s3cret-proxy-password"),
			"密码不应出现：{text}"
		);
		assert!(text.contains("脱敏"), "应有脱敏标记：{text}");
	}

	#[test]
	fn dialer_debug_never_prints_the_password() {
		let endpoint = ProxyEndpoint::new(ProxyKind::Socks5, "10.0.0.1", 1080)
			.expect("应通过")
			.with_auth(ProxyAuth::new("alice", "top-secret"));
		let dialer = ProxyDialer::new(endpoint, Box::new(crate::DirectDialer::default()));

		let text = format!("{dialer:?}");
		assert!(!text.contains("top-secret"), "不应含密码：{text}");
		assert!(
			text.contains("socks5://10.0.0.1:1080"),
			"应含代理标签：{text}"
		);
	}

	#[test]
	fn describe_reflects_the_proxy() {
		let endpoint = ProxyEndpoint::new(ProxyKind::Http, "proxy.corp", 8080).expect("应通过");
		let dialer = ProxyDialer::new(endpoint, Box::new(crate::DirectDialer::default()));
		assert_eq!(dialer.describe(), "经 http://proxy.corp:8080");
	}

	#[test]
	fn build_dialer_without_proxy_is_direct() {
		let dialer = build_dialer(None, Duration::from_secs(5));
		assert_eq!(dialer.describe(), "直连");
	}

	#[test]
	fn build_dialer_with_proxy_wraps() {
		let endpoint = ProxyEndpoint::new(ProxyKind::Socks5, "10.0.0.1", 1080).expect("应通过");
		let dialer = build_dialer(Some(endpoint), Duration::from_secs(5));
		assert!(
			dialer.describe().contains("socks5://10.0.0.1:1080"),
			"应说明走哪个代理：{}",
			dialer.describe()
		);
	}

	#[tokio::test]
	async fn unreachable_proxy_is_reported_as_a_proxy_problem() {
		// 绑一个端口再释放，拿到一个没人听的端口
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
			.await
			.expect("绑定失败");
		let port = listener.local_addr().expect("取地址失败").port();
		drop(listener);

		let endpoint = ProxyEndpoint::new(ProxyKind::Socks5, "127.0.0.1", port).expect("应通过");
		let dialer = ProxyDialer::new(
			endpoint,
			Box::new(crate::DirectDialer::new(Duration::from_secs(3))),
		);

		let target = TargetAddr::new(HostAddr::parse("example.com").expect("应通过"), 22);
		// `dial` 的 Ok 值是 `Box<dyn AsyncStream>`，不实现 Debug，
		// 所以不能用 expect_err，这里手动拆开
		let error = match dialer.dial(&target).await {
			Ok(_) => panic!("连不上代理时不应成功"),
			Err(error) => error,
		};

		match error {
			NetError::ProxyConnect { proxy, reason } => {
				assert!(proxy.contains("127.0.0.1"), "应指出代理地址：{proxy}");
				assert!(reason.contains("拒绝"), "应说明原因：{reason}");
			},
			other => panic!("应为 ProxyConnect，实际 {other:?}"),
		}
	}
}
