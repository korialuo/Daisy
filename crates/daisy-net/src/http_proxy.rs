//! HTTP CONNECT 客户端（含 `Proxy-Authorization: Basic`）。
//!
//! 对应 STEPS.md 步骤 52。
//!
//! ## 协议
//!
//! ```text
//!   CONNECT 10.0.0.9:3389 HTTP/1.1\r\n
//!   Host: 10.0.0.9:3389\r\n
//!   Proxy-Authorization: Basic <base64(user:pass)>\r\n
//!   \r\n
//!   ─────────────────────────────────────▶
//!                          ◀──────────────  HTTP/1.1 200 Connection established\r\n\r\n
//! ```
//!
//! 收到 `2xx` 之后，这条连接就变成了到目标的透明字节管道。
//!
//! ## 为什么要逐字节读响应头
//!
//! CONNECT 的响应头与隧道数据**共用一条连接且没有长度前缀**。若一次读一大块，
//! 越过 `\r\n\r\n` 的字节就是隧道的第一段数据——而我们返回的是原始流，
//! 没有地方把多读的字节还回去，那些数据就永久丢失了。
//!
//! 逐字节读虽然朴素，但头只有一两百字节，代价可以忽略，正确性却是硬保证。
//!
//! ## IPv6
//!
//! authority 形式按 RFC 3986 写成 `[2001:db8::1]:3389`——**这里需要方括号**，
//! 与 SOCKS5 的二进制编码方式不同。是 G1「支持 IPv6」在 HTTP 代理路径上的落点。

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::TargetAddr;
use crate::error::{NetError, Result};
use crate::tunnel::ProxyAuth;

/// 响应头长度上限。正常的 CONNECT 响应不到 200 字节；
/// 超过这个数说明对端根本不是 HTTP 代理，早点报错好过一直读。
const MAX_HEADER_BYTES: usize = 8192;

/// 在已建立的流上完成 HTTP CONNECT 握手。
///
/// `proxy_label` 仅用于错误文案（形如 `http://10.0.0.1:8080`）。
pub async fn negotiate<S>(
	stream: &mut S,
	target: &TargetAddr,
	auth: Option<&ProxyAuth>,
	proxy_label: &str,
) -> Result<()>
where
	S: AsyncRead + AsyncWrite + Unpin,
{
	let authority = authority_of(target);
	let mut request = String::with_capacity(160);

	request.push_str(&format!("CONNECT {authority} HTTP/1.1\r\n"));
	// Host 与请求行里的 authority 必须一致，某些代理会校验
	request.push_str(&format!("Host: {authority}\r\n"));
	// Daisy 不代理 HTTP 正文，只做隧道；这个头能避免中间设备做内容嗅探
	request.push_str("Proxy-Connection: keep-alive\r\n");
	if let Some(auth) = auth {
		let token =
			base64_encode(format!("{}:{}", auth.username, auth.password.as_str()).as_bytes());
		request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
	}
	request.push_str("User-Agent: Daisy\r\n");
	request.push_str("\r\n");

	stream
		.write_all(request.as_bytes())
		.await
		.map_err(NetError::Io)?;
	stream.flush().await.map_err(NetError::Io)?;

	let head = read_head(stream).await?;
	let status = parse_status(&head).ok_or_else(|| NetError::ProxyProtocol {
		proxy: proxy_label.to_string(),
		reason: format!("响应不是合法的 HTTP 状态行：{}", first_line_preview(&head)),
	})?;

	match status {
		200..=299 => {
			tracing::debug!("HTTP CONNECT 隧道已建立：{proxy_label} → {authority}");
			Ok(())
		},
		407 => Err(NetError::ProxyAuthRejected {
			proxy: proxy_label.to_string(),
			reason: if auth.is_some() {
				"代理拒绝了 Basic 凭据（用户名或密码不对）".to_string()
			} else {
				"代理要求认证，但没有配置凭据".to_string()
			},
		}),
		_ => Err(NetError::ProxyRefused {
			proxy: proxy_label.to_string(),
			target: target.display(),
			reason: status_message(status),
		}),
	}
}

/// 拼出 CONNECT 请求行 / Host 头里的 authority。
///
/// IPv6 **必须**加方括号：`[::1]:22`。这是 RFC 3986 的写法，
/// 也是唯一能和无方括号的 `host:port` 区分开的写法。
fn authority_of(target: &TargetAddr) -> String {
	format!("{}:{}", bracketed_host(target), target.port())
}

fn bracketed_host(target: &TargetAddr) -> String {
	match target.host_kind() {
		daisy_core::HostKind::V6 => format!("[{}]", target.host()),
		_ => target.host().to_string(),
	}
}

