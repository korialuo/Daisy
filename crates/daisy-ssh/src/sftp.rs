//! SFTP v3 客户端（只做 Daisy 用得到的子集）。
//!
//! ## 为什么自己实现
//!
//! `russh-sftp` 绑的是旧版 `russh`（0.5x），而本项目用 0.64——两套 `russh`
//! 的 `Handle` 类型不兼容，用不了。自己实现反而更省事：需要的只有十来个包型，
//! 而且是**串行请求**（发一个等一个），不需要管道与并发调度。
//!
//! 协议依据 `draft-ietf-secsh-filexfer-02`（即 SFTP v3，OpenSSH 的默认实现）。
//!
//! ## 边界
//!
//! - **单连接串行**：一次只有一个未完成的请求。Daisy 的文件操作是人点出来的，
//!   并发没有收益，反而要处理 id 匹配与乱序。
//! - **不做符号链接解析**：`lstat` 语义，符号链接当文件展示（大小是链接本身）。
//! - **不做权限编辑**：只读属性，写权限留到以后。

use std::path::Path;

use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Result, SshError};
use crate::session::SshSession;

// ==================== 协议常量 ====================

/// `SSH_FXP_INIT`：客户端握手。
const FXP_INIT: u8 = 1;
/// `SSH_FXP_VERSION`：服务端回应握手。
const FXP_VERSION: u8 = 2;
/// `SSH_FXP_OPEN`
const FXP_OPEN: u8 = 3;
/// `SSH_FXP_CLOSE`
const FXP_CLOSE: u8 = 4;
/// `SSH_FXP_READ`
const FXP_READ: u8 = 5;
/// `SSH_FXP_WRITE`
const FXP_WRITE: u8 = 6;
/// `SSH_FXP_LSTAT`：不跟随符号链接的 stat。
const FXP_LSTAT: u8 = 7;
/// `SSH_FXP_OPENDIR`
const FXP_OPENDIR: u8 = 11;
/// `SSH_FXP_READDIR`
const FXP_READDIR: u8 = 12;
/// `SSH_FXP_REMOVE`：删文件。
const FXP_REMOVE: u8 = 13;
/// `SSH_FXP_MKDIR`
const FXP_MKDIR: u8 = 14;
/// `SSH_FXP_RMDIR`：删空目录。
const FXP_RMDIR: u8 = 15;
/// `SSH_FXP_REALPATH`：规范化路径（也用来拿登录用户的 home）。
const FXP_REALPATH: u8 = 16;
/// `SSH_FXP_RENAME`
const FXP_RENAME: u8 = 18;

/// `SSH_FXP_STATUS`
const FXP_STATUS: u8 = 101;
/// `SSH_FXP_HANDLE`
const FXP_HANDLE: u8 = 102;
/// `SSH_FXP_DATA`
const FXP_DATA: u8 = 103;
/// `SSH_FXP_NAME`：目录项 / realpath 结果。
const FXP_NAME: u8 = 104;
/// `SSH_FXP_ATTRS`
const FXP_ATTRS: u8 = 105;

/// 打开文件的标志（v3 的 `pflags`）。
const FXF_READ: u32 = 0x01;
const FXF_WRITE: u32 = 0x02;
const FXF_CREAT: u32 = 0x08;
const FXF_TRUNC: u32 = 0x10;

/// `SSH_FX_OK`
const FX_OK: u32 = 0;
/// `SSH_FX_EOF`
const FX_EOF: u32 = 1;
/// `SSH_FX_NO_SUCH_FILE`
const FX_NO_SUCH_FILE: u32 = 2;
/// `SSH_FX_PERMISSION_DENIED`
const FX_PERMISSION_DENIED: u32 = 3;

/// 属性位：文件大小。
const ATTR_SIZE: u32 = 0x0000_0001;
/// 属性位：uid / gid。
const ATTR_UIDGID: u32 = 0x0000_0002;
/// 属性位：权限（含类型位，`ls -l` 的第一列就在这）。
const ATTR_PERMISSIONS: u32 = 0x0000_0004;
/// 属性位：访问 / 修改时间。
const ATTR_ACMODTIME: u32 = 0x0000_0008;
/// 属性位：扩展字段。
const ATTR_EXTENDED: u32 = 0x8000_0000;

