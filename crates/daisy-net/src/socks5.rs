//! SOCKS5 客户端（RFC 1928，含 RFC 1929 用户名/密码认证）。
//!
//! ## 握手三步
//!
//! ```text
//!   客户端                                    代理
//!   ──────                                    ────
//!   ① VER=5, NMETHODS, METHODS[]  ───────────▶
//!                                  ◀─────────  VER=5, METHOD
//!   ② 若 METHOD=0x02（用户密码）
//!      VER=1, ULEN, UNAME, PLEN, PASSWD ─────▶
//!                                  ◀─────────  VER=1, STATUS
//!   ③ VER=5, CMD=0x01, RSV, ATYP, DST.ADDR,
//!      DST.PORT                   ───────────▶
//!                                  ◀─────────  VER=5, REP, RSV, ATYP, BND.ADDR, BND.PORT
//! ```
//!
//! 三步都完成后，这条流就变成了一条到目标主机的透明字节管道。
//!
//! ## 假设：只做 CONNECT 命令
//!
//! SOCKS5 还有 BIND 与 UDP ASSOCIATE。Daisy 只需要「连到某台主机的某个端口」，
//! 也就是 CONNECT。**不做未使用的功能**——BIND 已基本被淘汰，
//! UDP 与 SSH/RDP 的语义也对不上。
//!
//! ## IPv6
//!
//! 目标地址按 [`HostKind`] 选 `ATYP`：IPv4 用 `0x01`、域名用 `0x03`、
//! **IPv6 用 `0x04`**（16 字节原始地址）。这是 G1「支持 IPv6」在代理路径上的落点。

use std::net::{Ipv4Addr, Ipv6Addr};

use daisy_core::HostKind;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::TargetAddr;
use crate::error::{NetError, Result};
use crate::tunnel::ProxyAuth;

/// 协议版本。
const VERSION: u8 = 0x05;

/// 用户名/密码子协商的版本（RFC 1929）。
const AUTH_VERSION: u8 = 0x01;

/// 不需要认证。
const METHOD_NO_AUTH: u8 = 0x00;
/// 用户名/密码认证。
const METHOD_USERNAME_PASSWORD: u8 = 0x02;
/// 服务端表示「你给的方式我都不支持」。
const METHOD_NONE_ACCEPTABLE: u8 = 0xFF;

/// CONNECT 命令。
const CMD_CONNECT: u8 = 0x01;

/// 地址类型。
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// 单个请求的最大字节数。域名最长 255，加上固定头足够。
const MAX_REQUEST: usize = 300;

/// 在已建立的流上完成 SOCKS5 握手，请求连接到 `target`。
///
/// `proxy_label` 仅用于错误文案（形如 `10.0.0.1:1080`），
/// 让用户一眼看出**是代理的问题还是目标的问题**。
pub async fn negotiate<S>(
	stream: &mut S,
	target: &TargetAddr,
	auth: Option<&ProxyAuth>,
	proxy_label: &str,
) -> Result<()>
where
	S: AsyncRead + AsyncWrite + Unpin,
{
	// 方法协商返回「服务端是否要求用户名/密码认证」。
	// 只有服务端真的选了用户名密码方式才发凭据——
	// 有些代理同时支持匿名与认证，选了匿名时不该白白把凭据送出去。
	let needs_auth = negotiate_method(stream, auth, proxy_label).await?;

	if needs_auth {
		// 走到这里说明 negotiate_method 已经确认凭据存在
		let auth = auth.expect("needs_auth 为真时凭据必已校验存在");
		authenticate(stream, auth, proxy_label).await?;
	}

	request_connect(stream, target, proxy_label).await
}