/// 逐字节读响应头，直到 `\r\n\r\n`。
async fn read_head<S>(stream: &mut S) -> Result<Vec<u8>>
where
	S: AsyncRead + Unpin,
{
	let mut head = Vec::with_capacity(256);
	let mut byte = [0u8; 1];

	loop {
		let read = stream.read(&mut byte).await.map_err(NetError::Io)?;
		if read == 0 {
			return Err(NetError::ProxyProtocol {
				proxy: "代理".to_string(),
				reason: format!("读响应头时连接就断了（已读 {} 字节）", head.len()),
			});
		}
		head.push(byte[0]);

		// 只认 \r\n\r\n；有些实现会只发 \n\n，一并容忍
		if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
			return Ok(head);
		}
		if head.len() > MAX_HEADER_BYTES {
			return Err(NetError::ProxyProtocol {
				proxy: "代理".to_string(),
				reason: format!("响应头超过 {MAX_HEADER_BYTES} 字节，这不像是 HTTP 代理"),
			});
		}
	}
}

/// 从状态行里取出状态码。
fn parse_status(head: &[u8]) -> Option<u16> {
	let text = String::from_utf8_lossy(head);
	let first = text.lines().next()?;
	let mut parts = first.split_whitespace();
	let _version = parts.next()?;
	let code = parts.next()?;
	code.parse().ok()
}

fn first_line_preview(head: &[u8]) -> String {
	let text = String::from_utf8_lossy(head);
	let line = text.lines().next().unwrap_or("(空响应)");
	// 只回显前 80 个字符，避免把一大段垃圾刷进错误信息
	let preview: String = line.chars().take(80).collect();
	format!("{preview:?}")
}

/// 把常见的失败状态码翻译成可操作的原因。
fn status_message(status: u16) -> String {
	match status {
		400 => "代理认为 CONNECT 请求格式有误".to_string(),
		403 => "代理的访问控制规则不允许连到该目标".to_string(),
		405 => "代理不支持 CONNECT 方法".to_string(),
		408 => "代理等待超时".to_string(),
		502 => "代理无法连接目标（可能目标主机名解析失败或端口未监听）".to_string(),
		503 => "代理暂时不可用".to_string(),
		504 => "代理连接目标超时（目标可能被防火墙丢包）".to_string(),
		other => format!("代理返回状态码 {other}"),
	}
}

