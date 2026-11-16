//! 测试专用：一个「读自预置响应、写进独立缓冲」的内存双向流。
//!
//! SOCKS5 / HTTP CONNECT 的握手是「写一段、读一段」。若拿
//! `std::io::Cursor<Vec<u8>>` 直接当流，写会从 position 0 开始覆盖
//! 预置的响应字节，导致读到的全是自己刚写的内容或 EOF。
//!
//! 这里把两个方向分开：读走 `response`，写收集到 `written`，
//! 互不干扰。仅用于测试，不进入生产代码路径。

use std::io::{self, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// 一个预置了响应、且能记录写入内容的双向流。
pub struct MemStream {
	/// 待读出的响应字节。
	response: std::io::Cursor<Vec<u8>>,
	/// 客户端写进来的字节，按序累积。
	written: Vec<u8>,
}

impl MemStream {
	/// 用预置响应构造。写缓冲初始为空。
	#[must_use]
	pub fn new(response: Vec<u8>) -> Self {
		Self {
			response: std::io::Cursor::new(response),
			written: Vec::new(),
		}
	}

	/// 取出客户端写进来的全部字节。
	#[must_use]
	pub fn into_written(self) -> Vec<u8> {
		self.written
	}
}

impl AsyncRead for MemStream {
	fn poll_read(
		mut self: Pin<&mut Self>,
		_cx: &mut Context<'_>,
		buf: &mut ReadBuf<'_>,
	) -> Poll<io::Result<()>> {
		let mut tmp = [0u8; 256];
		let max = buf.remaining().min(tmp.len());
		match self.response.read(&mut tmp[..max]) {
			Ok(n) => {
				buf.put_slice(&tmp[..n]);
				Poll::Ready(Ok(()))
			},
			Err(error) => Poll::Ready(Err(error)),
		}
	}
}

impl AsyncWrite for MemStream {
	fn poll_write(
		mut self: Pin<&mut Self>,
		_cx: &mut Context<'_>,
		buf: &[u8],
	) -> Poll<io::Result<usize>> {
		self.written.extend_from_slice(buf);
		Poll::Ready(Ok(buf.len()))
	}

	fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Poll::Ready(Ok(()))
	}

	fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
		Poll::Ready(Ok(()))
	}
}
