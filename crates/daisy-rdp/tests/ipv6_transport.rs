//! IPv6 传输层验证（S8-64d）。
//!
//! 目标机是 IPv4-only 的 Windows，没有可用的 IPv6 RDP 目标（PLAN R16）。
//! 所以这里换一种更硬的验证方式：**在 `::1` 上起一个只说 X.224 的假服务端**，
//! 让真正的 FreeRDP 客户端连上去。能走到「MCS 连接初始化」这一步，
//! 就说明下面这几件事在 IPv6 上都是通的：
//!
//! ```text
//! getaddrinfo(AF_INET6) → socket(AF_INET6) → connect → TPKT 收发
//!   → X.224 Connection Request 解析 → Connection Confirm 解析
//!   → 协议协商（选中 PROTOCOL_RDP）→ 发 MCS Connect Initial
//! ```
//!
//! 反过来，如果地址层（`HostAddr` / `RdpTarget`）把地址搞错了，
//! 失败会停在更早的阶段（DNS 找不到 / TCP 连不上），服务端一个字节都收不到。
//!
//! 假服务端收到 MCS Connect Initial 之后直接关连接——**画面与输入验证不了**，
//! 那需要一个真支持 IPv6 的 RDP 服务端，见 `docs/ACCEPTANCE-S8.md` 的说明。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use daisy_rdp::{ConnectionParams, RdpClient};

/// X.224 Connection Confirm + `RDP_NEG_RSP`（选中标准 RDP 安全，不升 TLS）。
///
/// 字节逐项对应 MS-RDPBCGR 2.2.1.2 与 `libfreerdp/core/nego.c:1584`
/// （`nego_send_negotiation_response`）里的服务端写法：
///
/// ```text
/// 03 00 00 13             TPKT：版本 3，总长 19
/// 0E                      X.224 LI = 6 + 8（协商数据）
/// D0                      Connection Confirm
/// 00 00 / 00 01 / 00      dst-ref / src-ref / class
/// 02 00 08 00 00 00 00 00 TYPE_RDP_NEG_RSP, flags=0, len=8, protocol=RDP
/// ```
const CONNECTION_CONFIRM: [u8; 19] = [
	0x03, 0x00, 0x00, 0x13, 0x0E, 0xD0, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x08, 0x00, 0x00,
	0x00, 0x00, 0x00,
];

/// 服务端观察到的事实。
#[derive(Debug, Default)]
struct Observed {
	/// 收到的 X.224 Connection Request 长度（不含 TPKT 头）。
	request_len: usize,
	/// 发完确认之后客户端又发了多少字节（应当是 MCS Connect Initial）。
	after_confirm: usize,
	/// 那批字节的开头几个，用于确认是 MCS 的 BER 编码。
	after_confirm_head: Vec<u8>,
}

/// 读一个 TPKT 帧：先 4 字节头，长度字段是大端。
fn read_tpkt(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
	let mut header = [0u8; 4];
	stream.read_exact(&mut header)?;
	let len = u16::from_be_bytes([header[2], header[3]]) as usize;
	assert!(len >= 4, "TPKT 长度不合法：{len}");
	let mut body = vec![0u8; len - 4];
	stream.read_exact(&mut body)?;
	let mut frame = header.to_vec();
	frame.extend_from_slice(&body);
	Ok(frame)
}

/// 在 `bind_host` 上起一个只说 X.224 的服务端，返回实际端口与观察结果。
///
/// `bind_host` 与 `connect_host` 分开：链路本地地址**没法用 `std` 绑**
/// （`ToSocketAddrs` 不认 `%`，而 `bind` 走的就是它），但客户端连的时候
/// 走的是 FreeRDP 自己的 `getaddrinfo`——那正是要验证的地方。所以服务端
/// 绑通配地址，客户端用带区域的地址连过去。
fn spawn_fake_server(bind_host: &str) -> (u16, mpsc::Receiver<Observed>) {
	let listener = TcpListener::bind(format!("[{bind_host}]:0")).expect("绑定失败");
	let port = listener.local_addr().expect("取本地地址失败").port();
	let (tx, rx) = mpsc::channel();

	std::thread::spawn(move || {
		let mut observed = Observed::default();
		// 客户端迟迟不来就别把测试挂死
		let Ok((mut stream, _peer)) = listener.accept() else {
			let _ = tx.send(observed);
			return;
		};
		let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

		let frame = read_tpkt(&mut stream).expect("读连接请求失败");
		// TPKT 头 4 字节 + X.224 LI 1 字节
		observed.request_len = frame.len() - 5;
		assert_eq!(frame[5], 0xE0, "应当是 X.224 Connection Request");

		stream.write_all(&CONNECTION_CONFIRM).expect("回确认失败");

		// 客户端接下来应当发 MCS Connect Initial（BER 编码，首字节 0x7F）
		if let Ok(next) = read_tpkt(&mut stream) {
			observed.after_confirm = next.len();
			observed.after_confirm_head = next.iter().take(16).copied().collect();
		}

		let _ = tx.send(observed);
	});

	(port, rx)
}

