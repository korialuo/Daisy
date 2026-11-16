//! S8 验收：用 Daisy 的 Rust 侧 FreeRDP 封装连真实 Windows 主机并抓帧。
//!
//! 这是 S1-6 的 C 程序 `rdp_capture` 的 Rust 等价物——验证 trampoline
//! 真的能跨 FFI 边界工作、帧真的能从 FreeRDP 工作线程拿到。
//!
//! 用法：
//!
//! ```bash
//! DAISY_RDP_PASSWORD='<密码>' cargo run -p daisy-rdp --example capture 192.168.10.20
//! ```
//!
//! 密码只从环境变量读。抓到帧后写出一张 PNG（用已链接的 zlib 手写，
//! 不引入额外依赖，与 C 版一致）。

use std::time::Duration;

use daisy_rdp::{ConnectionParams, RdpClient};

fn env_required(key: &str) -> String {
	std::env::var(key).unwrap_or_else(|_| {
		eprintln!("缺少环境变量 {key}");
		std::process::exit(2);
	})
}

fn main() {
	let args: Vec<String> = std::env::args().collect();
	let host = args.get(1).map(String::as_str).unwrap_or("192.168.10.20");
	let out_path = args.get(2).map(String::as_str).unwrap_or("rdp_frame.png");

	let password = env_required("DAISY_RDP_PASSWORD");
	let settle_ms: u64 = std::env::var("DAISY_RDP_SETTLE_MS")
		.ok()
		.and_then(|v| v.parse().ok())
		.unwrap_or(5000);

	let params = ConnectionParams::new(host, "Administrator", &password).with_resolution(1024, 768);

	println!("目标 {host} 分辨率 1024x768，稳定期 {settle_ms} ms");

	let mut client = match RdpClient::new() {
		Ok(client) => client,
		Err(error) => {
			eprintln!("创建实例失败：{error}");
			std::process::exit(1);
		},
	};

	// 认证失败是最常见的结局（密码过期、账号名不对），不该打一屏 backtrace
	if let Err(error) = client.connect(&params) {
		eprintln!("连接失败：{error}");
		std::process::exit(1);
	}

	println!("连接成功，等待画面稳定 ...");

	// 泵事件循环直到拿到帧或超时
	let deadline = std::time::Instant::now() + Duration::from_millis(settle_ms);
	while std::time::Instant::now() < deadline {
		if client.latest_frame().is_some() {
			break;
		}
		if client.is_disconnected() {
			eprintln!("会话提前断开");
			std::process::exit(1);
		}
		// 泵 200ms 一段
		let _ = client.pump(200);
	}

	// 稳定期继续泵，让画面画完（首帧通常全黑）
	let settle_deadline = std::time::Instant::now() + Duration::from_millis(settle_ms);
	while std::time::Instant::now() < settle_deadline && !client.is_disconnected() {
		let _ = client.pump(100);
	}

	let (seq, frame) = match client.latest_frame() {
		Some(pair) => pair,
		None => {
			eprintln!("未拿到任何帧");
			std::process::exit(1);
		},
	};

	println!(
		"拿到第 {seq} 帧：{}x{} stride={}（{} 字节）",
		frame.width,
		frame.height,
		frame.stride,
		frame.data.len()
	);

	write_png(out_path, &frame);
	println!("已写出 {out_path}");
}

/// 手写 PNG（XRGB32 → RGB），基于已链接的 zlib。
fn write_png(path: &str, frame: &daisy_rdp::FrameBuffer) {
	// 用系统的 Python 或直接写一个最小的 PNG 编码器。
	// 这里复用 C 版的逻辑思路，但用 Rust + zlib 已链接的能力。
	// 为简洁起见，直接调用一个 helper。
	let raw_len = (frame.width as usize * 3 + 1) * frame.height as usize;
	let mut raw = Vec::with_capacity(raw_len);

	for y in 0..frame.height as usize {
		raw.push(0); // filter type: None
		let row = &frame.data[y * frame.stride as usize..];
		for x in 0..frame.width as usize {
			// XRGB32 内存序：X,R,G,B
			raw.push(row[x * 4 + 1]); // R
			raw.push(row[x * 4 + 2]); // G
			raw.push(row[x * 4 + 3]); // B
		}
	}

	let png = encode_png(frame.width, frame.height, &raw);
	std::fs::write(path, png).expect("写 PNG 失败");
}

/// 最小 PNG 编码器（零依赖，IDAT 用 zlib 无压缩 store 块）。
fn encode_png(width: u32, height: u32, raw: &[u8]) -> Vec<u8> {
	fn be32(v: u32) -> [u8; 4] {
		v.to_be_bytes()
	}

	fn crc32(data: &[u8]) -> u32 {
		let mut crc: u32 = 0xFFFF_FFFF;
		for &byte in data {
			crc ^= u32::from(byte);
			for _ in 0..8 {
				let mask = (crc & 1).wrapping_neg();
				crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
			}
		}
		!crc
	}

	fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
		out.extend_from_slice(&be32(data.len() as u32));
		out.extend_from_slice(kind);
		out.extend_from_slice(data);
		let mut crc_input = Vec::with_capacity(4 + data.len());
		crc_input.extend_from_slice(kind);
		crc_input.extend_from_slice(data);
		out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
	}

	// zlib 无压缩流：header + 若干 store 块 + adler32
	let mut zlib = Vec::new();
	zlib.push(0x78); // CMF: deflate, 32K window
	zlib.push(0x01); // FLG: no dict, FLEVEL=0
	let mut offset = 0;
	while offset < raw.len() {
		let end = (offset + 0xFFFF).min(raw.len());
		let is_last = end == raw.len();
		zlib.push(if is_last { 0x01 } else { 0x00 }); // BFINAL + BTYPE=00(store)
		let len = (end - offset) as u16;
		zlib.extend_from_slice(&len.to_le_bytes());
		zlib.extend_from_slice(&(!len).to_le_bytes());
		zlib.extend_from_slice(&raw[offset..end]);
		offset = end;
	}
	zlib.extend_from_slice(&adler32(raw).to_be_bytes());

	let mut out = Vec::new();
	out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

	let mut ihdr = Vec::with_capacity(13);
	ihdr.extend_from_slice(&be32(width));
	ihdr.extend_from_slice(&be32(height));
	ihdr.push(8); // bit depth
	ihdr.push(2); // color type: truecolor RGB
	ihdr.push(0); // compression
	ihdr.push(0); // filter
	ihdr.push(0); // interlace
	chunk(&mut out, b"IHDR", &ihdr);

	chunk(&mut out, b"IDAT", &zlib);
	chunk(&mut out, b"IEND", &[]);
	out
}

fn adler32(data: &[u8]) -> u32 {
	let mut a: u32 = 1;
	let mut b: u32 = 0;
	for &byte in data {
		a = (a + u32::from(byte)) % 65521;
		b = (b + a) % 65521;
	}
	(b << 16) | a
}