/// ① 方法协商。返回 `true` 表示服务端选择了用户名/密码认证，
/// 调用方需要接着做子协商；`false` 表示服务端选择了匿名。
async fn negotiate_method<S>(
	stream: &mut S,
	auth: Option<&ProxyAuth>,
	proxy_label: &str,
) -> Result<bool>
where
	S: AsyncRead + AsyncWrite + Unpin,
{
	let mut greeting = Vec::with_capacity(4);
	greeting.push(VERSION);
	if auth.is_some() {
		// 同时提供匿名与认证两种：能用匿名就连，
		// 代理要求认证时也能满足
		greeting.push(2);
		greeting.push(METHOD_NO_AUTH);
		greeting.push(METHOD_USERNAME_PASSWORD);
	} else {
		greeting.push(1);
		greeting.push(METHOD_NO_AUTH);
	}

	stream.write_all(&greeting).await.map_err(NetError::Io)?;
	stream.flush().await.map_err(NetError::Io)?;

	let mut reply = [0u8; 2];
	stream.read_exact(&mut reply).await.map_err(NetError::Io)?;

	if reply[0] != VERSION {
		return Err(NetError::ProxyProtocol {
			proxy: proxy_label.to_string(),
			reason: format!("协商响应版本不是 5（收到 {}）", reply[0]),
		});
	}

	match reply[1] {
		METHOD_NO_AUTH => Ok(false),
		METHOD_USERNAME_PASSWORD => {
			if auth.is_some() {
				Ok(true)
			} else {
				Err(NetError::ProxyAuthRejected {
					proxy: proxy_label.to_string(),
					reason: "代理要求用户名/密码认证，但没有配置凭据".to_string(),
				})
			}
		},
		METHOD_NONE_ACCEPTABLE => Err(NetError::ProxyAuthRejected {
			proxy: proxy_label.to_string(),
			reason: "代理不接受任何可用的认证方式".to_string(),
		}),
		other => Err(NetError::ProxyProtocol {
			proxy: proxy_label.to_string(),
			reason: format!("代理选择了未知的认证方式 0x{other:02X}"),
		}),
	}
}

/// ② 用户名/密码子协商（RFC 1929）。
async fn authenticate<S>(stream: &mut S, auth: &ProxyAuth, proxy_label: &str) -> Result<()>
where
	S: AsyncRead + AsyncWrite + Unpin,
{
	// 用户名与密码都是「长度前缀 + 内容」，长度上限 255
	let name = auth.username.as_bytes();
	let password = auth.password.as_bytes();
	if name.len() > 255 || password.len() > 255 {
		return Err(NetError::ProxyAuthRejected {
			proxy: proxy_label.to_string(),
			reason: "用户名或密码超过 SOCKS5 允许的 255 字节".to_string(),
		});
	}

	let mut request = Vec::with_capacity(3 + name.len() + password.len());
	request.push(AUTH_VERSION);
	request.push(name.len() as u8);
	request.extend_from_slice(name);
	request.push(password.len() as u8);
	request.extend_from_slice(password);

	stream.write_all(&request).await.map_err(NetError::Io)?;
	stream.flush().await.map_err(NetError::Io)?;

	let mut reply = [0u8; 2];
	stream.read_exact(&mut reply).await.map_err(NetError::Io)?;

	if reply[1] != 0x00 {
		return Err(NetError::ProxyAuthRejected {
			proxy: proxy_label.to_string(),
			reason: "代理拒绝了用户名或密码".to_string(),
		});
	}
	Ok(())
}