/// 在 `::1` 上走完整条 X.224 握手。
#[test]
fn loopback_ipv6_reaches_mcs_connect_initial() {
	let (port, observed) = spawn_fake_server("::1");

	let mut params = ConnectionParams::new("::1", "daisy", "x");
	params.port = port;

	let mut client = RdpClient::new().expect("创建实例失败");
	client.trust_certificate(None);
	// 假服务端只做到 X.224，后面必定失败——这里断言的是**失败在哪一步**
	let error = client.connect(&params).expect_err("假服务端不提供完整会话");
	let text = error.to_string();

	let observed = observed
		.recv_timeout(Duration::from_secs(10))
		.expect("服务端没有回报结果");

	assert!(
		observed.request_len > 0,
		"服务端没收到连接请求：IPv6 的 TCP 没通"
	);
	assert!(
		observed.after_confirm > 0,
		"服务端回确认后客户端没继续发：X.224 协商没通过（错误：{text}）"
	);
	assert_mcs_connect_initial(&observed);
}

/// 客户端在 X.224 之后发的是 **MCS Connect Initial**：
/// `TPKT | 02 F0（X.224 Data TPDU）| 7F 65 …（BER: Connect-Initial）`。
///
/// 只断言「有数据」不够——那可能只是一次重连的 Connection Request。
/// 断言到 BER 标记 `7F 65` 才说明：传输层、X.224 协商、以及 MCS 的起步
/// 全都真实发生在了 IPv6 连接上。
fn assert_mcs_connect_initial(observed: &Observed) {
	let head = &observed.after_confirm_head;
	assert!(
		head.len() >= 8,
		"确认之后收到的数据太短，不足一个 MCS 帧：{head:02X?}"
	);
	assert_eq!(head[0], 0x03, "应当以 TPKT 版本号开头：{head:02X?}");
	assert_eq!(
		head[5], 0xF0,
		"TPKT 之后应当是 X.224 Data TPDU：{head:02X?}"
	);
	assert!(
		head.windows(2).any(|w| w == [0x7F, 0x65]),
		"没找到 MCS Connect-Initial 的 BER 标记 7F 65：{head:02X?}"
	);
}

/// 带区域标识的链路本地地址：`%区域` 必须一路传到 `getaddrinfo` 才算数。
///
/// 用 macOS 回环接口的 `fe80::1%lo0`，不碰真实网卡。没这个地址的环境
/// （部分 Linux 发行版默认不给 lo 配链路本地）直接跳过——那是环境差异，
/// 不是代码问题。
#[test]
fn link_local_with_zone_id_reaches_mcs_connect_initial() {
	let addr = "fe80::1%lo0";

	// 服务端绑通配地址，客户端连 `%lo0` 的链路本地地址
	let (port, observed) = spawn_fake_server("::");
	let mut params = ConnectionParams::new(addr, "daisy", "x");
	params.port = port;

	let mut client = RdpClient::new().expect("创建实例失败");
	let error = client.connect(&params).expect_err("假服务端不提供完整会话");

	let observed = observed
		.recv_timeout(Duration::from_secs(10))
		.expect("服务端没有回报结果");
	assert!(
		observed.request_len > 0,
		"带 %lo0 的地址没能连上：区域标识大概在某一层被剥掉了（错误：{error}）"
	);
	assert_mcs_connect_initial(&observed);
}