/// 单次读写块大小。
///
/// 32 KB 是 OpenSSH 的默认 `sftp` 客户端的选择：再大就要靠通道窗口撑，
/// 小文件反而多几次往返。协议允许 34 KB 左右的包，留出头部空间。
const CHUNK: usize = 32 * 1024;

/// 协议包长度上限。超了就是流错位，继续读只会把内存吃光。
const MAX_PACKET: usize = 256 * 1024;

// ==================== 数据结构 ====================

/// 目录项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
	/// 文件名（不含目录）。
	pub name: String,
	/// 绝对路径，直接可用于后续操作。
	pub path: String,
	/// 是否目录。
	pub is_dir: bool,
	/// 文件大小（目录为 0）。
	pub size: u64,
	/// 修改时间（Unix 秒）。
	pub mtime: Option<u32>,
	/// 权限位（含文件类型位，如 `0o100644`）。
	pub permissions: Option<u32>,
}

impl DirEntry {
	/// 是否符号链接。
	#[must_use]
	pub fn is_symlink(&self) -> bool {
		self.permissions
			.is_some_and(|mode| mode & 0o170_000 == 0o120_000)
	}

	/// `ls -l` 风格的权限串，如 `drwxr-xr-x`。
	#[must_use]
	pub fn mode_text(&self) -> String {
		let Some(mode) = self.permissions else {
			return "-".to_string();
		};
		let kind = match mode & 0o170_000 {
			0o040_000 => 'd',
			0o120_000 => 'l',
			0o010_000 => 'p',
			0o020_000 => 'c',
			0o060_000 => 'b',
			0o140_000 => 's',
			_ => '-',
		};
		let mut out = String::with_capacity(10);
		out.push(kind);
		for shift in [6, 3, 0] {
			let bits = (mode >> shift) & 0o7;
			out.push(if bits & 0o4 != 0 { 'r' } else { '-' });
			out.push(if bits & 0o2 != 0 { 'w' } else { '-' });
			out.push(if bits & 0o1 != 0 { 'x' } else { '-' });
		}
		out
	}
}

/// 一组文件属性。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attrs {
	/// 大小。
	pub size: Option<u64>,
	/// 权限位。
	pub permissions: Option<u32>,
	/// 修改时间（Unix 秒）。
	pub mtime: Option<u32>,
}

impl Attrs {
	/// 是否目录。
	#[must_use]
	pub fn is_dir(&self) -> bool {
		self.permissions
			.is_some_and(|mode| mode & 0o170_000 == 0o040_000)
	}
}

// ==================== 编码 ====================

/// 极简写入器。协议里只有四种类型，不值得为此引一个 crate。
struct Writer {
	buf: Vec<u8>,
}

impl Writer {
	fn new(kind: u8, request_id: u32) -> Self {
		let mut buf = Vec::with_capacity(64);
		buf.push(kind);
		buf.extend_from_slice(&request_id.to_be_bytes());
		Self { buf }
	}

	fn u32(mut self, value: u32) -> Self {
		self.buf.extend_from_slice(&value.to_be_bytes());
		self
	}

	fn u64(mut self, value: u64) -> Self {
		self.buf.extend_from_slice(&value.to_be_bytes());
		self
	}

	fn str(mut self, value: &str) -> Self {
		self.buf
			.extend_from_slice(&(value.len() as u32).to_be_bytes());
		self.buf.extend_from_slice(value.as_bytes());
		self
	}

	fn bytes(mut self, value: &[u8]) -> Self {
		self.buf
			.extend_from_slice(&(value.len() as u32).to_be_bytes());
		self.buf.extend_from_slice(value);
		self
	}

	/// 空属性集：`flags = 0`。
	fn empty_attrs(mut self) -> Self {
		self.buf.extend_from_slice(&0u32.to_be_bytes());
		self
	}

	fn finish(self) -> Vec<u8> {
		self.buf
	}
}

/// 读取器。越界一律当协议错误——继续读只会拿到垃圾数据。
struct Reader<'a> {
	buf: &'a [u8],
	pos: usize,
}