/// ③ 发 CONNECT 请求并读回应。
async fn request_connect<S>(stream: &mut S, target: &TargetAddr, proxy_label: &str) -> Result<()>
where
	S: AsyncRead + AsyncWrite + Unpin,
{
	let mut request = Vec::with_capacity(MAX_REQUEST);
	request.push(VERSION);
	request.push(CMD_CONNECT);
	request.push(0x00); // RSV

	// 地址按类型编码。域名不解析——**交给代理去解析**，
	// 这样内网域名在代理那一侧才能正确解析（本地解析往往是错的）
	match target.host_kind() {
		HostKind::V4 => {
			let addr: Ipv4Addr = target.host().parse().map_err(|_| NetError::ProxyProtocol {
				proxy: proxy_label.to_string(),
				reason: format!("IPv4 字面量无法解析：{}", target.host()),
			})?;
			request.push(ATYP_IPV4);
			request.extend_from_slice(&addr.octets());
		},
		HostKind::V6 => {
			let addr: Ipv6Addr = target.host().parse().map_err(|_| NetError::ProxyProtocol {
				proxy: proxy_label.to_string(),
				reason: format!("IPv6 字面量无法解析：{}", target.host()),
			})?;
			request.push(ATYP_IPV6);
			request.extend_from_slice(&addr.octets());
		},
		HostKind::Domain => {
			let host = target.host().as_bytes();
			if host.len() > 255 {
				return Err(NetError::ProxyProtocol {
					proxy: proxy_label.to_string(),
					reason: "域名超过 SOCKS5 允许的 255 字节".to_string(),
				});
			}
			request.push(ATYP_DOMAIN);
			request.push(host.len() as u8);
			request.extend_from_slice(host);
		},
	}

	request.extend_from_slice(&target.port().to_be_bytes());

	stream.write_all(&request).await.map_err(NetError::Io)?;
	stream.flush().await.map_err(NetError::Io)?;

	// 回应：VER, REP, RSV, ATYP, BND.ADDR, BND.PORT
	let mut header = [0u8; 4];
	stream.read_exact(&mut header).await.map_err(NetError::Io)?;

	if header[0] != VERSION {
		return Err(NetError::ProxyProtocol {
			proxy: proxy_label.to_string(),
			reason: format!("CONNECT 响应版本不是 5（收到 {}）", header[0]),
		});
	}

	// **必须把地址与端口读完**，否则残留字节会污染之后的数据流
	let read_length = match header[3] {
		ATYP_IPV4 => 4,
		ATYP_IPV6 => 16,
		ATYP_DOMAIN => {
			let mut length = [0u8; 1];
			stream.read_exact(&mut length).await.map_err(NetError::Io)?;
			length[0] as usize
		},
		other => {
			return Err(NetError::ProxyProtocol {
				proxy: proxy_label.to_string(),
				reason: format!("CONNECT 响应里有未知的地址类型 0x{other:02X}"),
			});
		},
	};
	let mut discard = vec![0u8; read_length + 2];
	stream
		.read_exact(&mut discard)
		.await
		.map_err(NetError::Io)?;

	if header[1] != 0x00 {
		return Err(NetError::ProxyRefused {
			proxy: proxy_label.to_string(),
			target: target.display(),
			reason: reply_message(header[1]).to_string(),
		});
	}

	tracing::debug!("SOCKS5 隧道已建立：{proxy_label} → {}", target.display());
	Ok(())
}