/// 标准 Base64 编码（RFC 4648，含 `=` 填充）。
///
/// 只为 Basic 认证而写，**只需要编码**（认证头是单向的，不解析回来的）。
/// 与其为一个 20 行的纯函数引入依赖，不如自己写并用已知向量钉死——
/// 这里不是密码学，不存在「不要自己造」的问题。
fn base64_encode(input: &[u8]) -> String {
	const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

	let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
	for chunk in input.chunks(3) {
		let b0 = chunk[0] as u32;
		let b1 = *chunk.get(1).unwrap_or(&0) as u32;
		let b2 = *chunk.get(2).unwrap_or(&0) as u32;
		let triple = (b0 << 16) | (b1 << 8) | b2;

		out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
		out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
		if chunk.len() > 1 {
			out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
		} else {
			out.push('=');
		}
		if chunk.len() > 2 {
			out.push(TABLE[(triple & 0x3F) as usize] as char);
		} else {
			out.push('=');
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use daisy_core::HostAddr;
	use tokio::io::AsyncReadExt;

	use crate::test_stream::MemStream;

	fn target(host: &str, port: u16) -> TargetAddr {
		TargetAddr::new(HostAddr::parse(host).expect("测试地址应合法"), port)
	}

	fn label() -> String {
		"http://127.0.0.1:8080".to_string()
	}

	/// 造一个「预置响应 + 独立写缓冲」的双向流。
	///
	/// 不能拿 `Cursor` 直接当流——CONNECT 握手是「写一段读一段」，
	/// 写会覆盖预置的响应字节。这里读走响应、写进独立缓冲，互不干扰。
	fn stream_with(response: &[u8]) -> MemStream {
		MemStream::new(response.to_vec())
	}

	// ---------- Base64 ----------

	#[test]
	fn base64_matches_rfc4648_vectors() {
		// RFC 4648 §10 的官方测试向量
		for (input, expect) in [
			(&b""[..], ""),
			(&b"f"[..], "Zg=="),
			(&b"fo"[..], "Zm8="),
			(&b"foo"[..], "Zm9v"),
			(&b"foob"[..], "Zm9vYg=="),
			(&b"fooba"[..], "Zm9vYmE="),
			(&b"foobar"[..], "Zm9vYmFy"),
		] {
			assert_eq!(
				base64_encode(input),
				expect,
				"{:?} 的编码结果不符",
				String::from_utf8_lossy(input)
			);
		}
	}

	#[test]
	fn base64_handles_the_full_byte_range() {
		let all: Vec<u8> = (0..=255u8).collect();
		let encoded = base64_encode(&all);
		// 256 字节 → 向上取整到 3 的倍数 258 → 344 个字符
		assert_eq!(encoded.len(), 344);
		assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g"));
	}

	#[test]
	fn base64_of_a_basic_credential() {
		// Basic 认证最经典的例子
		assert_eq!(
			base64_encode(b"Aladdin:open sesame"),
			"QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
		);
	}

	// ---------- 请求构造 ----------

	#[tokio::test]
	async fn sends_a_well_formed_connect_request() {
		let mut stream = stream_with(b"HTTP/1.1 200 Connection established\r\n\r\n");
		negotiate(&mut stream, &target("10.0.0.9", 3389), None, &label())
			.await
			.expect("应成功");

		let request = String::from_utf8_lossy(&stream.into_written()).to_string();
		assert!(
			request.starts_with("CONNECT 10.0.0.9:3389 HTTP/1.1\r\n"),
			"{request}"
		);
		assert!(request.contains("Host: 10.0.0.9:3389\r\n"), "{request}");
		assert!(
			!request.contains("Proxy-Authorization"),
			"无凭据时不应带认证头"
		);
		assert!(request.ends_with("\r\n\r\n"), "请求头应以空行结束");
	}

	#[tokio::test]
	async fn sends_basic_authorization_when_configured() {
		let auth = ProxyAuth::new("alice", "s3cret");
		let mut stream = stream_with(b"HTTP/1.1 200 OK\r\n\r\n");
		negotiate(
			&mut stream,
			&target("10.0.0.9", 3389),
			Some(&auth),
			&label(),
		)
		.await
		.expect("应成功");

		let request = String::from_utf8_lossy(&stream.into_written()).to_string();
		let expected = base64_encode(b"alice:s3cret");
		assert!(
			request.contains(&format!("Proxy-Authorization: Basic {expected}\r\n")),
			"认证头不符：{request}"
		);
	}

	#[tokio::test]
	async fn ipv6_target_uses_brackets_in_the_authority() {
		let mut stream = stream_with(b"HTTP/1.1 200 OK\r\n\r\n");
		negotiate(&mut stream, &target("2001:db8::1", 3389), None, &label())
			.await
			.expect("应成功");

		let request = String::from_utf8_lossy(&stream.into_written()).to_string();
		assert!(
			request.starts_with("CONNECT [2001:db8::1]:3389 HTTP/1.1\r\n"),
			"IPv6 目标需要方括号：{request}"
		);
		assert!(
			request.contains("Host: [2001:db8::1]:3389\r\n"),
			"Host 头也要带方括号：{request}"
		);
	}

	#[tokio::test]
	async fn domain_target_passes_through_unresolved() {
		let mut stream = stream_with(b"HTTP/1.1 200 OK\r\n\r\n");
		negotiate(
			&mut stream,
			&target("win.example.com", 3389),
			None,
			&label(),
		)
		.await
		.expect("应成功");

		let request = String::from_utf8_lossy(&stream.into_written()).to_string();
		// 域名原样交给代理解析——内网域名只有代理那侧解析得对
		assert!(
			request.starts_with("CONNECT win.example.com:3389 HTTP/1.1\r\n"),
			"{request}"
		);
	}

	// ---------- 响应处理 ----------

	#[tokio::test]
	async fn leaves_tunnel_data_intact() {
		// 响应头之后紧跟隧道数据：逐字节读的意义就在这条用例
		let mut response =
			b"HTTP/1.1 200 Connection established\r\nServer: tinyproxy\r\n\r\n".to_vec();
		response.extend_from_slice(b"\x03\x00\x00\x0bRDP-first-packet");

		let mut stream = stream_with(&response);
		negotiate(&mut stream, &target("10.0.0.9", 3389), None, &label())
			.await
			.expect("应成功");

		let mut rest = Vec::new();
		stream.read_to_end(&mut rest).await.expect("读取失败");
		assert_eq!(
			rest, b"\x03\x00\x00\x0bRDP-first-packet",
			"隧道的第一段数据不能被吞掉"
		);
	}

	#[tokio::test]
	async fn accepts_any_2xx() {
		for status in [&b"HTTP/1.1 200 OK"[..], &b"HTTP/1.0 204 No Content"[..]] {
			let mut response = status.to_vec();
			response.extend_from_slice(b"\r\n\r\n");
			let mut stream = stream_with(&response);
			negotiate(&mut stream, &target("10.0.0.9", 3389), None, &label())
				.await
				.unwrap_or_else(|error| panic!("{:?} 应被接受：{error}", status));
		}
	}

	#[tokio::test]
	async fn auth_required_is_reported_clearly() {
		let mut stream = stream_with(
			b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n",
		);
		let error = negotiate(&mut stream, &target("10.0.0.9", 3389), None, &label())
			.await
			.expect_err("407 不应成功");

		match error {
			NetError::ProxyAuthRejected { reason, proxy } => {
				assert!(reason.contains("没有配置凭据"), "应指出缺凭据：{reason}");
				assert_eq!(proxy, label());
			},
			other => panic!("应为 ProxyAuthRejected，实际 {other:?}"),
		}
	}

	#[tokio::test]
	async fn bad_credentials_are_distinguished_from_missing_ones() {
		let auth = ProxyAuth::new("alice", "wrong");
		let mut stream = stream_with(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n");
		let error = negotiate(
			&mut stream,
			&target("10.0.0.9", 3389),
			Some(&auth),
			&label(),
		)
		.await
		.expect_err("错误凭据不应成功");

		match error {
			NetError::ProxyAuthRejected { reason, .. } => {
				assert!(reason.contains("用户名或密码"), "应指到凭据本身：{reason}");
			},
			other => panic!("应为 ProxyAuthRejected，实际 {other:?}"),
		}
	}

	#[tokio::test]
	async fn target_unreachable_is_reported_with_the_target() {
		let mut stream = stream_with(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
		let error = negotiate(&mut stream, &target("10.0.0.9", 3389), None, &label())
			.await
			.expect_err("502 不应成功");

		match error {
			NetError::ProxyRefused { target, reason, .. } => {
				assert_eq!(target, "10.0.0.9:3389", "要指出是哪个目标没连上");
				assert!(reason.contains("无法连接目标"), "原因应可读：{reason}");
			},
			other => panic!("应为 ProxyRefused，实际 {other:?}"),
		}
	}

	#[tokio::test]
	async fn non_http_response_is_reported_as_protocol_error() {
		// 连到了不是 HTTP 代理的服务（例如误把 SSH 端口填成代理）
		let mut stream = stream_with(b"SSH-2.0-OpenSSH_9.9\r\n\r\n");
		let error = negotiate(&mut stream, &target("10.0.0.9", 3389), None, &label())
			.await
			.expect_err("非 HTTP 响应不应成功");

		match error {
			NetError::ProxyProtocol { reason, .. } => {
				assert!(reason.contains("状态行"), "应指出是状态行问题：{reason}");
			},
			other => panic!("应为 ProxyProtocol，实际 {other:?}"),
		}
	}

	#[tokio::test]
	async fn truncated_response_is_reported() {
		// 只发了半行就断开
		let mut stream = stream_with(b"HTTP/1.1 200 Conn");
		let error = negotiate(&mut stream, &target("10.0.0.9", 3389), None, &label())
			.await
			.expect_err("响应不完整不应成功");

		assert!(
			matches!(error, NetError::ProxyProtocol { .. }),
			"应为 ProxyProtocol，实际 {error:?}"
		);
	}

	#[tokio::test]
	async fn oversized_header_is_rejected() {
		// 一直发非终止的垃圾，必须在 8 KiB 处停下
		let mut junk = b"HTTP/1.1 200 OK\r\n".to_vec();
		junk.extend(std::iter::repeat_n(b'X', MAX_HEADER_BYTES + 100));
		let mut stream = stream_with(&junk);

		let error = negotiate(&mut stream, &target("10.0.0.9", 3389), None, &label())
			.await
			.expect_err("超长响应头不应成功");

		match error {
			NetError::ProxyProtocol { reason, .. } => {
				assert!(reason.contains("不像是 HTTP 代理"), "原因应可读：{reason}");
			},
			other => panic!("应为 ProxyProtocol，实际 {other:?}"),
		}
	}

	#[test]
	fn known_status_codes_have_readable_messages() {
		for status in [400u16, 403, 405, 408, 502, 503, 504] {
			let message = status_message(status);
			assert!(
				!message.contains("代理返回状态码"),
				"{status} 应有专门文案，实际：{message}"
			);
		}
		assert_eq!(status_message(418), "代理返回状态码 418");
	}

	#[test]
	fn authority_brackets_only_ipv6() {
		assert_eq!(authority_of(&target("10.0.0.9", 22)), "10.0.0.9:22");
		assert_eq!(authority_of(&target("host.example", 22)), "host.example:22");
		assert_eq!(authority_of(&target("::1", 22)), "[::1]:22");
	}
}