impl<'a> Reader<'a> {
	fn new(buf: &'a [u8]) -> Self {
		Self { buf, pos: 0 }
	}

	fn take(&mut self, len: usize) -> Result<&'a [u8]> {
		let end = self.pos.checked_add(len).ok_or_else(truncated)?;
		if end > self.buf.len() {
			return Err(truncated());
		}
		let slice = &self.buf[self.pos..end];
		self.pos = end;
		Ok(slice)
	}

	fn u8(&mut self) -> Result<u8> {
		Ok(self.take(1)?[0])
	}

	fn u32(&mut self) -> Result<u32> {
		let bytes = self.take(4)?;
		Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
	}

	fn u64(&mut self) -> Result<u64> {
		let bytes = self.take(8)?;
		let mut array = [0u8; 8];
		array.copy_from_slice(bytes);
		Ok(u64::from_be_bytes(array))
	}

	fn str(&mut self) -> Result<String> {
		let len = self.u32()? as usize;
		let bytes = self.take(len)?;
		Ok(String::from_utf8_lossy(bytes).into_owned())
	}

	fn bytes(&mut self) -> Result<Vec<u8>> {
		let len = self.u32()? as usize;
		Ok(self.take(len)?.to_vec())
	}

	/// 读一组属性，游标停在属性之后。目录列表要连读很多组，必须精确。
	fn attrs(&mut self) -> Result<Attrs> {
		let flags = self.u32()?;
		let mut attrs = Attrs::default();
		if flags & ATTR_SIZE != 0 {
			attrs.size = Some(self.u64()?);
		}
		if flags & ATTR_UIDGID != 0 {
			self.u32()?;
			self.u32()?;
		}
		if flags & ATTR_PERMISSIONS != 0 {
			attrs.permissions = Some(self.u32()?);
		}
		if flags & ATTR_ACMODTIME != 0 {
			self.u32()?; // atime：界面上只显示 mtime
			attrs.mtime = Some(self.u32()?);
		}
		if flags & ATTR_EXTENDED != 0 {
			let count = self.u32()?;
			for _ in 0..count {
				self.str()?;
				self.str()?;
			}
		}
		Ok(attrs)
	}
}

fn truncated() -> SshError {
	SshError::Sftp("SFTP 报文不完整（流已错位）".to_string())
}

// ==================== 客户端 ====================

/// 一条 SFTP 会话。
///
/// `SftpClient` 不是 `Send`（借用了通道两半），所以它活在自己的后台任务里，
/// 界面通过命令队列和它对讲——与 [`crate::channel::RemoteShell`] 同一套做法。
pub struct SftpClient {
	read: ChannelReadHalf,
	write: ChannelWriteHalf<russh::client::Msg>,
	/// 已收到但还没切包的字节（SSH 数据分片任意，包可能跨帧）。
	buffer: Vec<u8>,
	/// 下一个请求 id。
	next_id: u32,
}

impl SftpClient {
	/// 在既有会话上开一条 `sftp` 子系统通道并完成 v3 握手。
	pub async fn open(session: &SshSession) -> Result<Self> {
		let channel = session
			.handle()
			.channel_open_session()
			.await
			.map_err(|error| SshError::Sftp(format!("打开 SFTP 通道失败：{error}")))?;
		let (read, write) = channel.split();

		write
			.request_subsystem(true, "sftp")
			.await
			.map_err(|error| SshError::Sftp(format!("服务端不提供 sftp 子系统：{error}")))?;

		let mut client = Self {
			read,
			write,
			buffer: Vec::new(),
			next_id: 1,
		};
		client.handshake().await?;
		Ok(client)
	}

	/// `INIT` → `VERSION`。版本不是 3 时仍然继续：OpenSSH 一律回 3，
	/// 见到别的值说明对面不是标准实现，早点说清楚比后面解析错位强。
	async fn handshake(&mut self) -> Result<()> {
		let mut packet = vec![FXP_INIT];
		packet.extend_from_slice(&3u32.to_be_bytes());
		self.send(&packet).await?;

		let reply = self.recv().await?;
		let (&kind, rest) = reply.split_first().ok_or_else(truncated)?;
		if kind != FXP_VERSION {
			return Err(SshError::Sftp(format!(
				"SFTP 握手失败：期望 VERSION({FXP_VERSION})，收到 {kind}"
			)));
		}
		let mut reader = Reader::new(rest);
		let version = reader.u32()?;
		if version < 3 {
			return Err(SshError::Sftp(format!(
				"服务端只支持 SFTP v{version}，Daisy 需要 v3"
			)));
		}
		tracing::info!("[SFTP] 握手完成，协议版本 v{version}");
		Ok(())
	}