/// 把 SOCKS5 的 REP 码翻译成用户能看懂的原因。
///
/// 这张表是错误提示质量的关键——「0x05」对用户毫无意义，
/// 「目标端口未监听」才能让人立刻知道该去检查什么。
fn reply_message(code: u8) -> &'static str {
	match code {
		0x01 => "代理返回一般性失败",
		0x02 => "代理的访问控制规则不允许连到该目标",
		0x03 => "代理无法到达目标所在网络",
		0x04 => "代理无法到达目标主机（可能域名解析失败）",
		0x05 => "目标端口未监听（连接被拒绝）",
		0x06 => "连接已超时（TTL 耗尽）",
		0x07 => "代理不支持 CONNECT 命令",
		0x08 => "代理不支持该地址类型（旧代理可能不支持 IPv6）",
		_ => "代理返回未知错误码",
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use daisy_core::HostAddr;
	use tokio::net::TcpStream;

	use crate::test_stream::MemStream;

	fn target(host: &str, port: u16) -> TargetAddr {
		TargetAddr::new(HostAddr::parse(host).expect("测试地址应合法"), port)
	}

	fn label() -> String {
		"127.0.0.1:1080".to_string()
	}

	/// 造一个「预置响应 + 独立写缓冲」的双向流。
	///
	/// 不能拿 `Cursor` 直接当流——SOCKS5 握手是「写一段读一段」，
	/// 写会覆盖预置的响应字节。这里读走响应、写进独立缓冲，互不干扰。
	fn stream_with(response: &[u8]) -> MemStream {
		MemStream::new(response.to_vec())
	}

	#[tokio::test]
	async fn greeting_offers_no_auth_when_credentials_absent() {
		// 服务端回「不需要认证」
		let mut stream = stream_with(&[0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
		negotiate(&mut stream, &target("10.0.0.1", 22), None, &label())
			.await
			.expect("应成功");

		let written = stream.into_written();
		assert_eq!(
			&written[..3],
			&[0x05, 0x01, 0x00],
			"没凭据时只应提供「不需要认证」一种方式"
		);
	}

	#[tokio::test]
	async fn greeting_offers_both_methods_when_credentials_present() {
		let auth = ProxyAuth::new("user", "pass");
		// 服务端选「不需要认证」，于是不该再发凭据
		let mut stream = stream_with(&[0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
		negotiate(&mut stream, &target("10.0.0.1", 22), Some(&auth), &label())
			.await
			.expect("应成功");

		let written = stream.into_written();
		assert_eq!(
			&written[..4],
			&[0x05, 0x02, 0x00, 0x02],
			"应同时提供两种方式"
		);
		// 服务端选了匿名，凭据就不该出现
		assert!(
			!written.windows(4).any(|w| w == b"user"),
			"服务端选择匿名时不应发送凭据"
		);
	}

	#[tokio::test]
	async fn sends_credentials_when_the_proxy_demands_them() {
		let auth = ProxyAuth::new("user", "pass");
		// 方法协商选 0x02，认证成功，CONNECT 成功
		let mut stream = stream_with(&[
			0x05, 0x02, // 方法协商：要求用户名密码
			0x01, 0x00, // 认证：成功
			0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0, // CONNECT：成功
		]);
		negotiate(&mut stream, &target("10.0.0.1", 22), Some(&auth), &label())
			.await
			.expect("应成功");

		let written = stream.into_written();
		let text = String::from_utf8_lossy(&written);
		assert!(text.contains("user"), "应发送用户名：{text:?}");
		assert!(text.contains("pass"), "应发送密码：{text:?}");
	}

	#[tokio::test]
	async fn auth_rejection_is_reported() {
		let auth = ProxyAuth::new("user", "wrong");
		let mut stream = stream_with(&[0x05, 0x02, 0x01, 0x01]);
		let error = negotiate(&mut stream, &target("10.0.0.1", 22), Some(&auth), &label())
			.await
			.expect_err("认证失败不应成功");

		match error {
			NetError::ProxyAuthRejected { proxy, reason } => {
				assert_eq!(proxy, "127.0.0.1:1080");
				assert!(reason.contains("拒绝"), "原因应可读：{reason}");
			},
			other => panic!("应为 ProxyAuthRejected，实际 {other:?}"),
		}
	}

	#[tokio::test]
	async fn proxy_demanding_auth_without_credentials_is_reported() {
		let mut stream = stream_with(&[0x05, 0x02]);
		let error = negotiate(&mut stream, &target("10.0.0.1", 22), None, &label())
			.await
			.expect_err("代理要求认证时不应成功");

		match error {
			NetError::ProxyAuthRejected { reason, .. } => {
				assert!(reason.contains("没有配置凭据"), "应点明缺凭据：{reason}");
			},
			other => panic!("应为 ProxyAuthRejected，实际 {other:?}"),
		}
	}

	#[tokio::test]
	async fn no_acceptable_method_is_reported() {
		let mut stream = stream_with(&[0x05, 0xFF]);
		let error = negotiate(&mut stream, &target("10.0.0.1", 22), None, &label())
			.await
			.expect_err("无可用方式时不应成功");

		assert!(
			matches!(error, NetError::ProxyAuthRejected { .. }),
			"应为 ProxyAuthRejected，实际 {error:?}"
		);
	}

	#[tokio::test]
	async fn encodes_ipv4_target_as_atyp_1() {
		let mut stream = stream_with(&[0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
		negotiate(&mut stream, &target("10.1.2.3", 2222), None, &label())
			.await
			.expect("应成功");

		let written = stream.into_written();
		let request = &written[3..];
		assert_eq!(request[0..3], [0x05, 0x01, 0x00], "VER/CMD/RSV");
		assert_eq!(request[3], ATYP_IPV4);
		assert_eq!(&request[4..8], &[10, 1, 2, 3], "IPv4 应为 4 字节原始地址");
		assert_eq!(&request[8..10], &2222u16.to_be_bytes(), "端口应是大端");
	}

	#[tokio::test]
	async fn encodes_domain_target_as_atyp_3() {
		let mut stream = stream_with(&[0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
		negotiate(
			&mut stream,
			&target("win.example.com", 3389),
			None,
			&label(),
		)
		.await
		.expect("应成功");

		let written = stream.into_written();
		let request = &written[3..];
		assert_eq!(request[3], ATYP_DOMAIN);
		assert_eq!(request[4], 15, "域名长度应为 15");
		assert_eq!(&request[5..20], b"win.example.com");
		assert_eq!(&request[20..22], &3389u16.to_be_bytes());

		// 关键：域名按原样发给代理，**不做本地 DNS 解析**——
		// 内网域名只有代理那侧才解析得对
		assert!(
			!String::from_utf8_lossy(&written).is_empty(),
			"域名应作为文本出现在请求里"
		);
	}

	#[tokio::test]
	async fn encodes_ipv6_target_as_atyp_4() {
		let mut stream = stream_with(&[0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
		negotiate(&mut stream, &target("2001:db8::1", 22), None, &label())
			.await
			.expect("应成功");

		let written = stream.into_written();
		let request = &written[3..];
		assert_eq!(request[3], ATYP_IPV6, "IPv6 必须用 ATYP=0x04");
		let expected: Ipv6Addr = "2001:db8::1".parse().expect("应能解析");
		assert_eq!(&request[4..20], &expected.octets(), "IPv6 应为 16 字节");
		assert_eq!(&request[20..22], &22u16.to_be_bytes());
	}

	#[tokio::test]
	async fn reads_the_full_reply_before_handing_over() {
		// 回应里带一个域名形式的 BND.ADDR，长度可变。
		// 若没读干净，残留字节会污染隧道——这里用一个显式构造的回应验证。
		let mut response = vec![0x05, 0x00, 0x00, ATYP_DOMAIN, 7];
		response.extend_from_slice(b"bnd.hst");
		response.extend_from_slice(&1080u16.to_be_bytes());
		response.extend_from_slice(b"payload-after-handshake");

		// 方法协商阶段还要先回一段「不需要认证」
		let mut full = vec![0x05, 0x00];
		full.extend_from_slice(&response);
		let mut stream = stream_with(&full);

		negotiate(&mut stream, &target("10.0.0.1", 22), None, &label())
			.await
			.expect("应成功");

		let mut rest = Vec::new();
		stream.read_to_end(&mut rest).await.expect("读取失败");
		assert_eq!(
			rest, b"payload-after-handshake",
			"握手结束后流里应只剩真正的数据"
		);
	}

	#[tokio::test]
	async fn refusal_codes_are_translated() {
		for (code, expect) in [
			(0x02u8, "访问控制"),
			(0x03, "网络"),
			(0x04, "主机"),
			(0x05, "端口未监听"),
			(0x08, "IPv6"),
		] {
			let mut response = vec![0x05, 0x00, 0x05, code, 0x00, ATYP_IPV4];
			response.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
			let mut stream = stream_with(&response);

			let error = negotiate(&mut stream, &target("10.0.0.1", 22), None, &label())
				.await
				.expect_err("非零 REP 不应成功");

			match error {
				NetError::ProxyRefused { reason, target, .. } => {
					assert!(
						reason.contains(expect),
						"REP=0x{code:02X} 的文案应含「{expect}」，实际：{reason}"
					);
					assert_eq!(target, "10.0.0.1:22");
				},
				other => panic!("应为 ProxyRefused，实际 {other:?}"),
			}
		}
	}

	#[tokio::test]
	async fn wrong_version_is_reported() {
		let mut stream = stream_with(&[0x04, 0x00]);
		let error = negotiate(&mut stream, &target("10.0.0.1", 22), None, &label())
			.await
			.expect_err("版本不对不应成功");

		match error {
			NetError::ProxyProtocol { reason, proxy } => {
				assert!(reason.contains("版本"), "应指出是版本问题：{reason}");
				assert_eq!(proxy, "127.0.0.1:1080");
			},
			other => panic!("应为 ProxyProtocol，实际 {other:?}"),
		}
	}

	#[test]
	fn reply_messages_cover_every_defined_code() {
		// 八个定义过的码都不能落到「未知」分支
		for code in 1u8..=8 {
			let message = reply_message(code);
			assert_ne!(
				message, "代理返回未知错误码",
				"REP=0x{code:02X} 应有专门的文案"
			);
		}
		assert_eq!(reply_message(0x09), "代理返回未知错误码");
	}

	/// 一个能跑的 SOCKS5 服务端，用来做**真实套接字**上的握手验证
	/// （上面的用例都跑在内存 `Cursor` 上，覆盖不到分片读写）。
	mod live {
		use super::*;
		use tokio::net::TcpListener;

		/// 起一个只做握手就结束的 SOCKS5 服务端，返回端口与收到的请求。
		pub async fn start(replies: Vec<Vec<u8>>) -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
			let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定失败");
			let port = listener.local_addr().expect("取地址失败").port();

			let handle = tokio::spawn(async move {
				let (mut socket, _) = listener.accept().await.expect("accept 失败");
				let mut seen = Vec::new();
				let mut buffer = vec![0u8; 512];

				for reply in replies {
					// 先读一段（客户端写过来的），再回一段
					let n = socket.read(&mut buffer).await.expect("读取失败");
					seen.extend_from_slice(&buffer[..n]);
					socket.write_all(&reply).await.expect("写入失败");
				}
				seen
			});

			(port, handle)
		}
	}

	#[tokio::test]
	async fn handshake_over_a_real_socket() {
		// 两个阶段：方法协商 → CONNECT 回应
		let (port, server) = live::start(vec![
			vec![0x05, 0x00],
			vec![0x05, 0x00, 0x00, ATYP_IPV4, 127, 0, 0, 1, 0x04, 0x38],
		])
		.await;

		let mut socket = TcpStream::connect(("127.0.0.1", port))
			.await
			.expect("连接失败");

		negotiate(
			&mut socket,
			&target("example.com", 22),
			None,
			&format!("127.0.0.1:{port}"),
		)
		.await
		.expect("真实套接字上的握手应成功");

		let seen = server.await.expect("服务端任务失败");
		// 第一段是方法协商，第二段是 CONNECT 请求
		assert_eq!(&seen[..3], &[0x05, 0x01, 0x00]);

		let request = &seen[3..];
		assert_eq!(&request[..3], &[0x05, 0x01, 0x00]);
		assert_eq!(request[3], ATYP_DOMAIN);
		assert_eq!(request[4], 11, "域名长度应为 11");
		assert_eq!(&request[5..16], b"example.com");
	}
}