	async fn send(&self, packet: &[u8]) -> Result<()> {
		let mut framed = Vec::with_capacity(4 + packet.len());
		framed.extend_from_slice(&(packet.len() as u32).to_be_bytes());
		framed.extend_from_slice(packet);
		self.write
			.data_bytes(framed)
			.await
			.map_err(|error| SshError::Sftp(format!("写 SFTP 通道失败：{error}")))
	}

	/// 读一个完整包（`type` + payload，不含长度前缀）。
	async fn recv(&mut self) -> Result<Vec<u8>> {
		loop {
			if self.buffer.len() >= 4 {
				let len = u32::from_be_bytes([
					self.buffer[0],
					self.buffer[1],
					self.buffer[2],
					self.buffer[3],
				]) as usize;
				if len == 0 || len > MAX_PACKET {
					return Err(SshError::Sftp(format!("SFTP 包长度异常：{len}")));
				}
				if self.buffer.len() >= 4 + len {
					let packet = self.buffer[4..4 + len].to_vec();
					self.buffer.drain(..4 + len);
					return Ok(packet);
				}
			}
			match self.read.wait().await {
				Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
					self.buffer.extend_from_slice(&data);
				},
				Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
					return Err(SshError::Sftp("SFTP 通道已关闭".to_string()));
				},
				// 窗口调整之类与数据无关的消息，忽略
				Some(_) => {},
			}
		}
	}

	/// 发一个请求并等它的响应。
	///
	/// 串行模型下响应一定就是下一个包；仍校验 request-id——错位了要立刻发现，
	/// 而不是把上一轮的句柄当成这一轮的。
	async fn call(&mut self, request: Vec<u8>) -> Result<Vec<u8>> {
		let id = self.next_id;
		self.next_id = self.next_id.wrapping_add(1);
		self.send(&request).await?;
		loop {
			let packet = self.recv().await?;
			let mut reader = Reader::new(&packet);
			let kind = reader.u8()?;
			let reply_id = reader.u32()?;
			if reply_id != id {
				// 上一条请求的迟到响应：丢弃，继续等自己的
				tracing::debug!("[SFTP] 丢弃迟到响应 id={reply_id}（在等 {id}）");
				continue;
			}
			let mut out = Vec::with_capacity(packet.len() - 4);
			out.push(kind);
			out.extend_from_slice(&packet[5..]);
			return Ok(out);
		}
	}

	/// 切出 `(type, payload)`。
	fn split_packet(packet: Vec<u8>) -> Result<(u8, Vec<u8>)> {
		let mut iter = packet.into_iter();
		let kind = iter.next().ok_or_else(truncated)?;
		Ok((kind, iter.collect()))
	}

	/// 检查 STATUS 并翻译成中文错误。
	fn check_status(code: u32, message: &str, path: &str) -> Result<()> {
		match code {
			FX_OK => Ok(()),
			FX_NO_SUCH_FILE => Err(SshError::Sftp(format!("{path} 不存在"))),
			FX_PERMISSION_DENIED => Err(SshError::Sftp(format!("没有权限访问 {path}"))),
			_ => Err(SshError::Sftp(format!("{path}：{message}（code {code}）"))),
		}
	}

	/// 把路径规范化成绝对路径。空串按「登录用户的 home」处理。
	pub async fn realpath(&mut self, path: &str) -> Result<String> {
		let request = Writer::new(FXP_REALPATH, self.next_id).str(path).finish();
		let (kind, payload) = Self::split_packet(self.call(request).await?)?;
		let mut reader = Reader::new(&payload);
		match kind {
			FXP_NAME => {
				let count = reader.u32()?;
				if count == 0 {
					return Err(SshError::Sftp(format!("无法解析路径 {path}")));
				}
				let name = reader.str()?;
				Ok(name)
			},
			FXP_STATUS => {
				let code = reader.u32()?;
				let text = reader.str().unwrap_or_default();
				Self::check_status(code, &text, path)?;
				Err(SshError::Sftp(format!("{path} 没有返回路径")))
			},
			other => Err(SshError::Sftp(format!("realpath 收到意外响应 {other}"))),
		}
	}

	/// 列目录。`..` 与 `.` 由服务端返回，这里过滤掉。
	pub async fn list_dir(&mut self, path: &str) -> Result<Vec<DirEntry>> {
		let request = Writer::new(FXP_OPENDIR, self.next_id).str(path).finish();
		let (kind, payload) = Self::split_packet(self.call(request).await?)?;
		if kind == FXP_STATUS {
			let mut reader = Reader::new(&payload);
			let code = reader.u32()?;
			let text = reader.str().unwrap_or_default();
			Self::check_status(code, &text, path)?;
			return Ok(Vec::new());
		}
		if kind != FXP_HANDLE {
			return Err(SshError::Sftp(format!("opendir 收到意外响应 {kind}")));
		}
		let handle = Reader::new(&payload).bytes()?;

		let mut entries = Vec::new();
		loop {
			let request = Writer::new(FXP_READDIR, self.next_id)
				.bytes(&handle)
				.finish();
			let (kind, payload) = Self::split_packet(self.call(request).await?)?;
			match kind {
				FXP_NAME => {
					let mut reader = Reader::new(&payload);
					let count = reader.u32()?;
					for _ in 0..count {
						let name = reader.str()?;
						let _longname = reader.str()?;
						let attrs = reader.attrs()?;
						if name == "." || name == ".." {
							continue;
						}
						entries.push(DirEntry {
							path: join_path(path, &name),
							name,
							is_dir: attrs.is_dir(),
							size: attrs.size.unwrap_or(0),
							mtime: attrs.mtime,
							permissions: attrs.permissions,
						});
					}
				},
				FXP_STATUS => {
					let mut reader = Reader::new(&payload);
					let code = reader.u32()?;
					if code == FX_EOF {
						break;
					}
					let text = reader.str().unwrap_or_default();
					// 读完就关句柄，再报错
					let _ = self.close_handle(&handle).await;
					return Err(SshError::Sftp(format!("读目录 {path} 失败：{text}")));
				},
				other => {
					return Err(SshError::Sftp(format!("readdir 收到意外响应 {other}")));
				},
			}
		}
		self.close_handle(&handle).await?;
		// 目录在前、同类按名称排：与文件管理器一致，也让树更好认
		entries.sort_by(|a, b| {
			b.is_dir
				.cmp(&a.is_dir)
				.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
		});
		Ok(entries)
	}

	/// 取属性；文件不存在返回 `None`（不当作错误，界面上就是「没有」）。
	pub async fn stat(&mut self, path: &str) -> Result<Option<Attrs>> {
		let request = Writer::new(FXP_LSTAT, self.next_id).str(path).finish();
		let (kind, payload) = Self::split_packet(self.call(request).await?)?;
		let mut reader = Reader::new(&payload);
		match kind {
			FXP_ATTRS => Ok(Some(reader.attrs()?)),
			FXP_STATUS => {
				let code = reader.u32()?;
				if code == FX_NO_SUCH_FILE {
					return Ok(None);
				}
				let text = reader.str().unwrap_or_default();
				Self::check_status(code, &text, path)?;
				Ok(None)
			},
			other => Err(SshError::Sftp(format!("stat 收到意外响应 {other}"))),
		}
	}

	async fn close_handle(&mut self, handle: &[u8]) -> Result<()> {
		let request = Writer::new(FXP_CLOSE, self.next_id).bytes(handle).finish();
		let (kind, payload) = Self::split_packet(self.call(request).await?)?;
		if kind != FXP_STATUS {
			return Err(SshError::Sftp(format!("close 收到意外响应 {kind}")));
		}
		let mut reader = Reader::new(&payload);
		let code = reader.u32()?;
		let text = reader.str().unwrap_or_default();
		Self::check_status(code, &text, "关闭句柄")?;
		Ok(())
	}

	async fn open_handle(&mut self, path: &str, pflags: u32) -> Result<Vec<u8>> {
		let request = Writer::new(FXP_OPEN, self.next_id)
			.str(path)
			.u32(pflags)
			.empty_attrs()
			.finish();
		let (kind, payload) = Self::split_packet(self.call(request).await?)?;
		match kind {
			FXP_HANDLE => Ok(Reader::new(&payload).bytes()?),
			FXP_STATUS => {
				let mut reader = Reader::new(&payload);
				let code = reader.u32()?;
				let text = reader.str().unwrap_or_default();
				Self::check_status(code, &text, path)?;
				Err(SshError::Sftp(format!("打开 {path} 失败")))
			},
			other => Err(SshError::Sftp(format!("open 收到意外响应 {other}"))),
		}
	}

	/// 下载：流式读取并写入本地文件，返回字节数。
	pub async fn download(&mut self, remote: &str, local: &Path) -> Result<u64> {
		let handle = self.open_handle(remote, FXF_READ).await?;
		let mut file = tokio::fs::File::create(local).await.map_err(|error| {
			SshError::Sftp(format!("创建本地文件 {} 失败：{error}", local.display()))
		})?;

		let mut offset = 0u64;
		let mut total = 0u64;
		loop {
			let request = Writer::new(FXP_READ, self.next_id)
				.bytes(&handle)
				.u64(offset)
				.u32(CHUNK as u32)
				.finish();
			let (kind, payload) = Self::split_packet(self.call(request).await?)?;
			match kind {
				FXP_DATA => {
					let mut reader = Reader::new(&payload);
					let data = reader.bytes()?;
					if data.is_empty() {
						break;
					}
					file.write_all(&data)
						.await
						.map_err(|error| SshError::Sftp(format!("写本地文件失败：{error}")))?;
					offset += data.len() as u64;
					total += data.len() as u64;
				},
				FXP_STATUS => {
					let mut reader = Reader::new(&payload);
					let code = reader.u32()?;
					if code == FX_EOF {
						break;
					}
					let text = reader.str().unwrap_or_default();
					let _ = self.close_handle(&handle).await;
					return Err(SshError::Sftp(format!("下载 {remote} 失败：{text}")));
				},
				other => {
					let _ = self.close_handle(&handle).await;
					return Err(SshError::Sftp(format!("read 收到意外响应 {other}")));
				},
			}
		}
		file.flush()
			.await
			.map_err(|error| SshError::Sftp(format!("落盘失败：{error}")))?;
		self.close_handle(&handle).await?;
		Ok(total)
	}

	/// 上传：流式写入远端，返回字节数。
	pub async fn upload(&mut self, local: &Path, remote: &str) -> Result<u64> {
		let mut file = tokio::fs::File::open(local).await.map_err(|error| {
			SshError::Sftp(format!("打开本地文件 {} 失败：{error}", local.display()))
		})?;
		let handle = self
			.open_handle(remote, FXF_WRITE | FXF_CREAT | FXF_TRUNC)
			.await?;

		let mut offset = 0u64;
		let mut chunk = vec![0u8; CHUNK];
		loop {
			let read = file
				.read(&mut chunk)
				.await
				.map_err(|error| SshError::Sftp(format!("读本地文件失败：{error}")))?;
			if read == 0 {
				break;
			}
			let request = Writer::new(FXP_WRITE, self.next_id)
				.bytes(&handle)
				.u64(offset)
				.bytes(&chunk[..read])
				.finish();
			let (kind, payload) = Self::split_packet(self.call(request).await?)?;
			if kind != FXP_STATUS {
				let _ = self.close_handle(&handle).await;
				return Err(SshError::Sftp(format!("write 收到意外响应 {kind}")));
			}
			let mut reader = Reader::new(&payload);
			let code = reader.u32()?;
			let text = reader.str().unwrap_or_default();
			if code != FX_OK {
				let _ = self.close_handle(&handle).await;
				Self::check_status(code, &text, remote)?;
			}
			offset += read as u64;
		}
		self.close_handle(&handle).await?;
		Ok(offset)
	}

	/// 删文件。
	pub async fn remove_file(&mut self, path: &str) -> Result<()> {
		let request = Writer::new(FXP_REMOVE, self.next_id).str(path).finish();
		self.expect_ok(request, path).await
	}

	/// 删空目录（非空会失败——这是 SFTP 的语义，界面据此提示「目录非空」）。
	pub async fn remove_dir(&mut self, path: &str) -> Result<()> {
		let request = Writer::new(FXP_RMDIR, self.next_id).str(path).finish();
		self.expect_ok(request, path).await
	}

	/// 建目录。
	pub async fn mkdir(&mut self, path: &str) -> Result<()> {
		let request = Writer::new(FXP_MKDIR, self.next_id)
			.str(path)
			.empty_attrs()
			.finish();
		self.expect_ok(request, path).await
	}

	/// 重命名 / 移动。目标已存在时由服务端决定（OpenSSH 会覆盖文件）。
	pub async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
		let request = Writer::new(FXP_RENAME, self.next_id)
			.str(from)
			.str(to)
			.finish();
		self.expect_ok(request, from).await
	}

	/// 发一个「只关心成功/失败」的请求。
	async fn expect_ok(&mut self, request: Vec<u8>, path: &str) -> Result<()> {
		let (kind, payload) = Self::split_packet(self.call(request).await?)?;
		if kind != FXP_STATUS {
			return Err(SshError::Sftp(format!("操作 {path} 收到意外响应 {kind}")));
		}
		let mut reader = Reader::new(&payload);
		let code = reader.u32()?;
		let text = reader.str().unwrap_or_default();
		Self::check_status(code, &text, path)
	}

	/// 远端内部复制一个文件（读出来再写回去）。
	///
	/// 服务端有 `copy-data` 扩展时本可以一次完成，但 OpenSSH 默认不提供——
	/// 所以走「读 + 写」，大文件会慢，界面上按普通传输对待。
	pub async fn copy_file(&mut self, from: &str, to: &str) -> Result<u64> {
		let source = self.open_handle(from, FXF_READ).await?;
		let target = self
			.open_handle(to, FXF_WRITE | FXF_CREAT | FXF_TRUNC)
			.await?;
		let mut offset = 0u64;
		loop {
			let request = Writer::new(FXP_READ, self.next_id)
				.bytes(&source)
				.u64(offset)
				.u32(CHUNK as u32)
				.finish();
			let (kind, payload) = Self::split_packet(self.call(request).await?)?;
			match kind {
				FXP_DATA => {
					let data = Reader::new(&payload).bytes()?;
					if data.is_empty() {
						break;
					}
					let request = Writer::new(FXP_WRITE, self.next_id)
						.bytes(&target)
						.u64(offset)
						.bytes(&data)
						.finish();
					let (kind, payload) = Self::split_packet(self.call(request).await?)?;
					if kind != FXP_STATUS {
						let _ = self.close_handle(&source).await;
						let _ = self.close_handle(&target).await;
						return Err(SshError::Sftp(format!("复制写入失败：响应 {kind}")));
					}
					let mut reader = Reader::new(&payload);
					let code = reader.u32()?;
					if code != FX_OK {
						let text = reader.str().unwrap_or_default();
						let _ = self.close_handle(&source).await;
						let _ = self.close_handle(&target).await;
						Self::check_status(code, &text, to)?;
					}
					offset += data.len() as u64;
				},
				FXP_STATUS => {
					let mut reader = Reader::new(&payload);
					let code = reader.u32()?;
					if code == FX_EOF {
						break;
					}
					let text = reader.str().unwrap_or_default();
					let _ = self.close_handle(&source).await;
					let _ = self.close_handle(&target).await;
					return Err(SshError::Sftp(format!("复制 {from} 失败：{text}")));
				},
				other => {
					let _ = self.close_handle(&source).await;
					let _ = self.close_handle(&target).await;
					return Err(SshError::Sftp(format!("复制读到意外响应 {other}")));
				},
			}
		}
		self.close_handle(&source).await?;
		self.close_handle(&target).await?;
		Ok(offset)
	}
}

/// 拼路径，避免出现 `//`。
pub fn join_path(dir: &str, name: &str) -> String {
	if dir.is_empty() || dir == "/" {
		format!("/{name}")
	} else if dir.ends_with('/') {
		format!("{dir}{name}")
	} else {
		format!("{dir}/{name}")
	}
}

/// 上一级目录。根目录返回 `None`（界面上「上一级」按钮据此置灰）。
#[must_use]
pub fn parent_of(path: &str) -> Option<String> {
	let trimmed = path.trim_end_matches('/');
	if trimmed.is_empty() {
		return None;
	}
	match trimmed.rfind('/') {
		Some(0) => Some("/".to_string()),
		Some(index) => Some(trimmed[..index].to_string()),
		None => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn attrs_reader_stops_exactly_after_the_attributes() {
		// flags = SIZE | PERMISSIONS，后面再接一个字符串：游标必须正好停在字符串前
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&(ATTR_SIZE | ATTR_PERMISSIONS).to_be_bytes());
		bytes.extend_from_slice(&1234u64.to_be_bytes());
		bytes.extend_from_slice(&0o100_644u32.to_be_bytes());
		bytes.extend_from_slice(&2u32.to_be_bytes());
		bytes.extend_from_slice(b"ok");

		let mut reader = Reader::new(&bytes);
		let attrs = reader.attrs().expect("解析属性失败");
		assert_eq!(attrs.size, Some(1234));
		assert_eq!(attrs.permissions, Some(0o100_644));
		// 目录列表连着读很多组属性，错一个字节后面全乱
		assert_eq!(reader.str().expect("后续字符串"), "ok");
	}

	#[test]
	fn attrs_reader_skips_extended_fields() {
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&ATTR_EXTENDED.to_be_bytes());
		bytes.extend_from_slice(&1u32.to_be_bytes());
		bytes.extend_from_slice(&3u32.to_be_bytes());
		bytes.extend_from_slice(b"foo");
		bytes.extend_from_slice(&3u32.to_be_bytes());
		bytes.extend_from_slice(b"bar");
		bytes.extend_from_slice(&7u32.to_be_bytes());

		let mut reader = Reader::new(&bytes);
		reader.attrs().expect("应能跳过扩展字段");
		assert_eq!(reader.u32().expect("后续整数"), 7);
	}

	#[test]
	fn truncated_attributes_are_an_error_not_a_panic() {
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&ATTR_SIZE.to_be_bytes());
		// 少给 4 个字节
		bytes.extend_from_slice(&[0u8; 4]);
		let mut reader = Reader::new(&bytes);
		assert!(reader.attrs().is_err(), "截断的报文要报错，不能越界读");
	}

	#[test]
	fn mode_text_matches_ls_output() {
		let dir = DirEntry {
			name: "d".into(),
			path: "/tmp/d".into(),
			is_dir: true,
			size: 0,
			mtime: None,
			permissions: Some(0o040_755),
		};
		assert_eq!(dir.mode_text(), "drwxr-xr-x");

		let file = DirEntry {
			name: "f".into(),
			path: "/tmp/f".into(),
			is_dir: false,
			size: 10,
			mtime: None,
			permissions: Some(0o100_600),
		};
		assert_eq!(file.mode_text(), "-rw-------");

		let link = DirEntry {
			name: "l".into(),
			path: "/tmp/l".into(),
			is_dir: false,
			size: 3,
			mtime: None,
			permissions: Some(0o120_777),
		};
		assert_eq!(link.mode_text(), "lrwxrwxrwx");
		assert!(link.is_symlink());
	}

	#[test]
	fn join_and_parent_handle_root_and_trailing_slash() {
		assert_eq!(join_path("/", "etc"), "/etc");
		assert_eq!(join_path("", "etc"), "/etc");
		assert_eq!(join_path("/var/", "log"), "/var/log");
		assert_eq!(join_path("/var", "log"), "/var/log");

		assert_eq!(parent_of("/var/log"), Some("/var".to_string()));
		assert_eq!(parent_of("/var"), Some("/".to_string()));
		assert_eq!(parent_of("/"), None);
		assert_eq!(parent_of("/var/log/"), Some("/var".to_string()));
	}
}
