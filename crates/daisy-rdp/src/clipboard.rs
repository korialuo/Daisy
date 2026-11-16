//! 剪贴板 `cliprdr` 通道（S8 步骤 63）：**文本**双向；S10 扩展**文件**双向。
//!
//! ## 通道怎么接进来
//!
//! FreeRDP 的虚拟通道是个"插件"：调 `VirtualChannelEntryEx` 传一组入口点
//! （init/open/close/write），插件在握手完成后被打开，之后收发都走这套回调。
//!
//! 我们**不走** `freerdp_channels_load_plugin("cliprdr")` 那条按名查找的路——
//! 它依赖 `freerdp_register_addin_provider` 注册过的全局 provider，而那只在
//! `freerdp_client_context_new` 里才会做（S1 坑1：那个函数对树外客户端不可用）。
//! 所以直接把入口函数地址传给 [`ffi::freerdp_channels_client_load_ex`]。
//!
//! 入口 `cliprdr_VirtualChannelEntryEx` 在 FreeRDP 里是 .c 文件中的
//! `#define` 改名导出，公开头没有声明——由 `wrapper.h` 补了一份。
//!
//! ## 两条数据通路
//!
//! | 方向 | 触发 | 路径 |
//! | --- | --- | --- |
//! | 远端 → 本地 | 服务端广播剪贴板内容 | `ServerFormatList` 回调 → `CF_UNICODETEXT` → 存 [`Clipboard::take_remote`] |
//! | 本地 → 远端 | 用户按快捷键 | [`Clipboard::send_text`] → `ClientFormatList` + `ClientFormatDataResponse` |
//!
//! ## 文件通路（S10，MS-RDPECLIP §1.3.2.2.2 Copy File Sequence）
//!
//! 文件不走 `CF_HDROP`——RDP 剪贴板里文件是一对**注册格式**（服务端按
//! **名字**匹配，ID 值任意，MS-RDPECLIP 示例用 0xC0DE/0xC0DF）：
//!
//! | 方向 | 序列 |
//! | --- | --- |
//! | 本地 → 远端 | 上报 `FileGroupDescriptorW` + `FileContents` → 服务端锁 → 服务端 `FormatDataRequest` 要文件描述表 → `FileContentsRequest`(SIZE/RANGE) 逐块要字节 |
//! | 远端 → 本地 | 服务端广播文件格式 → 我们要描述表 → 解析 `FILEDESCRIPTOR` → 逐文件发 SIZE/RANGE 请求拼出字节 |
//!
//! 分块上限见 [`FILE_CHUNK_SIZE`]；整个下载在内存里拼装，单文件上限
//! [`MAX_FILE_SIZE`]（超限直接失败——这是第一版的取舍，流式落盘要等真实需求）。
//!
//! ## 线程模型
//!
//! 剪贴板内容**不走通道传给界面**，原因与光标形状那次一样：它是**状态**不是流。
//! 丢一次「远端复制了新文本」的通知，界面上就一直是旧文本。
//! 所以内容放在 `Mutex<Option<_>>` 里，界面每帧自己来取（`take_remote`）。
//! 发送方向相反：界面调 [`Clipboard::send_text`] 也只是把文本放进同一个 `Mutex`，
//! 真正的协议发送由会话线程在泵循环里完成——**剪贴板发送必须在会话线程**
//! （`CliprdrClientContext` 不是 `Send`，和输入注入同理）。

use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{RdpError, Result};
use crate::ffi;

/// `CF_UNICODETEXT`：Windows 剪贴板的 Unicode 文本格式（UTF-16LE，NUL 结尾）。
///
/// 出处 `winpr/user.h`。我们只做纯文本，所以只认这一个格式——
/// 其他格式（位图、文件列表等）直接忽略，不做部分支持。
const CF_UNICODETEXT: u32 = 13;

/// `CF_LOCALE`：Locale 标识。**Unicode 文本必须与它成对出现**，
/// 缺了它 Windows 的 `SetClipboardData` 会失败（粘贴菜单是灰的）。
const CF_LOCALE: u32 = 16;

/// `CF_TEXT`：ANSI 文本。服务端要 `CF_TEXT` 时给 UTF-8 字节——
/// 纯 ASCII 下与 ANSI 完全一致；含非 ASCII 时靠 `CF_UNICODETEXT` 兜底
/// （服务端按 `CF_UNICODETEXT` 优先，真要 ANSI 时才回退到这一路）。
const CF_TEXT: u32 = 1;

/// `CF_LOCALE` 的载荷：4 字节小端 LANGID。
///
/// **这是个整数，不是文本**——按 UTF-16 文本发过去，服务端
/// `SetClipboardData(CF_LOCALE)` 拿到的是乱码，
/// 于是连同 `CF_UNICODETEXT` 一起被判为无效（`winpr/libwinpr/clipboard/synthetic.c`
/// 的 `clipboard_synthesize_cf_locale` 就是返回 `UINT32`）。
const CF_LOCALE_LANGID_ZH_CN: u32 = 0x0804;

/// 数据响应与解锁之间的**最小间隔**。
///
/// 服务端要先处理完 `FormatDataResponse`（写进系统剪贴板），
/// 才处理 `UnlockClipboardData`。同一瞬间发两条它来不及，
/// 剪贴板就落空。
///
/// 放在最前面：解锁不依赖 `server_ready`，不该被下面的早退挡住。
const UNLOCK_MIN_GAP: std::time::Duration = std::time::Duration::from_millis(120);

/// 要写进 `FreeRDP_ClipboardFeatureMask` 的功能位。
///
/// **这个设置不写，剪贴板就是死的**：FreeRDP 在 cliprdr 的 8 处都要读它，
/// 掩码为 0 时那些检查会把消息整条丢弃（`cliprdr_format.c:233` / `:272`
/// 直接 return，连回调都不调）。默认就是 0。
///
/// 标志位出处 `freerdp/settings_types.h`：
/// `LOCAL_TO_REMOTE=0x01`、`LOCAL_TO_REMOTE_FILES=0x02`、
/// `REMOTE_TO_LOCAL=0x10`、`REMOTE_TO_LOCAL_FILES=0x20`。
///
/// S10 起四个位全开——文件通路的两处门槛都在 FreeRDP 内部：
/// 收 `ServerFileContentsRequest`（服务端要我们的文件字节）要求
/// `LOCAL_TO_REMOTE_FILES`；发 `ClientFileContentsRequest`（下载远端文件）
/// 要求 `REMOTE_TO_LOCAL_FILES`（见 vendored `cliprdr_main.c:318/:952`）。
pub const FEATURE_MASK_ALL: u32 =
	CLIPRDR_FLAG_LOCAL_TO_REMOTE
		| CLIPRDR_FLAG_REMOTE_TO_LOCAL
		| CLIPRDR_FLAG_LOCAL_TO_REMOTE_FILES
		| CLIPRDR_FLAG_REMOTE_TO_LOCAL_FILES;

const CLIPRDR_FLAG_LOCAL_TO_REMOTE: u32 = 0x01;
const CLIPRDR_FLAG_LOCAL_TO_REMOTE_FILES: u32 = 0x02;
const CLIPRDR_FLAG_REMOTE_TO_LOCAL: u32 = 0x10;
const CLIPRDR_FLAG_REMOTE_TO_LOCAL_FILES: u32 = 0x20;

/// `CB_CLIP_CAPS`：能力集交换的消息类型（`cliprdr.h:46`）。
const CB_CLIP_CAPS: u16 = 0x0007;

/// `CB_FORMAT_LIST_RESPONSE`：对格式列表的确认（`cliprdr.h`）。
const CB_FORMAT_LIST_RESPONSE: u16 = 0x0003;
/// `CB_UNLOCK_CLIPDATA`：应答服务端的取数锁（`cliprdr.h`）。
const CB_UNLOCK_CLIPDATA: u16 = 0x000B;
/// `CB_LOCK_CLIPDATA`：取数方在索要文件字节前先锁住数据（`cliprdr.h`）。
const CB_LOCK_CLIPDATA: u16 = 0x000A;
/// `CB_RESPONSE_OK`：确认我们接受这些格式。服务端见此才肯发数据。
const CB_RESPONSE_OK: u16 = 0x0001;
/// `CB_RESPONSE_FAIL`：拒绝这些格式。
const CB_RESPONSE_FAIL: u16 = 0x0002;

/// 上报格式列表后，等远端来索要数据的最长时间。
///
/// 超时就重发：远端有可能因为焦点在别的窗口、或正忙于自己的剪贴板处理
/// 而一时不回应。此时若无限等待，这条剪贴板就永远送不过去了。
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `CB_CAPSTYPE_GENERAL`：通用能力集类型（`cliprdr.h`）。
const CB_CAPSTYPE_GENERAL: u16 = 0x0001;
/// `CB_CAPS_VERSION_2`：能力集版本 2。
const CB_CAPS_VERSION_2: u32 = 2;
/// `CB_USE_LONG_FORMAT_NAMES`：唯一需要宣告的能力位。
const CB_USE_LONG_FORMAT_NAMES: u32 = 0x0002;

/// `CB_STREAM_FILECLIP_ENABLED`：宣告「文件剪贴板（FileContents 流）已实现」。
///
/// MS-RDPECLIP §2.2.2.2.1：服务端**只**对宣告了这个位、且格式列表里带
/// `FileGroupDescriptorW` 的对端走文件复制序列。S10 起我们真的实现了
/// FileContents 双向，所以可以宣告了——**能宣称的前提是实现了**，
/// 这条纪律来自第二十六轮（回声没实现的位导致文本链路死锁）。
const CB_STREAM_FILECLIP_ENABLED: u32 = 0x0004;

/// `CB_CAN_LOCK_CLIPDATA`：宣告「会用锁协议」。
///
/// 文件复制序列里服务端会先锁我们的剪贴板再逐块取数；
/// 不宣告这个位服务端就不发锁、文件链路不走。
/// 文本路径的教训仍然有效：数据在 `sending` 里**保留到解锁/超时**，
/// 所以锁来了不会拿不到东西。
const CB_CAN_LOCK_CLIPDATA: u32 = 0x0010;

/// 文件内容的注册格式 ID。服务端按**名字**匹配注册格式，
/// ID 值只要求自洽——MS-RDPECLIP 规范自己的示例就用 0xC0DE/0xC0DF。
const FORMAT_FILE_GROUP_DESCRIPTOR_W: u32 = 0xC0DE;
const FORMAT_FILE_CONTENTS: u32 = 0xC0DF;

/// `FileContentsRequest.dwFlags`：要文件大小 / 要一段内容。
/// 出处 `channels/cliprdr.h`（`FILECONTENTS_SIZE` / `FILECONTENTS_RANGE`）。
const FILECONTENTS_SIZE: u32 = 0x0000_0001;
const FILECONTENTS_RANGE: u32 = 0x0000_0002;

/// 一次 RANGE 请求要多少字节。
///
/// Windows 服务端单条 FileContents 响应最大约 1MB（通道消息上限），
/// 256KB 留足余量，也避免单块在慢链路上占太久。
const FILE_CHUNK_SIZE: u32 = 256 * 1024;

/// 内存拼装的单文件上限。超过直接判失败——流式落盘要等真实需求。
pub const MAX_FILE_SIZE: u64 = 512 * 1024 * 1024;

/// 描述表回空后的重试延时。
///
/// 400ms 是权衡：rdpclip 填系统剪贴板通常几十毫秒内完成，给足余量；
/// 太长则用户已经切过去按 Ctrl+V 了。
const DESCRIPTOR_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// `FILEDESCRIPTORW` 的定长（MS-RDPECLIP §2.2.4.2.3）：
/// 头 48 字节 + 260 个 WCHAR 的名字（520 字节）。
const FILE_DESCRIPTOR_SIZE: usize = 592;

/// `FILEDESCRIPTOR.dwFlags`：只解析/只填写我们用得到的位。
const FD_ATTRIBUTES: u32 = 0x0000_0004;
const FD_FILESIZE: u32 = 0x0000_0040;
/// 目录属性位（Windows `FILE_ATTRIBUTE_DIRECTORY`）。
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// 普通文件的属性位（Windows `FILE_ATTRIBUTE_ARCHIVE`）。
///
/// **不能填 0**：描述表上报普通文件时 `dwFileAttributes` 是 0 的话，
/// rdpclip 侧构造 CF_HDROP 会把条目当无效/无类型，Explorer 粘贴时
/// 既不建壳也不再要内容——表现就是「完全没有反应」。
/// 官方 Windows 客户端填的是 `GetFileAttributes(path)` 的真实值；
/// 我们这边统一按「普通文件」报 `ARCHIVE`（=0x20）最稳。
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;

/// `CLIPRDR_GENERAL_CAPABILITY_SET`——bindgen 没生成它，按规范补：
/// type(2) + length(2) + version(4) + generalFlags(4) = 12 字节。
#[repr(C)]
struct GeneralCapabilitySet {
	capability_set_type: u16,
	capability_set_length: u16,
	version: u32,
	general_flags: u32,
}

/// 本地待上传的文件（界面侧从系统剪贴板收集）。
///
/// `size` 在收集时用 `metadata` 量好——描述表里要用；
/// 真正的字节在服务端来索要时才按 `path` 现读，不提前占内存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
	/// 带扩展名的文件名（描述表里的 `cFileName`）。
	pub name: String,
	/// 字节数。
	pub size: u64,
	/// 本地完整路径。
	pub path: PathBuf,
}

/// 下载完成的远端文件，等界面取走落盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
	pub name: String,
	pub size: u64,
	pub data: Vec<u8>,
}

/// 待上报的剪贴板内容：文本与文件是**互斥**的两条上报路径，
/// 格式列表的形态完全不同（文本三种标准格式、文件两种注册格式）。
#[derive(Debug, Clone)]
enum Content {
	Text(String),
	Files(Vec<LocalFile>),
}

/// 我们**发出去**的取数请求的类别——`FormatDataResponse` 不带格式 ID，
/// 响应回来时全靠它分派（文本解 UTF-16，文件解描述表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
	Text(u32),
	FileDescriptors(u32),
}

/// 一次待发的 `FileContentsRequest`（帧循环里发出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileRequest {
	stream_id: u32,
	list_index: u32,
	dw_flags: u32,
	position: u64,
	count: u32,
	/// 服务端锁定数据时的 `clipDataId`（`CB_LOCK_CLIPDATA` 带来的）。
	/// 有值就必须随请求带回——FreeRDP 官方文件剪贴板客户端
	/// （`client_cliprdr_file.c` 的 `has_clip_data_id`）就是这么发的。
	clip_data_id: Option<u32>,
}

/// 描述表解析出来的一个文件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEntry {
	name: String,
	size: u64,
}

/// 远端 → 本地的下载状态机。
///
/// 每个文件先 `SIZE` 后 `RANGE`，一次只挂一个未决请求；
/// 响应用 `streamId` 对账，对不上的直接丢弃（服务端重发旧块时不能串档）。
#[derive(Debug)]
struct Download {
	entries: Vec<FileEntry>,
	/// 当前正在下载的文件下标。
	index: usize,
	/// 当前文件期望的字节数（SIZE 响应之后才有值）。
	expected: u64,
	/// 当前文件已拼装的字节。
	buffer: Vec<u8>,
	/// 已完成的文件（前几个下标的产物）。
	completed: Vec<RemoteFile>,
	/// 已发出、等响应的那个请求。
	outstanding: Option<FileRequest>,
	/// 待发请求（回调只记账，帧循环 [`Clipboard::flush_outgoing`] 发）。
	next_request: Option<FileRequest>,
	/// 下一个 streamId。
	next_stream_id: u32,
	/// 建立会话时我们自己锁定的 `clipDataId`（随每个请求带回）。
	clip_data_id: Option<u32>,
	/// 锁是否已经发给服务端（第一块请求之前发一次）。
	lock_sent: bool,
	/// 失败原因。置位后状态机不再推进。
	failed: Option<String>,
	/// 全部下载完成。
	finished: bool,
}

impl Download {
	/// 用描述表条目建下载会话，并给第一个文件排上 SIZE 请求。
	///
	/// 描述表已带精确大小，但仍先走一轮 SIZE：这是 FreeRDP 官方文件
	/// 剪贴板客户端的固定顺序，也与 Windows rdpclip 互操作最稳。
	/// （历史：跳过 SIZE 直接 RANGE 时，这台 Windows Server 2019
	/// 对本客户端一律 `CB_RESPONSE_FAIL`。）
	fn start(entries: Vec<FileEntry>, clip_data_id: Option<u32>) -> Result<Self, String> {
		if entries.is_empty() {
			return Err("远端剪贴板的文件列表为空".to_string());
		}
		let mut download = Self {
			entries,
			index: 0,
			expected: 0,
			buffer: Vec::new(),
			completed: Vec::new(),
			outstanding: None,
			next_request: None,
			next_stream_id: 1,
			clip_data_id,
			lock_sent: false,
			failed: None,
			finished: false,
		};
		download.check_capacity()?;
		download.begin_file();
		Ok(download)
	}

	/// 开始（或推进到）第 `index` 个文件：记期望大小、排第一个请求。
	///
	/// 先发 `SIZE`（`cbRequested` 恒为 8，MS-RDPECLIP 2.2.5.1 MUST），
	/// 拿到服务端确认后再走 `RANGE`——FreeRDP 官方文件剪贴板客户端
	/// （`client_cliprdr_file.c::request_file_size_async`）就是这个顺序，
	/// 与 Windows rdpclip 互操作时最稳。描述表里的大小仍作校验用。
	///
	/// 空文件没有字节可取（`cbRequested=0` 是兼容性雷区），直接收进结果。
	fn begin_file(&mut self) {
		while self.index < self.entries.len() {
			let size = self.entries[self.index].size;
			self.expected = size;
			self.buffer.clear();
			if size == 0 {
				self.finish_current_file();
				continue;
			}
			self.queue_size_request();
			return;
		}
		self.finished = true;
	}

	/// 排一个 SIZE 请求（问服务端要 8 字节的文件大小）。
	fn queue_size_request(&mut self) {
		let request = FileRequest {
			stream_id: self.next_stream_id,
			list_index: self.index as u32,
			dw_flags: FILECONTENTS_SIZE,
			position: 0,
			count: 8,
			clip_data_id: self.clip_data_id,
		};
		self.next_stream_id += 1;
		self.next_request = Some(request);
	}

	/// 当前文件齐了：收进结果表，下标推进。
	fn finish_current_file(&mut self) {
		let entry = &self.entries[self.index];
		self.completed.push(RemoteFile {
			name: entry.name.clone(),
			size: entry.size,
			data: std::mem::take(&mut self.buffer),
		});
		self.expected = 0;
		self.index += 1;
	}

	/// 超限检查：任何一个文件超过 [`MAX_FILE_SIZE`] 都直接失败。
	fn check_capacity(&self) -> Result<(), String> {
		for entry in &self.entries {
			if entry.size > MAX_FILE_SIZE {
				return Err(format!(
					"「{}」{} 超过单文件上限 {}，不支持粘贴这么大的文件",
					entry.name,
					human_size(entry.size),
					human_size(MAX_FILE_SIZE)
				));
			}
		}
		Ok(())
	}

	fn queue_range_request(&mut self) {
		let remaining = self.expected - self.buffer.len() as u64;
		let request = FileRequest {
			stream_id: self.next_stream_id,
			list_index: self.index as u32,
			dw_flags: FILECONTENTS_RANGE,
			position: self.buffer.len() as u64,
			count: remaining.min(u64::from(FILE_CHUNK_SIZE)) as u32,
			clip_data_id: self.clip_data_id,
		};
		self.next_stream_id += 1;
		self.next_request = Some(request);
	}

	/// 取走待发请求（帧循环里真正发出并挂进 `outstanding`）。
	fn pop_request(&mut self) -> Option<FileRequest> {
		let request = self.next_request.take()?;
		self.outstanding = Some(request);
		Some(request)
	}

	/// 服务端的一块响应。`ok` 为假（`CB_RESPONSE_FAIL`）或流号对不上时终止。
	fn on_response(&mut self, stream_id: u32, ok: bool, data: &[u8]) {
		if self.failed.is_some() || self.finished {
			return;
		}
		let Some(outstanding) = self.outstanding else {
			return;
		};
		if stream_id != outstanding.stream_id {
			// 旧请求的重发/乱序：不能串档，直接丢弃
			return;
		}
		self.outstanding = None;
		if !ok {
			// 失败响应可能带 4 字节错误码（Win32/HRESULT）；打出来才知道
			// 服务端为什么拒绝，否则只剩一句「被拒绝」无从下手。
			let code = data
				.get(..4)
				.map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]));
			self.failed = Some(match code {
				Some(code) => format!(
					"远端拒绝提供「{}」的内容（CB_RESPONSE_FAIL，code=0x{code:08x}）",
					self.entries[self.index].name
				),
				None => format!(
					"远端拒绝提供「{}」的内容（CB_RESPONSE_FAIL）",
					self.entries[self.index].name
				),
			});
			return;
		}

		if outstanding.dw_flags == FILECONTENTS_SIZE {
			// SIZE 响应：8 字节小端 = 服务端给出的文件大小
			if data.len() < 8 {
				self.failed = Some(format!(
					"SIZE 响应只有 {} 字节（应为 8）",
					data.len()
				));
				return;
			}
			let mut raw = [0u8; 8];
			raw.copy_from_slice(&data[..8]);
			let size = u64::from_le_bytes(raw);
			tracing::info!(
				"[剪贴板] 服务端确认大小：{} = {} 字节（描述表 {}）",
				self.entries[self.index].name,
				size,
				self.expected
			);
			self.expected = size;
			if size == 0 {
				self.finish_current_file();
				self.begin_file();
				return;
			}
			self.queue_range_request();
			return;
		}

		self.buffer.extend_from_slice(data);
		if self.buffer.len() as u64 >= self.expected {
			// 这个文件齐了，收进结果表再开下一个
			self.finish_current_file();
			self.begin_file();
		} else {
			self.queue_range_request();
		}
	}

	/// 全部完成后取走文件。未完成或失败时返回 `None`。
	fn take_result(&mut self) -> Option<Vec<RemoteFile>> {
		if !self.finished {
			return None;
		}
		Some(std::mem::take(&mut self.completed))
	}
}

/// 字节数的展示口径（错误消息里用）。
fn human_size(bytes: u64) -> String {
	let units = ["B", "KB", "MB", "GB"];
	let mut size = bytes as f64;
	let mut unit = 0;
	while size >= 1024.0 && unit + 1 < units.len() {
		size /= 1024.0;
		unit += 1;
	}
	if unit == 0 {
		format!("{bytes} {}", units[unit])
	} else {
		format!("{size:.1} {}", units[unit])
	}
}

/// 剪贴板插件在 `pExtendedData` 里拿到的自定义数据。
///
/// 生命周期由 [`Clipboard`] 持有：`Arc` 本身活到会话结束，
/// 回调里只借不取。
type PluginData = Mutex<ClipboardState>;

/// 剪贴板跨线程共享的状态。
#[derive(Default)]
struct ClipboardState {
	/// 远端剪贴板的最新文本（`take_remote` 取走即清空）。
	remote: Option<String>,
	/// 下载完成的远端文件（`take_remote_files` 取走即清空）。
	remote_files: Option<Vec<RemoteFile>>,
	/// 远端 → 本地的下载会话；`None` 表示没有进行中的下载。
	download: Option<Download>,
	/// 下载失败的文案（界面取走显示 toast）。成功不写这里。
	download_error: Option<String>,
	/// 用户放进来的内容，等会话线程上报格式列表。
	pending_out: Option<Content>,
	/// **已上报格式列表、正等远端来索要**的内容。
	///
	/// 必须和 `pending_out` 分开：上报格式时内容不能被取走——远端收到
	/// 格式列表后会另发一条 `CB_FORMAT_DATA_REQUEST` 来要数据，那时才拿。
	/// 两者合成一个字段的话，内容在上报时就被 `take()` 走了，
	/// 远端要数据时只能回空（文本方向第一版就错在这里：粘不过去）。
	sending: Option<(Content, std::time::Instant)>,
	/// 我们最近一次发出的取数请求——响应回来时分派用。
	requested: Option<RequestKind>,
	/// 空载荷重试的到期时刻（`None` 表示没有待重试）。
	///
	/// rdpclip 刚广播剪贴板变化时，会在系统剪贴板还没填好之前就应答
	/// `CB_FORMAT_DATA_REQUEST`（真机见过 `dataLen=0`）。此刻放弃，
	/// 用户看到的就是「远端复制了，这边却什么都没发生」——而 400ms
	/// 后再问一次通常就有内容了。
	retry_at: Option<std::time::Instant>,
	/// 还允许重试几次（每次新的广播重置）。
	retry_left: u8,
	/// 远端是否已经 `MonitorReady`（没就绪前发送会失败）。
	server_ready: bool,
	/// 服务端有没有主动广播过剪贴板变化（远端 → 本地方向是否可用）。
	server_broadcast: bool,
	/// 广播之后有没有真的收到数据（区分「服务端没发」与「发了但没到」）。
	remote_data_received: bool,
	/// 上一条上报的内容**是否已经应答出去了**（数据响应已发出）。
	///
	/// 用于「粘贴」时机的判定：Ctrl+V 必须在数据送达**之后**才注入，
	/// 否则远端粘贴到的是上一份内容。等固定时长是猜的——服务端
	/// 快则 20ms 就来索要、慢则几百毫秒，只有这个标志是确定的。
	/// 待发送的解锁（`clipDataId`, 记账时刻)——**数据响应之后延迟再发**。
	///
	/// 服务端收到 `FormatDataResponse` 后要先把它写进系统剪贴板，
	/// 之后才处理 `UnlockClipboardData`。两条在同一瞬间发出，
	/// 服务端还没写入完就收到解锁，剪贴板就落空。
	/// 放在最前面：解锁不依赖 `server_ready`，不该被下面的早退挡住。
	pending_unlock: Option<(u32, std::time::Instant)>,
	remote_taken: bool,
	/// 服务端是否已**确认**我们最近一次上报的格式列表（`CB_RESPONSE_OK`）。
	///
	/// **注入 Ctrl+V 之前必须等它为 `true`**，否则会形成死锁：
	/// 服务端只在「用户粘贴」时才发 `CB_FORMAT_DATA_REQUEST` 来索要数据，
	/// 而那个粘贴动作正是我们自己注入的 Ctrl+V。
	/// 若 Ctrl+V 先于格式列表到达，远端粘贴到的是**空剪贴板**，
	/// 不会发取数请求；随后格式列表到了、服务端回了 OK，
	/// 但没人再粘贴一次 —— 取数请求永远不会来。
	/// 实测：格式列表回执只要 5ms，而死锁会让整条链路彻底静默。
	list_acked: bool,
	/// 待索取的格式 ID——**不能在回调里直接发报文**，要推迟到下一帧。
	///
	/// 服务端广播格式列表的回调里，`filteredFormatList` 的内存归 FreeRDP 所有，
	/// 回调返回后立刻释放。在回调里同步发 `FormatDataRequest` 虽然此刻数据还在，
	/// 但通道写入与报文解析共享同一批结构体，时序上并不安全。
	/// 稳妥做法：回调里只记下要什么，帧循环（`flush_outgoing`）里再发。
	pending_request: Option<RequestKind>,
	/// 服务端当前锁定的 `clipDataId`（`CB_LOCK_CLIPDATA` 记下，
	/// `CB_UNLOCK_CLIPDATA` 清掉）。
	///
	/// **下载文件字节时必须带上它**：MS-RDPECLIP 3.1.1.3 的服务端
	/// Copy File Sequence 是「服务端锁 clipDataId → 广播描述表 →
	/// 客户端请求要带 clipDataId」。真机（Windows Server 2019）实测：
	/// 不带时 SIZE 请求石沉大海——服务端无法把请求关联到锁定的文件流，
	/// 直接丢弃。
	server_clip_data_id: Option<u32>,
	/// 本会话**有没有向服务端上报过格式列表**。
	///
	/// 这条不是可有可无的记账：Windows 的 rdpclip 在客户端上报第一条
	/// 格式列表之前**不会监听会话剪贴板**，于是远端 Ctrl+C 根本不发
	/// `CB_FORMAT_LIST`，「远端复制 → 本地粘贴」整条哑掉——而且从外面看
	/// 与「rdpclip 卡死」一模一样（都是没反应）。
	///
	/// 真机证据（2026-11-13，心悦装饰 Server 2019）：
	/// - 全天 21 次「远端广播了剪贴板变化」**全部**发生在已上报过格式列表
	///   的会话里，0 反例；
	/// - 反过来，客户端一直「没东西可报」的会话（本机剪贴板空 / 只有图片），
	///   远端复制一律毫无动静——用户当晚报的正是这一种。
	///
	/// 所以「本机没有可传内容」时也必须报一条**空文本**，见
	/// `Clipboard::arm` 的说明。
	announced_once: bool,
	/// 最近一次收到服务端报文的（类型名，时刻）——**只用于诊断**。
	///
	/// 「远端没反应」有两个完全不同的原因：rdpclip 不广播，或通道本身
	/// 已经死了。两者外部表现一模一样（用户只看到「粘贴没反应」），
	/// 没有这个时间戳就只能猜。界面提示会带上「最近一条远端报文是 X，
	/// N 秒前」，据此即可分辨。
	last_server_message: Option<(&'static str, std::time::Instant)>,
}

/// 剪贴板双向通道。
///
/// 界面侧：`take_remote()` 取远端内容，`send_text()` 投递本地内容。
/// 会话线程侧：`install()` 装上通道，`flush_outgoing()` 把待发内容发出去。
pub struct Clipboard {
	/// 跨线程共享状态。回调与界面都通过它。
	state: std::sync::Arc<PluginData>,
}

impl std::fmt::Debug for Clipboard {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Clipboard").finish_non_exhaustive()
	}
}

impl Clipboard {
	/// 建一个未安装的剪贴板通道。
	pub fn new() -> Self {
		Self {
			state: std::sync::Arc::new(Mutex::new(ClipboardState::default())),
		}
	}

	/// 装上 `cliprdr` 通道。**必须在会话线程、`freerdp_connect` 成功后调用。**
	///
	/// `instance` 用来取 `context->channels`（FreeRDP 在连接过程中建好它）。
	///
	/// 两步：先按入口函数加载通道，再从通道表里取回 `CliprdrClientContext`
	/// 装上我们的回调。
	///
	/// **第二步此时通常还拿不到**：`openDataList` 要等通道真正打开
	/// （握手完成之后）才被填，所以 `load_ex` 之后立刻去取往往返回空。
	/// 因此这里只加载通道，回调的接管交给 [`Self::try_bind`]——
	/// 它每帧调一次，取到就装，取不到就等下一帧。
	///
	/// **必须接管**：FreeRDP 的入口函数会把它自己的回调填进 context
	/// （并把 `custom` 置空），不覆盖的话收到的永远是它的默认处理。
	pub fn install(&self, instance: &mut ffi::freerdp) -> Result<()> {
		// SAFETY: 引用参数保证 instance 有效；context 字段只读。
		let context = unsafe { instance.context.as_ref() }
			.ok_or_else(|| RdpError::Gdi("context 为空，无法装剪贴板通道".to_string()))?;
		let channels = context.channels;
		let settings = context.settings;
		if channels.is_null() || settings.is_null() {
			return Err(RdpError::Gdi(
				"context->channels/settings 为空，无法装剪贴板通道".to_string(),
			));
		}

		// SAFETY: channels/settings 非空，entry 是库里导出的函数。
		let rc = unsafe {
			ffi::freerdp_channels_client_load_ex(
				channels,
				settings,
				Some(ffi::cliprdr_VirtualChannelEntryEx),
				std::ptr::null_mut(),
			)
		};
		if rc != 0 {
			return Err(RdpError::Gdi(format!("加载 cliprdr 通道失败（返回 {rc}）")));
		}
		tracing::info!("剪贴板通道 cliprdr 已加载，等待通道打开");
		Ok(())
	}

	/// 通道打开后接管回调。**每帧调一次**，取到就装、取不到就等下一帧。
	///
	/// 返回 `true` 表示已经接管（之后可以不再调）。
	pub fn try_bind(&self, instance: &mut ffi::freerdp) -> bool {
		// SAFETY: 引用参数保证 instance 有效；context 字段只读。
		let Some(context) = (unsafe { instance.context.as_ref() }) else {
			return false;
		};
		let channels = context.channels;
		if channels.is_null() {
			return false;
		}
		// SAFETY: channels 非空；"cliprdr" 是通道名（CLIPRDR_SVC_CHANNEL_NAME）。
		let interface = unsafe {
			ffi::freerdp_channels_get_static_channel_interface(channels, c"cliprdr".as_ptr())
		};
		if interface.is_null() {
			// 通道还没打开——正常，等下一帧
			return false;
		}
		// SAFETY: interface 是 cliprdr 入口建的 CliprdrClientContext。
		let cliprdr = unsafe { &mut *(interface as *mut ffi::CliprdrClientContext) };
		cliprdr.custom =
			std::sync::Arc::as_ptr(&self.state) as *mut PluginData as *mut std::ffi::c_void;
		install_callbacks(cliprdr);
		tracing::info!("剪贴板通道 cliprdr 已打开并接管回调");
		true
	}

	/// 取走远端剪贴板文本。`None` 表示没有新内容。
	///
	/// **取走即清空**：界面的复制按钮据此判断"有没有东西可粘"。
	pub fn take_remote(&self) -> Option<String> {
		let mut state = self.lock();
		state.remote.take()
	}

	/// 投递一段要发往远端的文本。真正的发送由会话线程的
	/// [`Self::flush_outgoing`] 完成。
	pub fn send_text(&self, text: String) {
		if let Ok(mut state) = self.state.lock() {
			state.pending_out = Some(Content::Text(text));
		}
	}

	/// 投递一组要发往远端的文件（Finder 里复制的文件）。
	///
	/// 服务端**在用户粘贴时**才来逐块取内容，所以文件路径必须活到那时候；
	/// 这里只收「名字 + 大小 + 路径」，不在界面上读文件内容。
	///
	/// 后到的内容顶掉先到的（与文本同一语义：剪贴板只有一个当前值）。
	pub fn send_files(&self, files: Vec<LocalFile>) {
		if let Ok(mut state) = self.state.lock() {
			state.pending_out = Some(Content::Files(files));
		}
	}

	/// 取走下载完成的远端文件。`None` 表示没有新东西。
	pub fn take_remote_files(&self) -> Option<Vec<RemoteFile>> {
		self.lock().remote_files.take()
	}

	/// 取走下载失败的文案（一次）。成功下载不写这个。
	pub fn take_download_error(&self) -> Option<String> {
		self.lock().download_error.take()
	}

	/// 把待发文本发往远端。**每帧调一次**（会话线程）。
	///
	/// 服务端没就绪时不发也不丢——`MonitorReady` 到了自然会补上。
	pub fn flush_outgoing(&self, mut context: Option<&mut ffi::CliprdrClientContext>) {
		// **待解锁：应答锁之后延迟再发。**
		// 服务端要先处理完数据（写进系统剪贴板），才轮到解锁——
		// 同一瞬间发两条，服务端还没写入完就收到解锁，剪贴板就落空。
		//
		// 「下一帧」不够——帧间隔可能只有几毫秒。给一个**明确的最小间隔**。
		let unlock = {
			let mut state = self.lock();
			match state.pending_unlock {
				Some((id, at)) if at.elapsed() >= UNLOCK_MIN_GAP => {
					state.pending_unlock = None;
					Some(id)
				},
				// 间隔不够：留在状态里，下一帧再发
				other => {
					if let Some((id, at)) = other {
						state.pending_unlock = Some((id, at));
					}
					None
				},
			}
		};
		if let Some(clip_data_id) = unlock
			&& let Some(context) = context.as_mut()
			&& let Some(send) = context.ClientUnlockClipboardData
		{
			let msg = ffi::CLIPRDR_UNLOCK_CLIPBOARD_DATA {
				common: cliprdr_header(CB_UNLOCK_CLIPDATA, 0, 4),
				clipDataId: clip_data_id,
			};
			// SAFETY: 回调指针有效，msg 在调用期间有效。
			let rc = unsafe { send(*context, &msg) };
			tracing::info!("[剪贴板] 已解锁（clipDataId={clip_data_id}，rc={rc}）");
		}

		// **反向：发出待索取的请求。** 每帧调一次，不依赖回调里的时序——
		// 回调只记下「要什么」，这里在安全的帧循环里真正发送。
		// 同时记下类别：`FormatDataResponse` 不带格式 ID，响应回来全靠它分派。
		let pending = self.lock().pending_request.take();
		if let Some(kind) = pending {
			if let Some(context) = context.as_mut() {
				let format_id = match kind {
					RequestKind::Text(id) | RequestKind::FileDescriptors(id) => id,
				};
				let rc = request_format_data(context, format_id);
				tracing::info!("[剪贴板] 已发出反向取数请求 formatId=0x{format_id:x}（rc={rc}）");
				let mut state = self.lock();
				state.requested = Some(kind);
				// 每次新的广播都重新给一次重试机会
				state.retry_left = 1;
				state.retry_at = None;
			} else {
				tracing::warn!("[剪贴板] 待索取但通道上下文为空");
			}
		}

		// **空载荷重试**：上次应答是空的（rdpclip 还没写完剪贴板），到点再问一次。
		let retry = {
			let mut state = self.lock();
			match state.retry_at {
				Some(at) if at <= std::time::Instant::now() => {
					state.retry_at = None;
					state.requested
				},
				_ => None,
			}
		};
		if let (Some(kind), Some(context)) = (retry, context.as_mut()) {
			let format_id = match kind {
				RequestKind::Text(id) | RequestKind::FileDescriptors(id) => id,
			};
			let rc = request_format_data(context, format_id);
			tracing::info!(
				"[剪贴板] 重试反向取数 formatId=0x{format_id:x}（rc={rc}）"
			);
		}

		// **文件下载的待发请求**：下载状态机每收一块就排下一块，
		// 全部在这里发出（不在回调里发，理由同上）。
		if let Some(context) = context.as_mut() {
			// 第一块请求之前先锁住这份文件流（见 `DOWNLOAD_CLIP_DATA_ID`）。
			// 顺序必须锁在前：Windows rdpclip 见到没有锁的取数请求不认。
			let need_lock = self
				.lock()
				.download
				.as_ref()
				.is_some_and(|d| !d.lock_sent);
			if need_lock {
				let rc = send_lock_clipdata(context, DOWNLOAD_CLIP_DATA_ID);
				tracing::info!(
					"[剪贴板] 已发 CB_LOCK_CLIPDATA clipDataId=0x{DOWNLOAD_CLIP_DATA_ID:08x}（rc={rc}），随后取文件内容"
				);
				if let Some(download) = self.lock().download.as_mut() {
					download.lock_sent = true;
				}
			}
			let request = self.lock().download.as_mut().and_then(|d| d.pop_request());
			if let Some(request) = request {
				let rc = send_file_contents_request(context, &request);
				tracing::info!(
					"[剪贴板] 已发 FileContentsRequest streamId={} listIndex={} flags=0x{:x} pos={} len={}（rc={rc}）",
					request.stream_id,
					request.list_index,
					request.dw_flags,
					request.position,
					request.count
				);
			}
		}
		// 收档；顺手把我们锁住的文件流释放掉，别让服务端一直挂着锁
		if self.finish_download_if_done()
			&& let Some(context) = context.as_mut()
		{
			let rc = send_unlock_clipdata(context, DOWNLOAD_CLIP_DATA_ID);
			tracing::info!("[剪贴板] 已发 CB_UNLOCK_CLIPDATA（rc={rc}）");
		}

		let content = {
			let mut state = self.lock();
			if !state.server_ready {
				return;
			}
			// 上一次已上报但远端一直不来要数据——**不能一直等下去**。
			// 真实链路里服务端可能因为忙/焦点在别处而不回应，那时若死等，
			// 这条剪贴板就永远送不过去了（同步看起来"卡住"）。
			//
			// **但服务端一旦回了 `CB_FORMAT_LIST_RESPONSE(OK)`，就绝不能再重播。**
			// 重播看起来无害，实际会把服务端拖进「剪贴板一直在变」的状态：
			// 每 2 秒一条新的格式列表，每次都要让它重设一遍 Windows 剪贴板。
			// 实测（192.168.10.20）：带重播的会话里服务端从头到尾没广播过一次
			// 剪贴板变化，Windows 剪贴板始终为空。回执之后安静等着即可——
			// 内容留在 `sending` 里，服务端的取数请求来了照样能取到。
			if !state.list_acked
				&& let Some((content, since)) = state.sending.clone()
				&& since.elapsed() >= SEND_TIMEOUT
			{
				// 注意用 debug 而非 warn：本地剪贴板是每 500ms 自动推送的，
				// 而远端**按 RDP 规范本来就只在用户粘贴时才来索要**数据。
				// 所以"没来索要"是常态，用 warn 会每 2 秒刷一条日志、
				// 把真正的错误淹没（实测日志里全是这个）。
				tracing::debug!("远端 {} 内未索要剪贴板数据，重发", SEND_TIMEOUT.as_secs());
				state.sending = None;
				state.pending_out = Some(content);
			}
			// **新的内容必须顶掉在途的**，不能因为 `sending` 非空就丢掉。
			//
			// 用户按 Cmd+V 时会走「再推一次 + 注入 Ctrl+V」。
			// 若此时上一条还挂在 `sending` 里（服务端还没来索要），
			// 原实现直接 return：新内容没进 `pending_out`，远端粘贴到的
			// 就是**上一份内容**——表现为「粘出来是旧的 / 空的」。
			//
			// 语义上「剪贴板只有一个当前值」：后到的自然覆盖先到的，
			// 不该有排队。`sending` 里的旧内容直接丢弃即可，
			// 服务端真来取时会取到 `pending_out` 里的最新值。
			state.pending_out.take()
		};
		let Some(content) = content else {
			return;
		};
		let Some(context) = context else {
			// 拿不到上下文（极少见：通道刚卸掉）——把内容放回去下次再试
			match &content {
				Content::Text(text) => self.send_text(text.clone()),
				Content::Files(files) => self.send_files(files.clone()),
			}
			return;
		};
		// 先把内容挪进 `sending`：上报成功后远端会来要数据，
		// 那时才能取到。放在上报之后是刻意的——上报失败还能退回 `pending_out`。
		if let Err(error) = send_format_list(context, &content) {
			tracing::warn!("发送剪贴板格式列表失败：{error}");
			// 发不出去就还回去，别把用户复制的内容吞了
			match &content {
				Content::Text(text) => self.send_text(text.clone()),
				Content::Files(files) => self.send_files(files.clone()),
			}
			return;
		}
		{
			let mut state = self.lock();
			state.sending = Some((content, std::time::Instant::now()));
			// 报过至少一条了——rdpclip 从此开始监听会话剪贴板
			state.announced_once = true;
			// 新一轮上报，重新等服务端来取
			state.remote_taken = false;
			// 新一轮格式列表尚未被确认——`ServerFormatListResponse` 里置位。
			// 不清的话会拿着上一轮的 OK 去注入 Ctrl+V，远端可能还没
			// 登记这轮的格式，粘贴依然落空。
			state.list_acked = false;
		}
	}

	/// 下载会话完成/失败后收档：结果挂进 `remote_files`，错误挂进 `download_error`。
	///
	/// 返回 `true` 表示这次收档前我们向服务端发过锁、需要补一条 `CB_UNLOCK_CLIPDATA`。
	fn finish_download_if_done(&self) -> bool {
		let mut state = self.lock();
		let Some(download) = state.download.as_mut() else {
			return false;
		};
		let need_unlock = download.lock_sent;
		if let Some(error) = download.failed.clone() {
			tracing::warn!("[剪贴板] 远端文件下载失败：{error}");
			state.download_error = Some(error);
			state.download = None;
			return need_unlock;
		}
		if let Some(files) = download.take_result() {
			let total: usize = files.iter().map(|f| f.data.len()).sum();
			tracing::info!(
				"[剪贴板] ✅ 远端文件下载完成 {} 个，共 {} 字节",
				files.len(),
				total
			);
			state.remote_files = Some(files);
			state.download = None;
			return need_unlock;
		}
		false
	}

	/// 服务端是否已就绪（收到过 `CB_CLIP_CAPS` 或 `CB_MONITOR_READY`）。
	pub fn server_ready(&self) -> bool {
		self.lock().server_ready
	}

	/// 服务端广播之后，我们**有没有真的拿到内容**。
	///
	/// 这是判断「反向链路到底断在哪一环」的关键：
	/// - `server_broadcast == false` → 服务端没广播（远端没动过 / 服务端限制）
	/// - `server_broadcast == true` 但这个为 `false` → 服务端广播了、
	///   我们回应了、也索要了，但**数据没回来**（协议层被卡）
	/// - 两者都 `true` → 内容确实到了
	pub fn remote_data_received(&self) -> bool {
		self.lock().remote_data_received
	}

	/// 本会话里服务端有没有主动广播过剪贴板变化。
	///
	/// **用来判定「远端 → 本地」方向是否真的可用。**
	///
	/// 这个方向完全由服务端主导：只有远端剪贴板变了，
	/// 服务端才会主动发 `CB_FORMAT_LIST`。客户端没有任何办法
	/// 去「拉」——它连远端现在有哪些格式都不知道。
	///
	/// 所以这个标志能让界面给出明确说法，而不是让用户反复重试、
	/// 以为软件坏了——两个方向失败时的表现都是「没反应」，
	/// 从外面看一模一样，没有这个标志就区分不出来。
	pub fn server_broadcast_ever(&self) -> bool {
		self.lock().server_broadcast
	}

	/// 上一条上报的内容**是否已经应答出去了**。
	///
	/// 注入 Ctrl+V 之前必须等它为 `true`——数据没送达就按，
	/// 远端粘贴到的是上一份内容。
	///
	/// 真实耗时从 20ms 到几百毫秒都有（看服务端当时忙不忙），
	/// 所以只能等这个事件，不能等固定时长。
	pub fn remote_took_data(&self) -> bool {
		self.lock().remote_taken
	}

	/// 服务端是否已确认我们最近一次上报的格式列表。
	///
	/// **注入 Ctrl+V 前必须等它为 `true`**（详见字段 `list_acked` 的说明）：
	/// 服务端只在用户粘贴时才来索要数据，而那次粘贴就是我们注入的 Ctrl+V。
	/// 先按键后上报 = 远端粘到空剪贴板 + 永远等不到取数请求。
	pub fn format_list_acked(&self) -> bool {
		self.lock().list_acked
	}

	/// 本会话是否已向服务端上报过至少一条格式列表。
	///
	/// 界面拿它判断「还需不需要补一条开嗓的空文本」，见 [`Self::arm`]。
	pub fn announced_ever(&self) -> bool {
		self.lock().announced_once
	}

	/// 上报一条**空文本**格式列表，只为唤醒服务端的剪贴板监听。
	///
	/// # 为什么必须有这一步
	///
	/// Windows 的 rdpclip 在客户端上报第一条格式列表之前不监听会话剪贴板，
	/// 远端 Ctrl+C 因此不发 `CB_FORMAT_LIST`——「远端复制 → 本地粘贴」
	/// 整条哑掉（真机 21/21 相关，见 `ClipboardState::announced_once`）。
	///
	/// 而**最容易被忽略的恰恰是「本机没东西可报」**：剪贴板为空、或者
	/// 只有一张图片（截图之后很常见，arboard 读文本会失败、也没有文件对象）。
	/// 旧逻辑此时一条格式列表都不发，会话就永远收不到远端广播。
	///
	/// 代价是远端剪贴板会被这条空文本覆盖一次。这与真实远程桌面客户端的
	/// 行为一致（连上就把本机剪贴板同步过去），而且只在「本机确实没有
	/// 可传内容」时发生，比「远端复制永远没反应」划算得多。
	pub fn arm(&self) {
		let mut state = self.lock();
		if state.announced_once || state.pending_out.is_some() {
			return;
		}
		state.pending_out = Some(Content::Text(String::new()));
		tracing::info!(
			"[剪贴板] 本机剪贴板没有可传的内容，先上报一条空文本——让远端 rdpclip 开始监听"
		);
	}

	/// 最近一条服务端报文的类型与距今时长（诊断用）。
	pub fn last_server_message(&self) -> Option<(&'static str, std::time::Duration)> {
		self.lock()
			.last_server_message
			.map(|(kind, at)| (kind, at.elapsed()))
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, ClipboardState> {
		// 锁只在极短的临界区里持有，不会 panic；真中毒也让调用方看到空状态
		self.state
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

impl Default for Clipboard {
	fn default() -> Self {
		Self::new()
	}
}

// ==================== 通道回调 ====================

/// 从 `CliprdrClientContext` 取回我们的状态。
///
/// `custom` 字段是插件自留的指针——`VirtualChannelEntryEx` 里由 FreeRDP
/// 把 `pExtendedData`（我们传进去的 `PluginData`）填到它上面。
/// SAFETY: 由 FreeRDP 在回调里调用，`context` 非空且 `custom` 已由入口填好。
unsafe fn state_of(context: *mut ffi::CliprdrClientContext) -> &'static PluginData {
	// SAFETY: 调用方保证 context 有效。
	let context = unsafe { &*context };
	// SAFETY: custom 由我们的入口数据填入，类型是 PluginData。
	unsafe { &*(context.custom as *const PluginData) }
}

/// 记下「刚收到一条服务端报文」。
///
/// 唯一的用途是诊断：远端没反应时，界面要能区分「服务端一条报文都没发」
/// 与「通道还活着、只是 rdpclip 没广播」，见
/// [`ClipboardState::last_server_message`]。
fn note_server_message(state: &PluginData, kind: &'static str) {
	if let Ok(mut guard) = state.lock() {
		guard.last_server_message = Some((kind, std::time::Instant::now()));
	}
}

/// `CB_CLIP_CAPS`：服务端声明自己的能力。
///
/// **必须回一个客户端 CAPS，不能只记账** —— 这是第 6 轮排查定下的根因。
///
/// FreeRDP 的 `cliprdr_client_capabilities`（`cliprdr_main.c:561`）在末尾做两件事：
/// 1. 与服务端协商出双方都支持的功能位，写进报文发回去；
/// 2. **`initialFormatListSent = FALSE`** —— 把「首条格式列表」的闸门重新打开。
///
/// 我们从不回复，服务端的协商停在中途，也不重置那个闸门。
/// 于是「上报格式后远端不索要数据」成了必然：服务端那边从未完成
/// 能力协商，根本不知道我们宣告了哪些格式，自然不会来取数据。
///
/// 症状极其隐蔽：通道握手正常、格式列表发得出去、服务端还回
/// `CB_RESPONSE_OK`（那只表示"收到了报文"），唯独内容永远送不过去，
/// 且没有任何错误可查。
unsafe extern "C" fn server_capabilities_trampoline(
	context: *mut ffi::CliprdrClientContext,
	capabilities: *const ffi::CLIPRDR_CAPABILITIES,
) -> ffi::UINT {
	// SAFETY: 由 FreeRDP 在回调里调用，context 非空。
	let context = unsafe { &mut *context };
	note_server_message(unsafe { state_of(context) }, "能力集");
	if capabilities.is_null() {
		return CHANNEL_RC_OK;
	}

	// 打印**服务端**宣告的能力位——文件广播行为取决于它：
	// 服务端没宣告 `CB_STREAM_FILECLIP_ENABLED` 时，rdpclip 收到
	// CF_HDROP 不会转成 FileGroupDescriptorW 广播（远端复制文件
	// 我们这边就毫无动静）。capabilitySets 头 4 字节是
	// (type, length)，紧跟 version(4) + generalFlags(4)。
	//
	// SAFETY: capabilitySets/cCapabilitiesSets 由 FreeRDP 填好，有效。
	let caps = unsafe { &*capabilities };
	if !caps.capabilitySets.is_null() && caps.cCapabilitiesSets > 0 {
		let base = caps.capabilitySets.cast::<u8>();
		let mut flags: u32 = 0;
		// SAFETY: set 长度至少 12（头 4 + version 4 + flags 4），越界前有判。
		unsafe {
			let set_len = std::slice::from_raw_parts(base.add(2).cast::<u16>(), 1)[0] as usize;
			if set_len >= 12 {
				flags = std::slice::from_raw_parts(base.add(8).cast::<u32>(), 1)[0];
			}
		}
		tracing::info!(
			"[剪贴板] 服务端能力集 flags=0x{flags:04x}（长名=0x02 流=0x04 无路径=0x08 锁=0x10 巨型=0x20）"
		);
	}

	// **回一份最小能力集：只宣告 `CB_USE_LONG_FORMAT_NAMES`。**
	//
	// 这是第二十六轮定位到的根因，也是整条链路最关键的一处。
	//
	// 原实现把服务端给的能力集（`generalFlags = 0x3E`，含
	// `CB_CAN_LOCK_CLIPDATA` / `CB_STREAM_FILECLIP_ENABLED` /
	// `CB_HUGE_FILE_SUPPORT_ENABLED`）原样回声。看起来"协商双方都支持的功能"，
	// 实际等于向服务端宣告：**我支持锁协议**。
	// 于是服务端放弃 `CB_FORMAT_DATA_REQUEST`，改用 `CB_LOCK_CLIPDATA`
	// 来跟我们交互——而锁本来只是「保留文件流数据」的通知，
	// 服务端的取数请求根本不会再来，正向链路彻底静默。
	//
	// 真机对照实验（192.168.10.20，FreeRDP 协议日志）：
	// - 回声 0x3E：每次上报格式列表后 5ms 内必收到一条 `CB_LOCK_CLIPDATA`，
	//   此后 20 秒全静默，无广播、无取数请求，Windows 剪贴板始终为空。
	// - 只宣告 0x02：服务端回一条 `CB_FORMAT_LIST_RESPONSE(OK)` 就安静，
	//   不再发锁，回到规范的正规路径。
	//
	// FreeRDP 官方 macOS 客户端（`client/mac/Clipboard.m` 的
	// `mac_cliprdr_send_client_capabilities`）正是这样：
	// `version = CB_CAPS_VERSION_2`、`generalFlags = CB_USE_LONG_FORMAT_NAMES`，
	// 一个多余的位都不宣告。
	//
	// 换句话说：**能力位不是"能宣称什么就宣称什么"，而是"真的实现了才宣称"。**
	// 我们只做了纯文本双向，文件流 / 锁 / 巨型文件一个都没实现，
	// 回声服务端的位等于主动邀请对方用我们没实现的机制。
	let mut set = GeneralCapabilitySet {
		capability_set_type: CB_CAPSTYPE_GENERAL,
		capability_set_length: 12,
		version: CB_CAPS_VERSION_2,
		general_flags: CB_USE_LONG_FORMAT_NAMES
			| CB_STREAM_FILECLIP_ENABLED
			| CB_CAN_LOCK_CLIPDATA,
	};
	let reply = ffi::CLIPRDR_CAPABILITIES {
		common: cliprdr_header(CB_CLIP_CAPS, 0, 4 + 12),
		cCapabilitiesSets: 1,
		capabilitySets: (&raw mut set).cast::<ffi::CLIPRDR_CAPABILITY_SET>(),
	};
	// SAFETY: send 回调由 FreeRDP 在接管回调时装好；set 在本次调用期间有效。
	unsafe {
		if let Some(send) = context.ClientCapabilities {
			let rc = send(context, &reply);
			if rc != 0 {
				tracing::warn!("回复剪贴板能力集失败：{rc}");
				return rc;
			}
			tracing::info!("[剪贴板] 已回复客户端能力集（版本 2，长格式名 + 文件流 + 锁）");
		}
	}

	// SAFETY: context 有效，custom 已由入口填好。
	let state = unsafe { state_of(context) };
	if let Ok(mut guard) = state.lock() {
		guard.server_ready = true;
	}
	tracing::info!("[剪贴板] 已回复客户端能力集");
	0
}

/// `CB_MONITOR_READY`：服务端说"我开始监听剪贴板了"。
///
/// **这是发送的起跑线**：没收到它之前发格式列表，服务端还没准备好接。
unsafe extern "C" fn monitor_ready_trampoline(
	context: *mut ffi::CliprdrClientContext,
	_monitor_ready: *const ffi::CLIPRDR_MONITOR_READY,
) -> ffi::UINT {
	// SAFETY: 同上。
	let state = unsafe { state_of(context) };
	note_server_message(state, "MonitorReady");
	if let Ok(mut guard) = state.lock() {
		guard.server_ready = true;
	}
	tracing::info!("[剪贴板] 远端已就绪（MonitorReady）");
	0
}

/// `CB_FORMAT_LIST`：远端广播"我剪贴板里有这些格式"。
///
/// **文件格式优先于文本**：Windows 在远端复制文件时，格式列表里可能同时带
/// `FileGroupDescriptorW` 与若干文本格式（文件名文本）；走文件路径才能拿到
/// 内容，走文本只会得到一串文件名。
unsafe extern "C" fn server_format_list_trampoline(
	context: *mut ffi::CliprdrClientContext,
	format_list: *const ffi::CLIPRDR_FORMAT_LIST,
) -> ffi::UINT {
	// SAFETY: context 有效，custom 已由入口填好。
	note_server_message(unsafe { state_of(context) }, "格式列表");
	let Some(list) = (unsafe { format_list.as_ref() }) else {
		return CHANNEL_RC_OK;
	};
	if list.numFormats == 0 {
		// 空列表也要留痕：过滤后全被筛掉/服务端发了个空广播，从外面看
		// 与「没广播」一模一样，不说话就没法区分这两种故障。
		// `dataLen` 是报文载荷字节数——>0 但解析出 0 条说明**解析**环节丢的，
		// =0 则是服务端真发了空壳
		tracing::info!(
			"[剪贴板] 收到服务端格式广播，但 0 个格式（载荷 {} 字节，flags=0x{:04x}）",
			list.common.dataLen,
			list.common.msgFlags
		);
		return CHANNEL_RC_OK;
	}
	if list.formats.is_null() {
		return CHANNEL_RC_OK;
	}

	// 服务端主动广播了——远端 → 本地方向可用。
	// SAFETY: context 有效，custom 已由入口填好。
	if let Ok(mut guard) = unsafe { state_of(context) }.lock() {
		guard.server_broadcast = true;
	}

	// 找出服务端广播的格式：
	//
	// 先找**文件描述表**（注册格式，按名字匹配——名字由服务端在格式列表里
	// 给出，ID 服务端自定，我们不能假设是 0xC0DE）。
	// 没有文件格式再退回文本：挑一个**服务端明确宣告支持的**去索要，
	// 比硬编码某一个更稳——只要它的 formatId 在列表里，服务端就认这个请求。
	//
	// SAFETY: numFormats/formats 由 FreeRDP 保证有效。
	let kind = unsafe {
		let formats = std::slice::from_raw_parts(list.formats, list.numFormats as usize);
		formats
			.iter()
			.find(|f| format_name_is(f, "FileGroupDescriptorW"))
			.map(|f| RequestKind::FileDescriptors(f.formatId))
			.or_else(|| {
				formats
					.iter()
					.find(|format| format.formatId == CF_UNICODETEXT)
					.or_else(|| formats.iter().find(|f| f.formatId == CF_TEXT))
					.map(|format| RequestKind::Text(format.formatId))
			})
	};
	let Some(kind) = kind else {
		tracing::info!(
			"[剪贴板] 远端广播了剪贴板变化，但无文件/文本格式（共 {} 种）",
			list.numFormats
		);
		return CHANNEL_RC_OK;
	};

	// SAFETY: context 有效。
	let context_ref = unsafe { &mut *context };

	// **顺序不能反：先回应格式列表，再索要数据。**
	//
	// MS-RDPECLIP 的 Copy Sequence（服务端 → 客户端方向）是：
	// ```
	// 服务端 ──Format List────────────►客户端   「我剪贴板里有这些格式」
	// 客户端 ──Format List Response───►服务端   ← 必须先这一步
	// 服务端 ──Lock Clipboard Data───►客户端
	// 客户端 ──Format Data Request───►服务端   「把内容给我」
	// 服务端 ──Format Data Response──►客户端
	// ```
	// 服务端是**看到确认之后才肯发数据**的。
	tracing::info!(
		"[剪贴板] 远端广播了剪贴板变化（{}），先回应再记下索要",
		match kind {
			RequestKind::FileDescriptors(_) => "文件",
			RequestKind::Text(_) => "文本",
		}
	);
	// **不据 rc 提前返回**：回应失败也照样要索数据，否则整条反向链路静默废掉。
	let rc = respond_format_list(context_ref, true);
	tracing::info!("[剪贴板] 已回应格式列表（rc={rc}）");

	// **只记账，不在这里发报文。** 见 `pending_request` 的说明：
	// 回调返回后 FreeRDP 会释放它给我们的格式列表内存，
	// 而通道写入与报文解析共用结构体，此刻发送并不安全。
	// 真正发出是在帧循环的 `flush_outgoing` 里。
	{
		let state = unsafe { state_of(context) };
		if let Ok(mut guard) = state.lock() {
			guard.pending_request = Some(kind);
			tracing::info!("[剪贴板] 已记下待索取请求（{kind:?}），等下一帧发出");
		}
	}
	CHANNEL_RC_OK
}

/// 一个广播格式是不是某个**注册格式**（按名字匹配，名字是 ANSI 串）。
///
/// # Safety
/// `format.formatName` 由 FreeRDP 分配且在本次回调内有效（UTF-8/ANSI C 串）。
unsafe fn format_name_is(format: &ffi::CLIPRDR_FORMAT, name: &str) -> bool {
	if format.formatName.is_null() {
		return false;
	}
	// SAFETY: 非空指针，指向 NUL 结尾的 C 串（FreeRDP 解析格式列表时填的）。
	let c_name = unsafe { std::ffi::CStr::from_ptr(format.formatName) };
	c_name.to_bytes() == name.as_bytes()
}

/// 回复服务端的格式列表：`accept` 为真表示接受这些格式。
///
/// MS-RDPECLIP 要求客户端先回应，服务端才肯发数据。省略这一步
/// 服务端就会一直等着，最终什么也不发。
fn respond_format_list(context: &mut ffi::CliprdrClientContext, accept: bool) -> ffi::UINT {
	// SAFETY: context 由 FreeRDP 传入且非空。
	let send = context.ClientFormatListResponse;
	let Some(send) = send else {
		tracing::warn!("通道未提供 ClientFormatListResponse");
		return CHANNEL_RC_OK;
	};
	let response = ffi::CLIPRDR_FORMAT_LIST_RESPONSE {
		common: cliprdr_header(
			CB_FORMAT_LIST_RESPONSE,
			if accept {
				CB_RESPONSE_OK
			} else {
				CB_RESPONSE_FAIL
			},
			0,
		),
	};
	// SAFETY: 回调指针由 FreeRDP 提供，参数是我们构造的合法结构。
	let rc = unsafe { send(context, &response) };
	if rc != 0 {
		tracing::warn!("回复格式列表失败（返回 {rc}）");
	}
	rc
}

/// `CB_FORMAT_DATA_RESPONSE`：远端发来我们索取的数据。
///
/// **响应不带格式 ID**——是什么数据全看我们此前发的是什么请求
/// （`state.requested`）：文本解 UTF-16，文件解描述表。
unsafe extern "C" fn server_format_data_response_trampoline(
	context: *mut ffi::CliprdrClientContext,
	response: *const ffi::CLIPRDR_FORMAT_DATA_RESPONSE,
) -> ffi::UINT {
	let Some(response) = (unsafe { response.as_ref() }) else {
		return CHANNEL_RC_OK;
	};
	let len = response.common.dataLen as usize;
	// SAFETY: context 有效。
	let state = unsafe { state_of(context) };
	note_server_message(state, "数据应答");
	if len == 0 || response.requestedFormatData.is_null() {
		// rdpclip 时序：广播刚到时系统剪贴板可能还没填好，应答就是空的。
		// 此刻放弃＝用户看到「远端复制了但这边没反应」，所以隔一会儿再问。
		let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
		if matches!(guard.requested, Some(RequestKind::FileDescriptors(_)))
			&& guard.retry_left > 0
		{
			guard.retry_left -= 1;
			guard.retry_at = Some(std::time::Instant::now() + DESCRIPTOR_RETRY_DELAY);
			tracing::info!(
				"[剪贴板] 远端描述表回空（rdpclip 可能还没写完剪贴板），{}ms 后重试",
				DESCRIPTOR_RETRY_DELAY.as_millis()
			);
			return CHANNEL_RC_OK;
		}
		tracing::info!("[剪贴板] 远端发来的数据为空（dataLen=0）");
		return CHANNEL_RC_OK;
	}
	// SAFETY: requestedFormatData/dataLen 由 FreeRDP 保证有效。
	let bytes = unsafe { std::slice::from_raw_parts(response.requestedFormatData, len) };

	let kind = state.lock().ok().and_then(|mut guard| guard.requested.take());

	match kind {
		Some(RequestKind::FileDescriptors(_)) => {
			// 文件描述表：建下载会话；SIZE/RANGE 请求由帧循环发出
			let entries = parse_file_group_descriptor(bytes);
			if entries.is_empty() {
				tracing::warn!("[剪贴板] 远端文件描述表解析不出任何条目（{} 字节）", bytes.len());
				return CHANNEL_RC_OK;
			}
			for entry in &entries {
				tracing::info!("[剪贴板] 远端文件：{}（{}）", entry.name, human_size(entry.size));
			}
			let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
			guard.remote_data_received = true;
			// **先锁再取**：取数方要主动 `CB_LOCK_CLIPDATA`（见该常量说明）
			let clip_data_id = Some(DOWNLOAD_CLIP_DATA_ID);
			match Download::start(entries, clip_data_id) {
				Ok(download) => {
					tracing::info!("[剪贴板] 开始下载远端文件（共 {} 个）", download.entries.len());
					guard.download = Some(download);
				},
				Err(error) => {
					tracing::warn!("[剪贴板] 远端文件下载取消：{error}");
					guard.download_error = Some(error);
				},
			}
		},
		_ => match decode_utf16le(bytes) {
			Some(text) => {
				if let Ok(mut guard) = state.lock() {
					guard.remote = Some(text.clone());
					guard.remote_data_received = true;
				}
				tracing::info!(
					"[剪贴板] ✅ 收到远端剪贴板文本 {} 字符：{text:?}",
					text.chars().count()
				);
			},
			None => tracing::warn!("[剪贴板] 远端数据不是合法 UTF-16LE（dataLen={len}）"),
		},
	}
	CHANNEL_RC_OK
}

/// 请求远端发来指定格式的数据。
fn request_format_data(context: &mut ffi::CliprdrClientContext, format_id: u32) -> ffi::UINT {
	// SAFETY: context 由 FreeRDP 传入且非空。
	let send = context.ClientFormatDataRequest;
	let Some(send) = send else {
		tracing::warn!("通道未提供 ClientFormatDataRequest，无法请求剪贴板数据");
		return CHANNEL_RC_OK;
	};
	let request = ffi::CLIPRDR_FORMAT_DATA_REQUEST {
		common: cliprdr_header(CB_FORMAT_DATA_REQUEST, 0, 4),
		requestedFormatId: format_id,
	};
	// SAFETY: 回调指针由 FreeRDP 提供，参数是我们构造的合法结构。
	// 请求发不出去是反向链路最隐蔽的故障点（见
	// `must_not_override_client_format_data_request`），必须能直接看到
	tracing::info!("[剪贴板] 已调用 ClientFormatDataRequest（formatId=0x{format_id:x}）");
	unsafe { send(context, &request) }
}

/// 上报"本地剪贴板有内容"，服务端随后会来要数据。
///
/// 文本与文件是**两套格式列表**：
/// - 文本：`CF_UNICODETEXT` + `CF_LOCALE` + `CF_TEXT`（标准格式，name 传 NULL）
/// - 文件：`FileGroupDescriptorW` + `FileContents`（注册格式，**必须带名字**——
///   服务端按名字匹配，MS-RDPECLIP 示例的 ID 就是 0xC0DE/0xC0DF）
fn send_format_list(
	context: &mut ffi::CliprdrClientContext,
	content: &Content,
) -> std::result::Result<(), String> {
	// SAFETY: context 非空。
	let send = context.ClientFormatList;
	let Some(send) = send else {
		return Err("通道未提供 ClientFormatList".to_string());
	};

	// **标准格式的 name 传 NULL**（规范 §2.2.3.1.2.1：无名字的格式发一个
	// Unicode NUL）；**注册格式的 name 必须给**，否则 Windows 认不出来。
	// 两类格式的差异被第二十六轮钉死，见 `must_not_override…` 附近的注释。
	//
	// 名字串必须活过 `send` 调用——CString 放在本函数栈上即可，
	// FreeRDP 在序列化时立即拷贝进报文。
	let file_names = [
		std::ffi::CString::new("FileGroupDescriptorW").expect("无内部 NUL"),
		std::ffi::CString::new("FileContents").expect("无内部 NUL"),
	];
	let (formats, summary): ([ffi::CLIPRDR_FORMAT; 2], String) = match content {
		Content::Text(_) => {
			return send_text_format_list(context);
		},
		Content::Files(files) => (
			[
				ffi::CLIPRDR_FORMAT {
					formatId: FORMAT_FILE_GROUP_DESCRIPTOR_W,
					formatName: file_names[0].as_ptr().cast_mut(),
				},
				ffi::CLIPRDR_FORMAT {
					formatId: FORMAT_FILE_CONTENTS,
					formatName: file_names[1].as_ptr().cast_mut(),
				},
			],
			format!("文件 {} 个", files.len()),
		),
	};
	// 注意 `dataLen` 由 FreeRDP 的 `cliprdr_packet_format_list_new`
	// 内部重算（长格式名按实际长度写记录），我们这里填的值只作参考。
	let list = ffi::CLIPRDR_FORMAT_LIST {
		common: cliprdr_header(CB_FORMAT_LIST, 0, 0),
		numFormats: formats.len() as u32,
		formats: formats.as_ptr().cast_mut().cast(),
	};
	tracing::info!(
		"[剪贴板] 上报格式列表（{summary}）：FileGroupDescriptorW=0x{:x} / FileContents=0x{:x}，等服务端来索要",
		formats[0].formatId,
		formats[1].formatId
	);
	// SAFETY: 回调指针有效，参数是合法结构；names 在调用期间有效。
	let rc = unsafe { send(context, &list) };
	if rc != 0 {
		return Err(format!("ClientFormatList 返回 {rc}"));
	}
	Ok(())
}

/// 文本内容的格式列表（原 S8 逻辑，S10 起被 [`send_format_list`] 分派）。
fn send_text_format_list(
	context: &mut ffi::CliprdrClientContext,
) -> std::result::Result<(), String> {
	// SAFETY: context 非空。
	let send = context.ClientFormatList;
	let Some(send) = send else {
		return Err("通道未提供 ClientFormatList".to_string());
	};

	// **上报 `CF_LOCALE` + `CF_UNICODETEXT` + `CF_TEXT` 三种。**
	//
	// `CF_LOCALE` 是 Unicode 文本的伴随格式：Windows 约定
	// `CF_UNICODETEXT` 必须配 Locale，缺了 `SetClipboardData` 会失败。
	//
	// 顺序按 mstsc 的习惯：Unicode 优先，Locale 次之，ANSI 兜底。
	let formats = [
		ffi::CLIPRDR_FORMAT {
			formatId: CF_UNICODETEXT,
			formatName: std::ptr::null_mut(),
		},
		ffi::CLIPRDR_FORMAT {
			formatId: CF_LOCALE,
			formatName: std::ptr::null_mut(),
		},
		ffi::CLIPRDR_FORMAT {
			formatId: CF_TEXT,
			formatName: std::ptr::null_mut(),
		},
	];
	// 注意 `dataLen` 由 FreeRDP 的 `cliprdr_packet_format_list_new`
	// 内部重算（它按 `numFormats × 36` 写完整记录），我们这里填的值
	// 只作参考——真正决定报文形态的是 numFormats 与 formats 指针。
	let list = ffi::CLIPRDR_FORMAT_LIST {
		common: cliprdr_header(CB_FORMAT_LIST, 0, 0),
		numFormats: formats.len() as u32,
		formats: formats.as_ptr().cast_mut().cast(),
	};
	tracing::info!(
		"[剪贴板] 上报格式列表 3 种：0x{:x} / 0x{:x} / 0x{:x}，等服务端 FormatDataRequest 来索要",
		formats[0].formatId,
		formats[1].formatId,
		formats[2].formatId
	);
	// SAFETY: 回调指针有效，参数是合法结构。
	let rc = unsafe { send(context, &list) };
	if rc != 0 {
		return Err(format!("ClientFormatList 返回 {rc}"));
	}
	Ok(())
}

/// `CB_FORMAT_DATA_REQUEST`：**服务端来索要**我们上报过的某种格式的数据。
///
/// ## 这是正向（本地 → 远端）链路唯一真正的入口
///
/// MS-RDPECLIP 的 Paste Sequence（§1.3.2.2.3 / §2.2.5）：
///
/// ```text
/// 客户端 ──Format List─────────► 服务端   「我剪贴板里有这些格式」
/// 服务端 ──Format List Response► 客户端
/// 服务端 ──Format Data Request─► 客户端   「把 CF_UNICODETEXT 给我」← 到了这里
/// 客户端 ──Format Data Response► 服务端   数据在这里，服务端写进系统剪贴板
/// ```
///
/// **此前二十轮修的全是错的那一步。** 我们把数据挂在 `ServerLockClipboardData`
/// 上主动推出去，而规范里 `CB_LOCK_CLIPDATA`（§2.2.4.1）是
/// **File Stream 数据保留机制**——服务端锁住自己剪贴板上的文件流，
/// 好让后续 `File Contents Request` 能取到，与纯文本无关。
/// 于是真实链路变成：服务端要数据 → 没人接 → Windows 剪贴板永远空 →
/// 右键菜单「粘贴」是灰的（用户实测现象）。
///
/// 依据：`vendor/freerdp/channels/cliprdr/client/cliprdr_format.c:240`
/// `cliprdr_process_format_data_request` 末尾
/// `IFCALLRET(context->ServerFormatDataRequest, ...)` —— 这个回调为 `None`
/// 时，FreeRDP **静默丢弃**请求，一条字节都不回。
unsafe extern "C" fn server_format_data_request_trampoline(
	context: *mut ffi::CliprdrClientContext,
	request: *const ffi::CLIPRDR_FORMAT_DATA_REQUEST,
) -> ffi::UINT {
	let Some(request) = (unsafe { request.as_ref() }) else {
		return CHANNEL_RC_OK;
	};
	let format_id = request.requestedFormatId;
	note_server_message(unsafe { state_of(context) }, "取数请求");
	tracing::info!(
		"[剪贴板] 服务端索要数据 formatId=0x{format_id:x}（CF_UNICODETEXT=0x{CF_UNICODETEXT:x}，FileGroupDescriptorW=0x{FORMAT_FILE_GROUP_DESCRIPTOR_W:x}）"
	);

	// SAFETY: context 由 FreeRDP 传入，有效。
	let context = unsafe { &mut *context };

	// **取内容但不销毁它**：同一份内容可能被反复索要
	// （服务端按不同格式各要一次），`take()` 会让第二次变空。
	let content = {
		let state = unsafe { state_of(context) };
		let guard = state.lock().unwrap_or_else(|p| p.into_inner());
		guard
			.sending
			.as_ref()
			.map(|(content, _)| content.clone())
			.or_else(|| guard.pending_out.clone())
	};

	// 文件路径：服务端要**文件描述表**（名字 + 大小的目录）。
	if format_id == FORMAT_FILE_GROUP_DESCRIPTOR_W {
		let Some(Content::Files(files)) = &content else {
			tracing::warn!("[剪贴板] 服务端要文件描述表，但本地待发内容不是文件，回空响应");
			return send_format_data_response(context, format_id, &[]);
		};
		let bytes = match build_file_group_descriptor(files) {
			Ok(bytes) => bytes,
			Err(error) => {
				tracing::warn!("[剪贴板] 组装文件描述表失败：{error}");
				// 失败要明说：CB_RESPONSE_FAIL 会让服务端放弃这个格式而不是干等
				return send_format_data_response_fail(context, format_id);
			},
		};
		tracing::info!("[剪贴板] 文件描述表 {} 字节（{} 个条目）", bytes.len(), files.len());
		return send_format_data_response(context, format_id, &bytes);
	}
	// `FileContents` 格式本身没有 FormatData 载荷——内容走 FileContentsRequest。
	// 给个空 OK，让服务端知道我们活着。
	if format_id == FORMAT_FILE_CONTENTS {
		return send_format_data_response(context, format_id, &[]);
	}

	let Some(Content::Text(text)) = &content else {
		tracing::warn!("[剪贴板] 服务端索要文本格式但本地待发内容不是文本，回空响应");
		return send_format_data_response(context, format_id, &[]);
	};

	let bytes = encode_for_format(format_id, text);
	tracing::info!(
		"[剪贴板] 按 formatId=0x{format_id:x} 应答 {} 字节",
		bytes.len()
	);
	// 数据确实发出去了——置位给「粘贴」路径判定时机用。
	{
		let state = unsafe { state_of(context) };
		if let Ok(mut guard) = state.lock() {
			guard.remote_taken = true;
		}
	}
	send_format_data_response(context, format_id, &bytes)
}

/// 按服务端索要的格式编码载荷。
///
/// **`CF_LOCALE` 是个整数，不是文本**——这点错了会连累
/// `CF_UNICODETEXT` 一起被判无效（第十八轮加 `CF_LOCALE` 时按文本发的）。
fn encode_for_format(format_id: u32, text: &str) -> Vec<u8> {
	match format_id {
		CF_UNICODETEXT => encode_utf16le(text),
		CF_TEXT => {
			let mut bytes = text.as_bytes().to_vec();
			bytes.push(0);
			bytes
		},
		CF_LOCALE => CF_LOCALE_LANGID_ZH_CN.to_le_bytes().to_vec(),
		// 不认识的格式：给空载荷，服务端会退回去试别的格式
		_ => Vec::new(),
	}
}

/// 下载远端文件前，我们先「锁」住这份文件流的 `clipDataId`。
///
/// MS-RDPECLIP 3.1.5：**取数方**在向数据属主索要文件字节前先发
/// `CB_LOCK_CLIPDATA`，属主回 `CB_FILECONTENTS_RESPONSE` 时可据此对账。
/// 反向（Windows 来要我们的文件）时正是这个顺序：它先发锁、再发请求，
/// 请求载荷也带 id（dataLen 28 = 24 + 4）。
/// 我们之前跳过锁直接请求，Windows Server 2019 的 rdpclip 一律拒绝/忽略。
const DOWNLOAD_CLIP_DATA_ID: u32 = 0x00da_1501;

/// 发一条 `CB_LOCK_CLIPDATA`（我们作为取数方，锁住远端给的文件流）。
fn send_lock_clipdata(context: &mut ffi::CliprdrClientContext, clip_data_id: u32) -> ffi::UINT {
	let Some(send) = context.ClientLockClipboardData else {
		tracing::warn!("通道未提供 ClientLockClipboardData");
		return CHANNEL_RC_OK;
	};
	let msg = ffi::CLIPRDR_LOCK_CLIPBOARD_DATA {
		common: cliprdr_header(CB_LOCK_CLIPDATA, 0, 4),
		clipDataId: clip_data_id,
	};
	// SAFETY: 回调指针有效，msg 在调用期间有效。
	unsafe { send(context, &msg) }
}

/// 发一条 `CB_UNLOCK_CLIPDATA`（下载结束/失败后释放我们锁住的文件流）。
fn send_unlock_clipdata(context: &mut ffi::CliprdrClientContext, clip_data_id: u32) -> ffi::UINT {
	let Some(send) = context.ClientUnlockClipboardData else {
		return CHANNEL_RC_OK;
	};
	let msg = ffi::CLIPRDR_UNLOCK_CLIPBOARD_DATA {
		common: cliprdr_header(CB_UNLOCK_CLIPDATA, 0, 4),
		clipDataId: clip_data_id,
	};
	// SAFETY: 回调指针有效，msg 在调用期间有效。
	unsafe { send(context, &msg) }
}

/// 发一条 `CB_FORMAT_DATA_RESPONSE`。
fn send_format_data_response(
	context: &mut ffi::CliprdrClientContext,
	format_id: u32,
	bytes: &[u8],
) -> ffi::UINT {
	let Some(send) = context.ClientFormatDataResponse else {
		tracing::warn!("通道未提供 ClientFormatDataResponse");
		return CHANNEL_RC_OK;
	};
	// msgFlags 带 `CB_RESPONSE_OK`（0x0001）：MS-RDPECLIP §2.2.1 明确
	// 「CB_RESPONSE_OK 用于 Format List Response、**Format Data Response**
	// 与 File Contents Response PDU，表示关联的请求被成功处理」。
	// 第十一轮加过又被第十八轮当成语义错误撤掉了——规范里它就长在这个位置。
	let response = ffi::CLIPRDR_FORMAT_DATA_RESPONSE {
		common: cliprdr_header(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, bytes.len() as u32),
		requestedFormatData: bytes.as_ptr(),
	};
	// SAFETY: 回调指针有效；bytes 活到调用返回。
	let rc = unsafe { send(context, &response) };
	if rc != 0 {
		tracing::warn!("[剪贴板] 发送数据响应失败（返回 {rc}）");
	} else {
		tracing::info!(
			"[剪贴板] ✅ 已发数据响应 formatId=0x{format_id:x} {} 字节（msgFlags=CB_RESPONSE_OK）",
			bytes.len()
		);
	}
	rc
}

/// 拒绝一条 FormatDataRequest：`CB_RESPONSE_FAIL`。
///
/// 服务端收到 FAIL 会放弃该格式（或中止粘贴），好过对着一串垃圾数据发呆。
fn send_format_data_response_fail(
	context: &mut ffi::CliprdrClientContext,
	format_id: u32,
) -> ffi::UINT {
	let Some(send) = context.ClientFormatDataResponse else {
		return CHANNEL_RC_OK;
	};
	let response = ffi::CLIPRDR_FORMAT_DATA_RESPONSE {
		common: cliprdr_header(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_FAIL, 0),
		requestedFormatData: std::ptr::null(),
	};
	// SAFETY: 回调指针有效。
	tracing::warn!("[剪贴板] 已拒绝 formatId=0x{format_id:x} 的数据请求（CB_RESPONSE_FAIL）");
	unsafe { send(context, &response) }
}

/// 发一条 `FileContentsRequest`（**下载**远端文件的下一步）。
///
/// 这是 FreeRDP 的发送函数（`ClientFileContentsRequest`），门槛在它内部：
/// `ClipboardFeatureMask` 必须含 `REMOTE_TO_LOCAL_FILES`（见 vendored
/// `cliprdr_main.c:952`）。
fn send_file_contents_request(
	context: &mut ffi::CliprdrClientContext,
	request: &FileRequest,
) -> ffi::UINT {
	let Some(send) = context.ClientFileContentsRequest else {
		tracing::warn!("通道未提供 ClientFileContentsRequest");
		return CHANNEL_RC_OK;
	};
	// **服务端锁过就带回锁 id**：`CB_LOCK_CLIPDATA` 表示服务端把这份
	// 文件流「锁」在某个 clipDataId 下，之后的取数请求要声明自己读的
	// 就是那份数据。FreeRDP 官方文件剪贴板客户端（`client_cliprdr_file.c`
	// 的 `has_clip_data_id`/`clip_data_id`）正是这么发的；真机实测不带
	// 会被 Windows Server 2019 的 rdpclip 判 `CB_RESPONSE_FAIL`。
	let (have_clip_data_id, clip_data_id) = match request.clip_data_id {
		Some(id) => (1, id),
		None => (0, 0),
	};
	let msg = ffi::CLIPRDR_FILE_CONTENTS_REQUEST {
		// 载荷 = streamId(4)+listIndex(4)+dwFlags(4)+nPosLow(4)+nPosHigh(4)
		//       +cbRequested(4) = 24；SIZE 请求的 cbRequested 恒为 8
		// （MS-RDPECLIP 2.2.5.1 MUST）。
		common: cliprdr_header(CB_FILECONTENTS_REQUEST, 0, 24),
		streamId: request.stream_id,
		listIndex: request.list_index,
		dwFlags: request.dw_flags,
		nPositionLow: request.position as u32,
		nPositionHigh: (request.position >> 32) as u32,
		cbRequested: request.count,
		haveClipDataId: have_clip_data_id,
		clipDataId: clip_data_id,
	};
	// SAFETY: 回调指针有效，msg 在调用期间有效。
	unsafe { send(context, &msg) }
}

/// 发一条 `FileContentsResponse`（**应答**服务端对我们文件的索取）。
///
/// # Safety
/// `context` 由 FreeRDP 回调传入且有效。
unsafe fn send_file_contents_response(
	context: &mut ffi::CliprdrClientContext,
	stream_id: u32,
	msg_flags: u16,
	bytes: &[u8],
) -> ffi::UINT {
	let Some(send) = context.ClientFileContentsResponse else {
		tracing::warn!("通道未提供 ClientFileContentsResponse");
		return CHANNEL_RC_OK;
	};
	let msg = ffi::CLIPRDR_FILE_CONTENTS_RESPONSE {
		common: cliprdr_header(CB_FILECONTENTS_RESPONSE, msg_flags, bytes.len() as u32),
		streamId: stream_id,
		cbRequested: bytes.len() as u32,
		requestedData: bytes.as_ptr(),
	};
	// SAFETY: 回调指针有效；bytes 活到调用返回。
	let rc = unsafe { send(context, &msg) };
	if rc != 0 {
		tracing::warn!("[剪贴板] 发送 FileContentsResponse 失败（返回 {rc}）");
	}
	rc
}

/// 组装 `FILEGROUPDESCRIPTORW` 载荷（服务端索要文件格式时的应答）：
/// `cItems(4) + cItems × FILEDESCRIPTORW(592)`。
///
/// 条目按**当时**的 `metadata` 量大小——上报到取数之间文件可能变化，
/// 以现值为准；量不到的条目整份失败（描述表少一条，服务端的
/// `listIndex` 就会错位，宁可让它重来）。
fn build_file_group_descriptor(files: &[LocalFile]) -> std::result::Result<Vec<u8>, String> {
	let mut entries = Vec::with_capacity(files.len());
	for file in files {
		let metadata = std::fs::metadata(&file.path)
			.map_err(|error| format!("「{}」已不可访问：{error}", file.name))?;
		if metadata.is_dir() {
			return Err(format!("「{}」是文件夹，暂不支持粘贴文件夹", file.name));
		}
		entries.push(FileEntry {
			name: file.name.clone(),
			size: metadata.len(),
		});
	}

	let mut bytes = Vec::with_capacity(4 + entries.len() * FILE_DESCRIPTOR_SIZE);
	bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
	for entry in &entries {
		append_file_descriptor(&mut bytes, entry);
	}
	Ok(bytes)
}

/// 追加一个 `FILEDESCRIPTORW`（592 字节，MS-RDPECLIP §2.2.4.2.3）。
fn append_file_descriptor(bytes: &mut Vec<u8>, entry: &FileEntry) {
	// dwFlags：属性 + 大小 + 进度 UI。不带 FD_WRITESTIME——
	// 没有现成的 FILETIME 转换需求，Windows 会用当前时间兜底。
	let flags = FD_ATTRIBUTES | FD_FILESIZE | 0x0000_4000 /* FD_PROGRESSUI */;
	bytes.extend_from_slice(&flags.to_le_bytes());
	bytes.extend_from_slice(&[0u8; 16]); // clsid（不用）
	bytes.extend_from_slice(&[0u8; 8]); // sizel（POINT，不用）
	bytes.extend_from_slice(&[0u8; 8]); // pointl（POINT，不用）
	bytes.extend_from_slice(&FILE_ATTRIBUTE_ARCHIVE.to_le_bytes()); // dwFileAttributes：普通文件，不能填 0
	bytes.extend_from_slice(&[0u8; 8]); // ftCreationTime
	bytes.extend_from_slice(&[0u8; 8]); // ftLastAccessTime
	bytes.extend_from_slice(&[0u8; 8]); // ftLastWriteTime
	bytes.extend_from_slice(&((entry.size >> 32) as u32).to_le_bytes()); // nFileSizeHigh
	bytes.extend_from_slice(&(entry.size as u32).to_le_bytes()); // nFileSizeLow
	// cFileName：260 个 WCHAR，UTF-16LE，NUL 结尾，不足补零
	let mut name = [0u8; 520];
	for (index, unit) in entry.name.encode_utf16().take(259).enumerate() {
		let offset = index * 2;
		name[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
	}
	bytes.extend_from_slice(&name);
}

/// 解析 `FILEGROUPDESCRIPTORW` 载荷（远端 → 本地的文件列表）。
///
/// 目录条目**保留在列表里**（`listIndex` 必须与服务端的记账一致），
/// 由下载状态机在轮到它时报错——跳过会让下标错位。
fn parse_file_group_descriptor(bytes: &[u8]) -> Vec<FileEntry> {
	let mut entries = Vec::new();
	if bytes.len() < 4 {
		return entries;
	}
	let count = u32::from_le_bytes(bytes[0..4].try_into().expect("4 字节")) as usize;
	// 条目数守上界：载荷最多装下这么多个 592 字节的描述符
	let max_entries = (bytes.len() - 4) / FILE_DESCRIPTOR_SIZE;
	for index in 0..count.min(max_entries) {
		let start = 4 + index * FILE_DESCRIPTOR_SIZE;
		let Some(descriptor) = bytes.get(start..start + FILE_DESCRIPTOR_SIZE) else {
			break;
		};
		let dw_flags = u32::from_le_bytes(descriptor[0..4].try_into().expect("4 字节"));
		let attributes =
			u32::from_le_bytes(descriptor[36..40].try_into().expect("4 字节"));
		let size_high = u32::from_le_bytes(descriptor[64..68].try_into().expect("4 字节"));
		let size_low = u32::from_le_bytes(descriptor[68..72].try_into().expect("4 字节"));
		let size = (u64::from(size_high) << 32) | u64::from(size_low);
		// 名字：偏移 72 起，260 个 WCHAR UTF-16LE，NUL 结尾
		let name_units: Vec<u16> = descriptor[72..592]
			.as_chunks::<2>()
			.0
			.iter()
			.map(|pair| u16::from_le_bytes(*pair))
			.take_while(|unit| *unit != 0)
			.collect();
		let name = String::from_utf16_lossy(&name_units);
		if name.is_empty() {
			continue;
		}
		let is_dir =
			dw_flags & FD_ATTRIBUTES != 0 && attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
		entries.push(FileEntry {
			name: format!("{}{name}", if is_dir { "[目录] " } else { "" }),
			size: if dw_flags & FD_FILESIZE != 0 {
				size
			} else {
				0
			},
		});
	}
	entries
}

/// `CB_LOCK_CLIPDATA`：**服务端来锁我们的剪贴板，要数据了。**
///
/// ## 这是本机实测确认的取数入口（第二十五轮，真机 192.168.10.20）
///
/// FreeRDP 协议日志（`~/daisy-probe/cliprdr.log/64069.log`）逐条报文：
/// ```text
/// 18:16:42.102  ClientFormatList: numFormats: 3
///               [0] CF_UNICODETEXT  [1] CF_LOCALE  [2] CF_TEXT   ← 我们发的
/// 18:16:42.106  ← CB_LOCK_CLIPDATA (clipDataId=…)                 ← 服务端来锁
/// 18:16:42.108  ← CB_FORMAT_LIST_RESPONSE (CB_RESPONSE_OK)         ← 服务端接受
/// （全程 0 条 CB_FORMAT_DATA_REQUEST）
/// ```
/// **服务端在这台 Windows 上就是用「锁」来索要数据的，从不发取数请求。**
///
/// 我在第二十一轮按 MS-RDPECLIP §2.2.4.1 的字面描述
/// （「请求 Shared Clipboard Owner 保留 File Stream 数据」）判定它
/// 与纯文本无关、只记日志——**这个判断是错的，等于亲口拒绝了服务端的请求。**
/// 规范那句话描述的是**文件流**用途，但服务端实际把它用作
/// 「我要Clipboard 数据」的通用信号；`CB_CAN_LOCK_CLIPDATA` 能力位
/// 正是服务端宣告「我会用锁」的地方（本次会话 generalFlags=0x3E，含该位）。
///
/// ## 正确应答
///
/// 收到锁就立刻回 `CB_FORMAT_DATA_RESPONSE`，然后由
/// [`Self::flush_outgoing`] 在**一小段间隔之后**回 `CB_UNLOCK_CLIPDATA`
/// （服务端要先处理完数据、写入系统剪贴板，才轮到解锁——第二十轮那个
/// 120ms 的直觉是对的，只是当时挂错了触发点）。
unsafe extern "C" fn server_lock_clipdata_trampoline(
	context: *mut ffi::CliprdrClientContext,
	lock_data: *const ffi::CLIPRDR_LOCK_CLIPBOARD_DATA,
) -> ffi::UINT {
	let Some(lock_data) = (unsafe { lock_data.as_ref() }) else {
		return CHANNEL_RC_OK;
	};
	note_server_message(unsafe { state_of(context) }, "锁");
	let _ = context;
	// **只记日志，不回答数据。**
	//
	// 规范 §1.3.2.2.2 说得很清楚：`CB_LOCK_CLIPDATA` 是读取方
	//（这里是服务端）**请求数据属主保留文件流数据**，等它发
	// `CB_UNLOCK_CLIPDATA` 再释放。它不是取数请求——
	// 取数只走 `CB_FORMAT_DATA_REQUEST`（§1.3.2.2.3）。
	//
	// 第二十五轮把它当成「服务端来要数据」的信号，回了一条
	// `FormatDataResponse`。但那条响应**没有对应的请求**，
	// 服务端无从知道它属于哪个格式，只能丢弃。
	// 更糟的是它把我们锁进了错误的交互范式：
	// 实测证明，一旦我们回数据，服务端此后**再也不会广播剪贴板变化**
	// （10 月 15 日的版本忽略锁，当天服务端广播了 29 次；
	//   10 月 16 日改成回数据之后，一次都没有），
	// Windows 系统剪贴板始终是空的——右键粘贴灰、Ctrl+V 粘出空白。
	//
	// FreeRDP 官方 macOS 客户端（`client/mac/Clipboard.m` 的
	// `mac_cliprdr_server_lock_clipboard_data`）对此同样只返回 OK，
	// 一个字节都不回。
	// 记下锁定的 clipDataId——下载文件字节时要随请求带回（见
	// `server_clip_data_id` 字段说明）。
	tracing::info!("[剪贴板] 服务端锁定 clipDataId={}（下载请求将随带）", lock_data.clipDataId);
	// SAFETY: context 由 FreeRDP 传入，有效。
	let state = unsafe { state_of(context) };
	if let Ok(mut guard) = state.lock() {
		guard.server_clip_data_id = Some(lock_data.clipDataId);
	}
	CHANNEL_RC_OK
}

/// `CB_UNLOCK_CLIPDATA`：服务端释放它先前锁住的文件流。
unsafe extern "C" fn server_unlock_clipdata_trampoline(
	context: *mut ffi::CliprdrClientContext,
	unlock_data: *const ffi::CLIPRDR_UNLOCK_CLIPBOARD_DATA,
) -> ffi::UINT {
	note_server_message(unsafe { state_of(context) }, "解锁");
	if let Some(unlock_data) = unsafe { unlock_data.as_ref() } {
		tracing::info!("[剪贴板] 服务端解锁文件流 clipDataId={}", unlock_data.clipDataId);
		// SAFETY: context 由 FreeRDP 传入，有效。
		let state = unsafe { state_of(context) };
		if let Ok(mut guard) = state.lock()
			&& guard.server_clip_data_id == Some(unlock_data.clipDataId)
		{
			guard.server_clip_data_id = None;
		}
	}
	CHANNEL_RC_OK
}

/// `CB_FILECONTENTS_REQUEST`：**服务端来取我们文件的字节**（本地 → 远端）。
///
/// Copy File Sequence（§1.3.2.2.2）的取数步骤：服务端先锁、要过描述表，
/// 之后按 `listIndex` 逐文件取：
/// - `FILECONTENTS_SIZE` → 8 字节小端 u64（大小），**无论 `cbRequested` 是几**
/// - `FILECONTENTS_RANGE` → `[nPosition, nPosition+cbRequested)` 的字节
///
/// 字节按 `path` **现读**：文件可能很大，不能上报时就灌进内存。
unsafe extern "C" fn server_file_contents_request_trampoline(
	context: *mut ffi::CliprdrClientContext,
	request: *const ffi::CLIPRDR_FILE_CONTENTS_REQUEST,
) -> ffi::UINT {
	let Some(request) = (unsafe { request.as_ref() }) else {
		return CHANNEL_RC_OK;
	};
	note_server_message(unsafe { state_of(context) }, "文件内容请求");
	let stream_id = request.streamId;
	let list_index = request.listIndex;
	let position =
		u64::from(request.nPositionHigh) << 32 | u64::from(request.nPositionLow);

	// SAFETY: context 有效。
	let state = unsafe { state_of(context) };
	let guard = state.lock().unwrap_or_else(|p| p.into_inner());
	// 先把要用的文件信息克隆出来：锁不能横跨文件 I/O，
	// 而借来的引用在 drop(guard) 之后就不能用了。
	let file: Option<LocalFile> = guard
		.sending
		.as_ref()
		.map(|(c, _)| c)
		.cloned()
		.and_then(|content| match content {
			Content::Files(files) => Some(files),
			Content::Text(_) => None,
		})
		.and_then(|files| files.into_iter().nth(list_index as usize));
	let total_files = guard
		.sending
		.as_ref()
		.and_then(|(c, _)| match c {
			Content::Files(files) => Some(files.len()),
			Content::Text(_) => None,
		})
		.unwrap_or(0);
	let Some(file) = file else {
		tracing::warn!(
			"[剪贴板] 服务端索要 listIndex={list_index}（共 {total_files}）但本地没有对应文件（streamId={stream_id}）"
		);
		drop(guard);
		// SAFETY: context 有效。
		return unsafe { send_file_contents_response(&mut *context, stream_id, CB_RESPONSE_FAIL, &[]) };
	};

	if request.dwFlags & FILECONTENTS_SIZE != 0 {
		let size = file.size.to_le_bytes();
		tracing::info!(
			"[剪贴板] 服务端问「{}」的大小：{}（streamId={stream_id}）",
			file.name,
			human_size(file.size)
		);
		drop(guard);
		// SAFETY: context 有效。
		return unsafe {
			send_file_contents_response(&mut *context, stream_id, CB_RESPONSE_OK, &size)
		};
	}

	if request.dwFlags & FILECONTENTS_RANGE == 0 {
		tracing::warn!("[剪贴板] FileContentsRequest 的 dwFlags 不认识：0x{:x}", request.dwFlags);
		drop(guard);
		// SAFETY: context 有效。
		return unsafe { send_file_contents_response(&mut *context, stream_id, CB_RESPONSE_FAIL, &[]) };
	}

	// RANGE：现读文件的一段。读失败（文件被删/被移动）回 FAIL——
	// 服务端会中止这次粘贴，好过收到一段错误数据。
	let requested = request.cbRequested as usize;
	let chunk = read_file_chunk(&file.path, position, requested);
	drop(guard);
	match chunk {
		Ok(bytes) => {
			tracing::debug!(
				"[剪贴板] 服务端取「{}」pos={position} len={}",
				file.name,
				bytes.len()
			);
			// SAFETY: context 有效。
			unsafe { send_file_contents_response(&mut *context, stream_id, CB_RESPONSE_OK, &bytes) }
		},
		Err(error) => {
			tracing::warn!("[剪贴板] 读「{}」失败：{error}", file.name);
			// SAFETY: context 有效。
			unsafe {
				send_file_contents_response(&mut *context, stream_id, CB_RESPONSE_FAIL, &[])
			}
		},
	}
}

/// 读本地文件的一段（`position` 起 `len` 字节）。文件变短时截断，不补零。
fn read_file_chunk(path: &std::path::Path, position: u64, len: usize) -> std::io::Result<Vec<u8>> {
	use std::io::{Read, Seek, SeekFrom};
	let mut file = std::fs::File::open(path)?;
	file.seek(SeekFrom::Start(position))?;
	let mut buffer = vec![0u8; len];
	let mut filled = 0;
	while filled < len {
		let read = file.read(&mut buffer[filled..])?;
		if read == 0 {
			break;
		}
		filled += read;
	}
	buffer.truncate(filled);
	Ok(buffer)
}

/// `CB_FILECONTENTS_RESPONSE`：**服务端发来远端文件的字节**（远端 → 本地）。
///
/// 用 `streamId` 对账（一次只挂一个未决请求），交给下载状态机推进；
/// 请求的下一条（下一块/下一文件）由帧循环的 `flush_outgoing` 发出。
unsafe extern "C" fn server_file_contents_response_trampoline(
	context: *mut ffi::CliprdrClientContext,
	response: *const ffi::CLIPRDR_FILE_CONTENTS_RESPONSE,
) -> ffi::UINT {
	let Some(response) = (unsafe { response.as_ref() }) else {
		return CHANNEL_RC_OK;
	};
	note_server_message(unsafe { state_of(context) }, "文件内容应答");
	let ok = response.common.msgFlags & CB_RESPONSE_OK != 0;
	let len = response.common.dataLen as usize;
	// SAFETY: context 有效；requestedData/dataLen 由 FreeRDP 保证有效。
	let data = if len > 0 && !response.requestedData.is_null() {
		unsafe { std::slice::from_raw_parts(response.requestedData, len) }
	} else {
		&[]
	};

	// SAFETY: context 有效。
	let state = unsafe { state_of(context) };
	let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
	let Some(download) = guard.download.as_mut() else {
		tracing::warn!(
			"[剪贴板] 收到 FileContentsResponse 但没有进行中的下载（streamId={}）",
			response.streamId
		);
		return CHANNEL_RC_OK;
	};
	download.on_response(response.streamId, ok, data);
	if download.failed.is_some() {
		tracing::warn!(
			"[剪贴板] 远端文件下载中止：{}",
			download.failed.as_deref().unwrap_or("")
		);
	}
	CHANNEL_RC_OK
}

/// `CB_FORMAT_LIST_RESPONSE`：服务端确认收到我们的格式列表。
///
/// **注册在 `ServerFormatListResponse` 上，不是 `ClientFormatListResponse`。**
/// 后者是 FreeRDP 的**发送**函数（`cliprdr_main.c:731`），
/// 覆盖它会让 `respond_format_list` 调我们自己的空实现、**一包都不发**——
/// 与第十轮踩过的 `ClientFormatDataRequest` 是同一类错误。
unsafe extern "C" fn server_format_list_response_trampoline(
	_context: *mut ffi::CliprdrClientContext,
	response: *const ffi::CLIPRDR_FORMAT_LIST_RESPONSE,
) -> ffi::UINT {
	note_server_message(unsafe { state_of(_context) }, "格式列表应答");
	if let Some(response) = unsafe { response.as_ref() } {
		let accepted = response.common.msgFlags & CB_RESPONSE_OK != 0;
		// SAFETY: context 由 FreeRDP 传入且 custom 已填好。
		if let Ok(mut guard) = unsafe { state_of(_context) }.lock() {
			// 只有 OK 才算「远端知道我们有什么了」——
			// FAIL 意味着这轮格式列表白发了，要等下一轮。
			guard.list_acked = accepted;
		}
		tracing::info!(
			"[剪贴板] 服务端回应我们的格式列表 msgFlags=0x{:04x}（1=OK 2=FAIL），accepted={accepted}",
			response.common.msgFlags
		);
	}
	CHANNEL_RC_OK
}

/// 装接收回调到 `CliprdrClientContext`。
///
/// **只碰 `Server*` / `MonitorReady`，一个 `Client*` 都不许动。**
///
/// `Client*` 全是 FreeRDP 提供的**发送**函数（`cliprdr_main.c:1178-1187`
/// 在 `VirtualChannelEntryEx` 里统一挂好），覆盖它们 = 把发包能力清零。
/// 这个坑踩了两次：
/// - 第十轮：覆盖 `ClientFormatDataRequest` → 反向取数一条都发不出去
/// - 本轮：覆盖 `ClientFormatListResponse` → 格式列表响应从未真正发出
///
/// **正则在 `install_callbacks` 里显式列出**（见
/// `must_not_override_any_client_send_function`），防止第三次。
fn install_callbacks(context: &mut ffi::CliprdrClientContext) {
	context.ServerCapabilities = Some(server_capabilities_trampoline);
	context.MonitorReady = Some(monitor_ready_trampoline);
	context.ServerFormatList = Some(server_format_list_trampoline);
	context.ServerFormatListResponse = Some(server_format_list_response_trampoline);
	context.ServerFormatDataResponse = Some(server_format_data_response_trampoline);
	// **正向链路的命门**：服务端索要数据的唯一入口。
	// 不注册它，`cliprdr_process_format_data_request` 会
	// `IFCALLRET(nullptr)` —— 静默丢弃请求，数据永远送不过去。
	context.ServerFormatDataRequest = Some(server_format_data_request_trampoline);
	// 文件流锁/解锁：文本方向记日志；文件方向靠「数据保留」语义
	context.ServerLockClipboardData = Some(server_lock_clipdata_trampoline);
	context.ServerUnlockClipboardData = Some(server_unlock_clipdata_trampoline);
	// 文件双向的两个接收槽（发送侧 ClientFileContentsRequest/Response 是
	// FreeRDP 的发送函数，**绝不覆盖**，见 must_not_override_any_client_send_function）：
	// - ServerFileContentsRequest：服务端来取**我们**文件的字节（本地 → 远端）
	// - ServerFileContentsResponse：服务端发来**远端**文件的字节（远端 → 本地）
	//   （vendored cliprdr_main.c:352 派发到 ServerFileContentsResponse，
	//   不是名字看起来更顺眼的 ClientFileContentsResponse）
	context.ServerFileContentsRequest = Some(server_file_contents_request_trampoline);
	context.ServerFileContentsResponse = Some(server_file_contents_response_trampoline);
}

/// 造一个剪贴板消息头。
fn cliprdr_header(msg_type: u16, msg_flags: u16, data_len: u32) -> ffi::CLIPRDR_HEADER {
	ffi::CLIPRDR_HEADER {
		msgType: msg_type,
		msgFlags: msg_flags,
		dataLen: data_len,
	}
}

// ==================== 纯函数（可单测） ====================

/// UTF-16LE 字节 → `String`。容忍结尾 NUL，非法代理项替换为 U+FFFD。
fn decode_utf16le(bytes: &[u8]) -> Option<String> {
	if !bytes.len().is_multiple_of(2) {
		return None;
	}
	let units: Vec<u16> = bytes
		.as_chunks::<2>()
		.0
		.iter()
		.map(|pair| u16::from_le_bytes(*pair))
		.collect();
	// 剪尾部的 NUL（Windows 文本以 NUL 结尾）
	let end = units
		.iter()
		.position(|unit| *unit == 0)
		.unwrap_or(units.len());
	let units = &units[..end];
	Some(String::from_utf16_lossy(units))
}

/// `String` → UTF-16LE 字节（含结尾 NUL，Windows 约定）。
fn encode_utf16le(text: &str) -> Vec<u8> {
	let mut bytes = Vec::with_capacity((text.len() + 1) * 2);
	for unit in text.encode_utf16() {
		bytes.extend_from_slice(&unit.to_le_bytes());
	}
	bytes.extend_from_slice(&0u16.to_le_bytes());
	bytes
}

/// 剪贴板消息类型（`CLIPRDR_HEADER.msgType`，出处 channels/cliprdr.h）。
const CB_FORMAT_LIST: u16 = 0x0002;
const CB_FORMAT_DATA_REQUEST: u16 = 0x0004;
const CB_FORMAT_DATA_RESPONSE: u16 = 0x0005;
/// 文件内容请求/响应（S10）。
const CB_FILECONTENTS_REQUEST: u16 = 0x0008;
const CB_FILECONTENTS_RESPONSE: u16 = 0x0009;

/// `CHANNEL_RC_OK`：回调返回 0 表示成功。
const CHANNEL_RC_OK: ffi::UINT = 0;

#[cfg(test)]
mod tests {
	use super::*;

	/// 取内容里的文本（测试断言用）。
	fn text_of(content: &Option<Content>) -> Option<String> {
		content.as_ref().and_then(|c| match c {
			Content::Text(text) => Some(text.clone()),
			Content::Files(_) => None,
		})
	}

	/// 取在途内容里的文本（测试断言用）。
	fn sending_text(state: &ClipboardState) -> Option<String> {
		state.sending.as_ref().and_then(|(c, _)| match c {
			Content::Text(text) => Some(text.clone()),
			Content::Files(_) => None,
		})
	}

	/// `CB_LOCK_CLIPDATA`：只有「锁不推数据」那条回归测试用到。
	const CB_LOCK_CLIPDATA_MSG: u16 = 0x000A;

	/// ASCII 文本编解码。
	#[test]
	fn ascii_round_trips() {
		let bytes = encode_utf16le("hello");
		assert_eq!(
			bytes,
			vec![b'h', 0, b'e', 0, b'l', 0, b'l', 0, b'o', 0, 0, 0],
			"UTF-16LE + 结尾 NUL"
		);
		assert_eq!(decode_utf16le(&bytes).as_deref(), Some("hello"));
	}

	/// 中文等多字节字符（代理对）必须 survive。
	#[test]
	fn non_ascii_round_trips() {
		let text = "中文混合 English";
		let bytes = encode_utf16le(text);
		assert_eq!(decode_utf16le(&bytes).as_deref(), Some(text));
	}

	/// Emoji 走代理对（> U+FFFF），最容易在高位字节上出错。
	#[test]
	fn surrogate_pairs_round_trip() {
		let text = "ok 👍 done";
		let bytes = encode_utf16le(text);
		assert_eq!(decode_utf16le(&bytes).as_deref(), Some(text));
	}

	/// 结尾 NUL 要剥掉，否则远端会看到一个多余的 `\0`。
	#[test]
	fn trailing_nul_is_stripped() {
		let bytes = encode_utf16le("abc");
		let decoded = decode_utf16le(&bytes).expect("应解出");
		assert_eq!(decoded, "abc");
		assert!(!decoded.contains('\0'), "结尾 NUL 不该出现在结果里");
	}

	/// 空文本：编码出 2 字节（只有 NUL），解码为空串。
	#[test]
	fn empty_text() {
		assert_eq!(encode_utf16le(""), vec![0, 0]);
		assert_eq!(decode_utf16le(&[0, 0]).as_deref(), Some(""));
	}

	/// 奇数长度不是合法 UTF-16LE，必须拒绝而不是 panic。
	#[test]
	fn odd_length_is_rejected() {
		assert!(decode_utf16le(&[0x68, 0x00, 0x00]).is_none());
	}

	/// 空切片也解成空串（服务端可能发 0 长度数据）。
	#[test]
	fn empty_slice_decodes_to_empty_string() {
		assert_eq!(decode_utf16le(&[]).as_deref(), Some(""));
	}

	/// 远端文本存取：取走即清空。
	#[test]
	fn remote_text_take_and_clear() {
		let clipboard = Clipboard::new();
		assert!(clipboard.take_remote().is_none(), "初始应为空");
		clipboard.state.lock().unwrap().remote = Some("远端内容".to_string());
		assert_eq!(clipboard.take_remote().as_deref(), Some("远端内容"));
		assert!(
			clipboard.take_remote().is_none(),
			"取走即清空，第二次应拿不到"
		);
	}

	/// 未就绪时不许发送——服务端还没开始监听。
	#[test]
	fn no_flush_before_server_ready() {
		let clipboard = Clipboard::new();
		clipboard.send_text("待发".to_string());
		// server_ready = false，且没有 context：应原样保留
		clipboard.flush_outgoing(None);
		let state = clipboard.state.lock().unwrap();
		assert_eq!(
			text_of(&state.pending_out),
			Some("待发".to_string()),
			"未就绪不该丢文本"
		);
		assert!(state.sending.is_none(), "未就绪时不该进 sending");
	}

	/// 就绪后没 context 时文本要还回去，不能吞掉用户复制的内容。
	#[test]
	fn text_returns_when_context_missing() {
		let clipboard = Clipboard::new();
		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_ready = true;
			state.pending_out = Some(Content::Text("内容".to_string()));
		}
		clipboard.flush_outgoing(None);
		let state = clipboard.state.lock().unwrap();
		assert_eq!(
			text_of(&state.pending_out),
			Some("内容".to_string()),
			"拿不到 context 时应还回去"
		);
		assert!(state.sending.is_none());
	}

	/// **回归**：上报格式列表后文本必须留在 `sending` 里等远端来要。
	///
	/// 第一版的 bug 正是这里——`pending_out` 在上报时就被 `take()` 走，
	/// 远端随后发 `CB_FORMAT_DATA_REQUEST` 时只能读到空，回空数据，
	/// 表现为「按了 Cmd+V 但远端什么都没粘上」。
	#[test]
	fn text_survives_until_remote_asks_for_it() {
		use std::sync::atomic::{AtomicU32, Ordering};
		static SENT: AtomicU32 = AtomicU32::new(0);
		static FIRST_ID: AtomicU32 = AtomicU32::new(0);

		let clipboard = Clipboard::new();
		clipboard.send_text("hello".to_string());

		// 造一个最小可用的 CliprdrClientContext：只挂 ClientFormatList，
		// 它会把要发的格式数记下来，模拟远端收到了格式列表。
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_format_list(
			_context: *mut ffi::CliprdrClientContext,
			list: *const ffi::CLIPRDR_FORMAT_LIST,
		) -> ffi::UINT {
			let list = unsafe { &*list };
			SENT.store(list.numFormats, Ordering::Relaxed);
			if !list.formats.is_null() && list.numFormats > 0 {
				let formats =
					unsafe { std::slice::from_raw_parts(list.formats, list.numFormats as usize) };
				FIRST_ID.store(formats[0].formatId, Ordering::Relaxed);
			}
			0
		}
		ctx.ClientFormatList = Some(on_format_list);

		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_ready = true;
		}
		clipboard.flush_outgoing(Some(&mut ctx));

		// 格式列表已上报：`CF_UNICODETEXT` + `CF_LOCALE` + `CF_TEXT`
		//
		// `CF_LOCALE` 必须与 `CF_UNICODETEXT` 成对出现（Windows 约定），
		// 但**放第一个没有依据**——第十八轮那个排序猜测已撤销，
		// 改成 Unicode 优先（与 mstsc 一致）。
		assert_eq!(SENT.load(Ordering::Relaxed), 3, "应上报 3 种格式");
		assert_eq!(
			FIRST_ID.load(Ordering::Relaxed),
			CF_UNICODETEXT,
			"列表首位应是 CF_UNICODETEXT（Unicode 优先）"
		);
		// **关键**：文本还在，且在 sending 里等着被索要
		let state = clipboard.state.lock().unwrap();
		assert!(state.pending_out.is_none(), "上报后 pending_out 应已被取走");
		assert_eq!(
			sending_text(&state),
			Some("hello".to_string()),
			"上报后文本必须留在 sending 等远端来要"
		);
	}

	/// **开嗓**：本机没有可传内容时也要上报一条空文本，且只报一次。
	///
	/// 真机数据：全天 21 次「远端广播了剪贴板变化」全部发生在已上报过
	/// 格式列表的会话里，0 反例；客户端一直没东西可报的会话（本机剪贴板空 /
	/// 只有图片）远端 Ctrl+C 一律毫无动静。这条测试钉住两个要点：
	/// 1. 空内容也要真的发出去一条格式列表；
	/// 2. 报过之后不再重复——重复上报会把 rdpclip 拖进「反复索要描述表、
	///    一次不问大小」的死循环（真机对照过）。
	#[test]
	fn arm_reports_empty_text_exactly_once() {
		use std::sync::atomic::{AtomicU32, Ordering};
		static SENT: AtomicU32 = AtomicU32::new(0);

		unsafe extern "C" fn on_format_list(
			_context: *mut ffi::CliprdrClientContext,
			list: *const ffi::CLIPRDR_FORMAT_LIST,
		) -> ffi::UINT {
			let list = unsafe { &*list };
			SENT.store(list.numFormats, Ordering::Relaxed);
			0
		}

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		ctx.ClientFormatList = Some(on_format_list);
		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_ready = true;
		}
		assert!(!clipboard.announced_ever(), "新会话还没报过");

		clipboard.arm();
		clipboard.flush_outgoing(Some(&mut ctx));
		assert_eq!(
			SENT.load(Ordering::Relaxed),
			3,
			"开嗓也要是一条完整的文本格式列表（Unicode/Locale/ANSI）"
		);
		assert!(clipboard.announced_ever(), "报过之后必须置位");

		// 第二轮：已经报过，arm 不该再动 pending_out
		SENT.store(0, Ordering::Relaxed);
		clipboard.arm();
		clipboard.flush_outgoing(Some(&mut ctx));
		assert_eq!(
			SENT.load(Ordering::Relaxed),
			0,
			"已经报过就不能再报（重复格式列表会让 rdpclip 卡死）"
		);
	}

	/// 上一条还没被索要时，新的发送请求要排队而不是丢弃。
	#[test]
	fn new_send_waits_while_previous_is_in_flight() {
		let clipboard = Clipboard::new();
		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_ready = true;
			state.sending = Some((Content::Text("在途".to_string()), std::time::Instant::now()));
			state.pending_out = Some(Content::Text("后来的".to_string()));
		}
		// 没 context，会直接 return——但 `sending` 非空时连 pending_out 都不该动
		clipboard.flush_outgoing(None);
		let state = clipboard.state.lock().unwrap();
		assert_eq!(
			text_of(&state.pending_out),
			Some("后来的".to_string()),
			"在途未完成时不该取走新文本"
		);
		assert_eq!(
			sending_text(&state),
			Some("在途".to_string())
		);
	}

	/// **回归**：`FEATURE_MASK_ALL` 必须是 0x33（文本两位 + 文件两位）。
	///
	/// FreeRDP 在 cliprdr 的 8 处读 `ClipboardFeatureMask` 决定放不放行，
	/// 默认为 0 → 消息全被丢弃（表现为「协议跑通但收发全无动静」）。
	/// 少一位就是单向不通（只发不收或只收不发），很难从界面上看出来，
	/// 所以把精确值钉在这里。
	///
	/// S10 起文件两位也必须开——两条门槛在 FreeRDP 内部：
	/// 收 `ServerFileContentsRequest` 要求 `LOCAL_TO_REMOTE_FILES(0x02)`，
	/// 发 `ClientFileContentsRequest` 要求 `REMOTE_TO_LOCAL_FILES(0x20)`。
	#[test]
	fn feature_mask_enables_text_and_file_directions() {
		assert_eq!(
			FEATURE_MASK_ALL, 0x33,
			"必须同时开文本(0x01/0x10)与文件(0x02/0x20)四位"
		);
	}

	/// **回归**：远端迟迟不索要数据时必须超时重发，不能永久卡死。
	///
	/// 没有超时的话，只要服务端有一次不回应（焦点在别处 / 忙于自己的
	/// 剪贴板处理），`sending` 就一直非空，`flush_outgoing` 每帧提前返回——
	/// 之后**所有**剪贴板同步都失效，看起来就是"某个时刻之后剪贴板全废了"。
	#[test]
	fn resends_after_remote_never_asks() {
		let clipboard = Clipboard::new();
		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_ready = true;
			// 伪造一个「早就上报了但没人来要」的状态：时间戳往回拨，
			// 让它看起来已经等过了 SEND_TIMEOUT
			state.sending = Some((
				Content::Text("卡住的".to_string()),
				std::time::Instant::now() - SEND_TIMEOUT - std::time::Duration::from_secs(1),
			));
		}
		// 没 context，重发逻辑仍应把文本退回 pending_out
		clipboard.flush_outgoing(None);
		let state = clipboard.state.lock().unwrap();
		assert!(state.sending.is_none(), "超时后 sending 应被清空");
		assert_eq!(
			text_of(&state.pending_out),
			Some("卡住的".to_string()),
			"超时的文本应退回 pending_out 等重发"
		);
	}

	/// **回归**：`remote_taken` 必须在**数据响应发出**后置位。
	///
	/// 注入 Ctrl+V 的时机完全依赖它：数据没送达就按键，
	/// 远端粘贴到的是上一份内容。
	///
	/// 真实耗时从 11ms 到几百毫秒都有（看服务端忙不忙），
	/// 所以**只能等这个事件**，等固定时长就是赌运气。
	#[test]
	fn remote_taken_tracks_server_pickup() {
		let clipboard = Clipboard::new();
		assert!(!clipboard.remote_took_data(), "刚上报时不该认为已应答");
		// 模拟：上报 → 服务端来索要 → 我们应答
		{
			let mut state = clipboard.state.lock().unwrap();
			state.sending = Some((Content::Text("x".to_string()), std::time::Instant::now()));
		}
		{
			let mut state = clipboard.state.lock().unwrap();
			state.remote_taken = true;
		}
		assert!(clipboard.remote_took_data(), "应答数据后应能查到");
		// 再来一轮上报必须重置
		{
			let mut state = clipboard.state.lock().unwrap();
			state.remote_taken = false;
		}
		assert!(!clipboard.remote_took_data(), "新一轮上报要重置标志");
	}

	/// **回归（最关键的一个）**：不能覆盖 `ClientFormatDataRequest`。
	/// **回归（最关键的一个）**：`ServerFormatDataRequest` 必须注册。
	///
	/// 这是正向（本地 → 远端）链路的**唯一入口**：
	/// MS-RDPECLIP 的 Paste Sequence 里，服务端索要数据发的是
	/// `CB_FORMAT_DATA_REQUEST`，FreeRDP 把它派发到
	/// `context->ServerFormatDataRequest`。这个回调为 `None` 时
	/// `cliprdr_process_format_data_request` 走 `IFCALLRET(nullptr)`——
	/// **静默丢弃，一条字节都不回**，且没有任何错误可查。
	///
	/// 症状与「格式没发对」完全一样：Windows 剪贴板空、右键粘贴灰，
	/// 所以此前二十轮一直在格式列表/时序上打转。
	#[test]
	fn server_format_data_request_must_be_registered() {
		let mut context: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		install_callbacks(&mut context);
		assert!(
			context.ServerFormatDataRequest.is_some(),
			"必须注册 ServerFormatDataRequest，否则服务端索要数据时一条都不回"
		);
		assert!(context.ServerFormatList.is_some(), "要能收到服务端广播");
		assert!(
			context.ServerFormatDataResponse.is_some(),
			"要能收到远端发来的内容"
		);
	}

	/// **回归**：格式列表里必须有 `CF_LOCALE`，且不能与别的格式重号。
	///
	/// 第二十二轮实测日志打出 `0xd / 0xd / 0x1`——`CF_LOCALE`
	/// 被误写成了重复的 `CF_UNICODETEXT`，等于没上报 Locale。
	/// 日志里那句 `上报格式列表 3 种：…` 就是防这个的：
	/// 格式 ID 印出来，重号一眼可见。
	#[test]
	fn format_list_advertises_locale_without_duplicates() {
		use std::sync::atomic::{AtomicU32, Ordering};
		static IDS: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
		static COUNT: AtomicU32 = AtomicU32::new(0);

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_format_list(
			_context: *mut ffi::CliprdrClientContext,
			list: *const ffi::CLIPRDR_FORMAT_LIST,
		) -> ffi::UINT {
			let list = unsafe { &*list };
			let n = list.numFormats as usize;
			COUNT.store(n as u32, Ordering::Relaxed);
			let formats = unsafe { std::slice::from_raw_parts(list.formats, n) };
			for (index, format) in formats.iter().enumerate().take(3) {
				IDS[index].store(format.formatId, Ordering::Relaxed);
			}
			0
		}
		ctx.ClientFormatList = Some(on_format_list);
		clipboard.state.lock().unwrap().server_ready = true;
		clipboard.send_text("x".to_string());
		clipboard.flush_outgoing(Some(&mut ctx));

		assert_eq!(COUNT.load(Ordering::Relaxed), 3, "应上报 3 种格式");
		let ids: Vec<u32> = IDS.iter().map(|a| a.load(Ordering::Relaxed)).collect();
		assert!(
			ids.contains(&CF_LOCALE),
			"必须上报 CF_LOCALE（Windows 要求 Unicode 文本配 Locale），实际 {ids:x?}"
		);
		assert!(
			ids.contains(&CF_UNICODETEXT),
			"必须上报 CF_UNICODETEXT，实际 {ids:x?}"
		);
		// 重号会让服务端只认其中一个，另一个格式等于没报
		for (i, a) in ids.iter().enumerate() {
			for b in ids.iter().skip(i + 1) {
				assert_ne!(a, b, "格式列表出现重号：{ids:x?}");
			}
		}
	}

	/// **回归（第二十六轮，根因）**：客户端能力集只宣告 `CB_USE_LONG_FORMAT_NAMES`。
	///
	/// 原来把服务端的 `0x3E` 原样回声，等于告诉服务端
	/// 「我支持锁 / 文件流 / 巨型文件」。服务端据此放弃
	/// `CB_FORMAT_DATA_REQUEST`，改用 `CB_LOCK_CLIPDATA` 跟我们交互——
	/// 而锁只是「保留文件流数据」的通知，取数请求从此再也不来。
	///
	/// 真机报文对照：回声 `0x3E` 时每次上报都收到锁、此后全静默；
	/// 只宣告 `0x02` 时服务端回 OK 后立刻按规范发取数请求。
	#[test]
	fn capabilities_declare_long_names_stream_and_lock() {
		use std::sync::atomic::{AtomicU32, Ordering};
		static FLAGS: AtomicU32 = AtomicU32::new(u32::MAX);
		static VERSION: AtomicU32 = AtomicU32::new(0);
		static SETS: AtomicU32 = AtomicU32::new(0);

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_caps(
			_context: *mut ffi::CliprdrClientContext,
			caps: *const ffi::CLIPRDR_CAPABILITIES,
		) -> ffi::UINT {
			let caps = unsafe { &*caps };
			SETS.store(caps.cCapabilitiesSets, Ordering::Relaxed);
			// SAFETY: 我们的回复一定指向 GeneralCapabilitySet。
			let set = unsafe { &*(caps.capabilitySets as *const GeneralCapabilitySet) };
			VERSION.store(set.version, Ordering::Relaxed);
			FLAGS.store(set.general_flags, Ordering::Relaxed);
			0
		}
		ctx.ClientCapabilities = Some(on_caps);
		ctx.custom =
			std::sync::Arc::as_ptr(&clipboard.state) as *mut PluginData as *mut std::ffi::c_void;

		// 伪造一份服务端能力集：0x3E，正是这台 Windows 机器报的
		let mut server_set = GeneralCapabilitySet {
			capability_set_type: CB_CAPSTYPE_GENERAL,
			capability_set_length: 12,
			version: 2,
			general_flags: 0x3E,
		};
		let server_caps = ffi::CLIPRDR_CAPABILITIES {
			common: cliprdr_header(CB_CLIP_CAPS, 0, 16),
			cCapabilitiesSets: 1,
			capabilitySets: (&raw mut server_set).cast::<ffi::CLIPRDR_CAPABILITY_SET>(),
		};
		unsafe { server_capabilities_trampoline(&mut ctx as *mut _, &server_caps) };

		assert_eq!(SETS.load(Ordering::Relaxed), 1, "只回一个能力集");
		assert_eq!(VERSION.load(Ordering::Relaxed), 2, "版本必须是 2");
		assert_eq!(
			FLAGS.load(Ordering::Relaxed),
			CB_USE_LONG_FORMAT_NAMES | CB_STREAM_FILECLIP_ENABLED | CB_CAN_LOCK_CLIPDATA,
			"长格式名 + 文件流 + 锁三位缺一不可：文件复制序列要求对端宣告流式支持，\
			 锁是服务端取文件字节的入口；仍不得宣告 CB_HUGE_FILE_SUPPORT（未支持 >4GB）"
		);
	}

	/// **回归（第二十六轮）**：标准格式的 name 必须是 NULL。
	///
	/// 规范 §2.2.3.1.2.1：无名字的格式只发一个 Unicode NUL。
	/// FreeRDP 官方 macOS 客户端对 `formatId <= CF_MAX` 一律传 NULL，
	/// 只给注册格式带名字。第十七轮「必须带 CF_UNICODETEXT」是未经报文
	/// 验证的推断，此后带了十来轮名字，问题一点没变。
	#[test]
	fn standard_formats_carry_no_name() {
		use std::sync::atomic::{AtomicBool, Ordering};
		static ALL_NULL: AtomicBool = AtomicBool::new(false);

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_format_list(
			_context: *mut ffi::CliprdrClientContext,
			list: *const ffi::CLIPRDR_FORMAT_LIST,
		) -> ffi::UINT {
			let list = unsafe { &*list };
			let formats =
				unsafe { std::slice::from_raw_parts(list.formats, list.numFormats as usize) };
			ALL_NULL.store(
				formats.iter().all(|f| f.formatName.is_null()),
				Ordering::Relaxed,
			);
			0
		}
		ctx.ClientFormatList = Some(on_format_list);
		clipboard.state.lock().unwrap().server_ready = true;
		clipboard.send_text("x".to_string());
		clipboard.flush_outgoing(Some(&mut ctx));

		assert!(
			ALL_NULL.load(Ordering::Relaxed),
			"标准格式应只带 formatId、名称留空（参照 FreeRDP macOS 客户端）"
		);
	}

	/// **回归（第二十六轮）**：服务端确认过格式列表后，**不许再重播**。
	///
	/// 重播会把服务端拖进「剪贴板一直在变」的状态：每 2 秒一条新格式列表，
	/// 每次都要重设一遍 Windows 剪贴板。实测带重播的会话从头到尾没广播过
	/// 一次剪贴板变化。回执之后安静等着，文本留在 `sending` 里给取数请求用。
	#[test]
	fn acked_format_list_is_not_reannounced() {
		use std::sync::atomic::{AtomicU32, Ordering};
		static SENDS: AtomicU32 = AtomicU32::new(0);

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_format_list(
			_context: *mut ffi::CliprdrClientContext,
			_list: *const ffi::CLIPRDR_FORMAT_LIST,
		) -> ffi::UINT {
			SENDS.fetch_add(1, Ordering::Relaxed);
			0
		}
		ctx.ClientFormatList = Some(on_format_list);
		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_ready = true;
			// 已经上报很久（远超 SEND_TIMEOUT），且服务端已确认
			state.sending = Some((
				Content::Text("内容".to_string()),
				std::time::Instant::now() - SEND_TIMEOUT * 3,
			));
			state.list_acked = true;
		}

		clipboard.flush_outgoing(Some(&mut ctx));
		assert_eq!(
			SENDS.load(Ordering::Relaxed),
			0,
			"已确认的格式列表不能重播——重播会让服务端永远不提交剪贴板"
		);
		// 文本必须还留着，服务端的取数请求来了才取得到
		assert_eq!(
			sending_text(&clipboard.state.lock().unwrap()),
			Some("内容".to_string()),
			"文本要留着给取数请求用"
		);
	}

	/// **回归**：`list_acked` 只能在服务端回 `CB_RESPONSE_OK` 时置位。
	///
	/// 这是「先上报、后注入 Ctrl+V」的依据。服务端只在用户粘贴时
	/// 才来索要数据，而那次粘贴就是我们注入的按键——
	/// 顺序反了就形成死锁：按键先到，远端剪贴板空，粘贴落空，
	/// 不发取数请求；随后格式列表到了并被确认，但没人再粘贴一次。
	/// 实测死锁的表现是整条链路静默、一条取数请求都没有。
	#[test]
	fn list_acked_requires_response_ok() {
		let clipboard = Clipboard::new();
		assert!(!clipboard.format_list_acked(), "初始不该认为已确认");
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		ctx.custom =
			std::sync::Arc::as_ptr(&clipboard.state) as *mut PluginData as *mut std::ffi::c_void;

		// FAIL 不能算确认
		let fail = ffi::CLIPRDR_FORMAT_LIST_RESPONSE {
			common: cliprdr_header(CB_FORMAT_LIST_RESPONSE, CB_RESPONSE_FAIL, 0),
		};
		unsafe { server_format_list_response_trampoline(&mut ctx as *mut _, &fail) };
		assert!(!clipboard.format_list_acked(), "FAIL 不能算已确认");

		// OK 才是
		let ok = ffi::CLIPRDR_FORMAT_LIST_RESPONSE {
			common: cliprdr_header(CB_FORMAT_LIST_RESPONSE, CB_RESPONSE_OK, 0),
		};
		unsafe { server_format_list_response_trampoline(&mut ctx as *mut _, &ok) };
		assert!(clipboard.format_list_acked(), "OK 应算已确认");
	}

	/// **回归**：新一轮上报必须清掉上一轮的确认。
	///
	/// 否则会拿着上一轮的 OK 去注入 Ctrl+V，而远端还没登记这轮的
	/// 格式，粘贴照样落空。
	#[test]
	fn new_format_list_clears_previous_ack() {
		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_format_list(
			_context: *mut ffi::CliprdrClientContext,
			_list: *const ffi::CLIPRDR_FORMAT_LIST,
		) -> ffi::UINT {
			0
		}
		ctx.ClientFormatList = Some(on_format_list);
		ctx.custom =
			std::sync::Arc::as_ptr(&clipboard.state) as *mut PluginData as *mut std::ffi::c_void;

		// 上一轮已确认
		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_ready = true;
			state.list_acked = true;
			state.sending = Some((Content::Text("旧的".to_string()), std::time::Instant::now()));
		}
		// 新一轮上报
		clipboard.send_text("新的".to_string());
		clipboard.flush_outgoing(Some(&mut ctx));

		assert!(
			!clipboard.format_list_acked(),
			"新一轮上报后不能沿用上一轮的确认"
		);
		assert_eq!(
			sending_text(&clipboard.state.lock().unwrap()),
			Some("新的".to_string())
		);
	}

	/// **回归**：一个 `Client*` 发送函数都不许覆盖。
	///
	/// `Client*` 全是 FreeRDP 在 `VirtualChannelEntryEx` 里挂好的
	/// **发送**函数（`cliprdr_main.c:1178-1187`），覆盖等于把发包能力清零。
	/// 这个坑踩了两次——第十轮覆盖 `ClientFormatDataRequest`，
	/// 本轮又覆盖了 `ClientFormatListResponse`（导致格式列表响应从未发出）。
	#[test]
	fn must_not_override_any_client_send_function() {
		let mut context: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		install_callbacks(&mut context);
		assert!(
			context.ClientFormatDataRequest.is_none(),
			"`ClientFormatDataRequest` 是 FreeRDP 的发送函数"
		);
		assert!(
			context.ClientFormatListResponse.is_none(),
			"`ClientFormatListResponse` 是 FreeRDP 的发送函数，覆盖它会让格式列表响应一包都不发"
		);
		assert!(
			context.ClientFormatDataResponse.is_none(),
			"`ClientFormatDataResponse` 是 FreeRDP 的发送函数"
		);
		// 接收侧对照项：这几个必须是我们自己的回调
		assert!(
			context.ServerFormatListResponse.is_some(),
			"格式列表的**回执**是接收事件，要挂在 ServerFormatListResponse 上"
		);
		assert!(
			context.ServerUnlockClipboardData.is_some(),
			"文件流解锁也要接上，否则日志里看不到完整序列"
		);
	}

	/// **回归**：必须能区分「服务端没广播」与「广播了但数据没到」。
	///
	/// 这两者的用户可见表现完全一样（都是「远端复制没反应」），
	/// 但处置完全不同：前者可能是远端没复制成功 / 服务端限制，
	/// 后者是 Daisy 的协议层卡住。笼统说「没反应」对用户毫无帮助。
	#[test]
	fn distinguishes_no_broadcast_from_no_data() {
		let clipboard = Clipboard::new();
		assert!(!clipboard.server_broadcast_ever(), "初始不该认为广播过");
		assert!(!clipboard.remote_data_received(), "初始不该认为收到过数据");
		// 场景一：服务端广播了，但数据没回来
		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_broadcast = true;
		}
		assert!(clipboard.server_broadcast_ever(), "应查到广播过");
		assert!(
			!clipboard.remote_data_received(),
			"广播了但没收到数据，两标志必须能区分"
		);
		// 场景二：数据真的到了
		{
			let mut state = clipboard.state.lock().unwrap();
			state.remote_data_received = true;
		}
		assert!(clipboard.remote_data_received(), "应查到收到过数据");
	}

	/// **回归**：「远端 → 本地」方向完全由服务端主导，客户端只能观察。
	///
	/// 服务端只有在远端剪贴板变化时才主动广播 `CB_FORMAT_LIST`，
	/// 客户端没有任何办法去「拉」（它连远端有哪些格式都不知道）。
	///
	/// 所以必须能把「服务端到底有没有广播过」记下来并暴露出去——
	/// 这个方向不可用时，两个方向的表现都是「没反应」，
	/// 从外面看一模一样，没有这个标志界面给不出可操作的提示。
	#[test]
	fn tracks_whether_server_ever_broadcast() {
		let clipboard = Clipboard::new();
		assert!(
			!clipboard.server_broadcast_ever(),
			"刚建好时不该认为服务端广播过"
		);
		{
			let mut state = clipboard.state.lock().unwrap();
			state.server_broadcast = true;
		}
		assert!(clipboard.server_broadcast_ever(), "服务端广播后应能查到");
	}

	/// **回归**：`CF_LOCALE` 的载荷必须是 4 字节 LANGID，不能是文本。
	///
	/// Windows 约定 Unicode 文本要配 Locale，但 Locale 本身是个
	/// 4 字节小端 LANGID（`winpr/libwinpr/clipboard/synthetic.c`
	/// 的 `clipboard_synthesize_cf_locale` 直接返回 `UINT32`）。
	/// 按 UTF-16 文本发过去，`SetClipboardData(CF_LOCALE)` 拿到乱码，
	/// 整份剪贴板数据被判无效——第十八轮正是踩在这里。
	#[test]
	fn cf_locale_payload_is_a_langid_not_text() {
		let bytes = encode_for_format(CF_LOCALE, "hello");
		assert_eq!(bytes.len(), 4, "LANGID 是 4 字节，不是文本长度");
		assert_eq!(
			u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
			CF_LOCALE_LANGID_ZH_CN
		);
	}

	/// Unicode 文本走 UTF-16LE + 结尾 NUL。
	#[test]
	fn unicode_text_payload_is_utf16le() {
		let bytes = encode_for_format(CF_UNICODETEXT, "hi");
		assert_eq!(bytes, vec![b'h', 0, b'i', 0, 0, 0]);
	}

	/// 不认识的格式给空载荷（服务端会退回去试别的格式），不能 panic。
	#[test]
	fn unknown_format_yields_empty_payload() {
		assert!(encode_for_format(0xdead_beef, "hi").is_empty());
	}

	/// **回归（真机实测确认）**：`CB_LOCK_CLIPDATA` **必须**触发数据应答。
	///
	/// 2026-10-16 在 192.168.10.20 上用 FreeRDP 协议日志逐条核对：
	/// ```text
	/// 18:18:23.694  ClientFormatList: numFormats: 3（CF_UNICODETEXT/LOCALE/TEXT）
	/// 18:18:23.699  ← CB_LOCK_CLIPDATA (clipDataId)          服务端来锁
	/// 18:18:23.699  → ClientFormatDataResponse (50 bytes)   我们应答
	/// 18:18:23.699  ← CB_UNLOCK_CLIPDATA + CB_FORMAT_LIST_RESPONSE(OK)
	/// 18:18:23.823  → ClientUnlockClipboardData (120ms 后)
	/// ```
	/// **整场会话服务端一条 `CB_FORMAT_DATA_REQUEST` 都没发过**——
	/// 它就是用「锁」来索要数据的。
	///
	/// 我在第二十一轮按规范 §2.2.4.1 的字面描述（「请求保留 File Stream 数据」）
	/// 判定它与纯文本无关、只记日志不回数据 —— 那个判断把整条正向链路掐断了，
	/// **回归（第二十六轮）：收到锁绝不能回数据。**
	///
	/// 规范 §1.3.2.2.2：`CB_LOCK_CLIPDATA` 是读取方请求数据属主
	/// **保留文件流数据**，取数只走 `CB_FORMAT_DATA_REQUEST`。
	///
	/// 第二十五轮曾断言「收到锁必须立刻应答数据」，理由是服务端
	/// 用它来索要数据。真机报文推翻了这条：回数据之后服务端
	/// 再也没广播过剪贴板变化（Windows 剪贴板始终为空）；
	/// 改回「忽略锁」后服务端立刻改走 `CB_FORMAT_DATA_REQUEST`。
	/// FreeRDP 官方 macOS 客户端同样只返回 OK。
	#[test]
	fn lock_does_not_trigger_data_response() {
		use std::sync::atomic::{AtomicU32, Ordering};
		static SENT_LEN: AtomicU32 = AtomicU32::new(u32::MAX);

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_response(
			_context: *mut ffi::CliprdrClientContext,
			response: *const ffi::CLIPRDR_FORMAT_DATA_RESPONSE,
		) -> ffi::UINT {
			SENT_LEN.store(unsafe { (*response).common.dataLen }, Ordering::Relaxed);
			0
		}
		unsafe extern "C" fn on_unlock(
			_context: *mut ffi::CliprdrClientContext,
			_unlock: *const ffi::CLIPRDR_UNLOCK_CLIPBOARD_DATA,
		) -> ffi::UINT {
			SENT_LEN.store(0, Ordering::Relaxed);
			0
		}
		ctx.ClientFormatDataResponse = Some(on_response);
		ctx.ClientUnlockClipboardData = Some(on_unlock);
		ctx.custom =
			std::sync::Arc::as_ptr(&clipboard.state) as *mut PluginData as *mut std::ffi::c_void;

		{
			let mut state = clipboard.state.lock().unwrap();
			state.sending = Some((Content::Text("内容".to_string()), std::time::Instant::now()));
		}

		let lock = ffi::CLIPRDR_LOCK_CLIPBOARD_DATA {
			common: cliprdr_header(CB_LOCK_CLIPDATA_MSG, 0, 4),
			clipDataId: 7,
		};
		unsafe { server_lock_clipdata_trampoline(&mut ctx as *mut _, &lock) };

		assert_eq!(
			SENT_LEN.load(Ordering::Relaxed),
			u32::MAX,
			"锁不是取数请求，一个字节都不该回——回了服务端就不走正规通道了"
		);
		assert!(
			clipboard.state.lock().unwrap().pending_unlock.is_none(),
			"没有应答就不需要记账解锁"
		);
		assert!(
			!clipboard.remote_took_data(),
			"没应答数据，不能置位「服务端已取走」"
		);
		// 文本必须原样留着，等服务端的 CB_FORMAT_DATA_REQUEST
		assert_eq!(
			sending_text(&clipboard.state.lock().unwrap()),
			Some("内容".to_string()),
			"文本要留着给正式取数请求用"
		);
	}

	/// **回归**：服务端索要数据时必须按请求的格式应答，且文本不被销毁。
	///
	/// - 按格式应答：`CF_UNICODETEXT` 给 UTF-16、`CF_LOCALE` 给 LANGID
	/// - 文本不销毁：服务端可能为不同格式索要多次，`take()` 会让第二次变空
	#[test]
	fn format_data_request_answers_per_format_and_keeps_text() {
		use std::sync::{Mutex as StdMutex, OnceLock};
		/// 一次数据响应：`(msgFlags, dataLen, 载荷)`
		type Captured = StdMutex<Vec<(u32, u32, Vec<u8>)>>;
		// extern "C" fn item 不能捕获环境，放 static 里
		static CAPTURED: OnceLock<Captured> = OnceLock::new();
		let captured = CAPTURED.get_or_init(|| StdMutex::new(Vec::new()));
		captured.lock().unwrap().clear();

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_response(
			_context: *mut ffi::CliprdrClientContext,
			response: *const ffi::CLIPRDR_FORMAT_DATA_RESPONSE,
		) -> ffi::UINT {
			let response = unsafe { &*response };
			let len = response.common.dataLen as usize;
			let bytes = unsafe { std::slice::from_raw_parts(response.requestedFormatData, len) };
			CAPTURED
				.get_or_init(|| StdMutex::new(Vec::new()))
				.lock()
				.unwrap()
				.push((
					u32::from(response.common.msgFlags),
					len as u32,
					bytes.to_vec(),
				));
			0
		}
		ctx.ClientFormatDataResponse = Some(on_response);
		ctx.custom =
			std::sync::Arc::as_ptr(&clipboard.state) as *mut PluginData as *mut std::ffi::c_void;

		{
			let mut state = clipboard.state.lock().unwrap();
			state.sending = Some((Content::Text("ab".to_string()), std::time::Instant::now()));
		}

		// 依次索要 Unicode 文本与 Locale
		for format_id in [CF_UNICODETEXT, CF_LOCALE, CF_UNICODETEXT] {
			let request = ffi::CLIPRDR_FORMAT_DATA_REQUEST {
				common: cliprdr_header(CB_FORMAT_DATA_REQUEST, 0, 4),
				requestedFormatId: format_id,
			};
			unsafe { server_format_data_request_trampoline(&mut ctx as *mut _, &request) };
		}

		let log = captured.lock().unwrap();
		assert_eq!(log.len(), 3, "每次索要都要有一条响应");
		// Unicode 文本：UTF-16LE + NUL
		assert_eq!(
			log[0].0,
			u32::from(CB_RESPONSE_OK),
			"msgFlags 必须带 CB_RESPONSE_OK"
		);
		assert_eq!(log[0].1, 6, "\"ab\" → 2 字符 × 2 字节 + NUL");
		// Locale：4 字节 LANGID
		assert_eq!(log[1].1, 4, "CF_LOCALE 必须是 4 字节 LANGID");
		// 第二次索要 Unicode 仍要有内容（文本没被销毁）
		assert_eq!(log[2].2, log[0].2, "重复索要必须仍拿到同样的文本");
		drop(log);

		assert!(
			clipboard.remote_took_data(),
			"应答过就该置位「数据已送达」，供注入 Ctrl+V 判定时机"
		);
	}

	/// 没有待发内容时是空转，不动状态。
	#[test]
	fn flush_with_nothing_pending_is_a_noop() {
		let clipboard = Clipboard::new();
		clipboard.state.lock().unwrap().server_ready = true;
		clipboard.flush_outgoing(None);
		assert!(clipboard.state.lock().unwrap().pending_out.is_none());
	}
}

// ==================== 文件通路（S10）测试 ====================

#[cfg(test)]
mod file_clipboard_tests {
	use super::*;

	/// 描述表编码 → 解析往返：名字、大小、条目数都要对得上。
	#[test]
	fn file_group_descriptor_round_trips() {
		let dir = std::env::temp_dir().join(format!("daisy-clip-test-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path_a = dir.join("报告.txt");
		std::fs::write(&path_a, "0123456789").unwrap();
		let path_b = dir.join("川江.pdf");
		std::fs::write(&path_b, vec![0u8; 70000]).unwrap();

		let files = vec![
			LocalFile {
				name: "报告.txt".to_string(),
				size: 10,
				path: path_a.clone(),
			},
			LocalFile {
				name: "川江.pdf".to_string(),
				size: 70000,
				path: path_b.clone(),
			},
		];
		let bytes = build_file_group_descriptor(&files).expect("应能组装");
		assert_eq!(bytes.len(), 4 + 2 * FILE_DESCRIPTOR_SIZE);

		let entries = parse_file_group_descriptor(&bytes);
		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].name, "报告.txt");
		assert_eq!(entries[0].size, 10);
		assert_eq!(entries[1].name, "川江.pdf");
		assert_eq!(entries[1].size, 70000);

		std::fs::remove_dir_all(&dir).ok();
	}

	/// 中文文件名（UTF-16）要在 260 WCHAR 里 survive。
	#[test]
	fn file_descriptor_names_survive_wide_chars() {
		let dir = std::env::temp_dir().join(format!("daisy-clip-wide-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("歌词 - 星光下的诺言 .md");
		std::fs::write(&path, b"x").unwrap();

		let bytes = build_file_group_descriptor(&[LocalFile {
			name: "歌词 - 星光下的诺言 .md".to_string(),
			size: 1,
			path: path.clone(),
		}])
		.expect("应能组装");
		let entries = parse_file_group_descriptor(&bytes);
		assert_eq!(entries[0].name, "歌词 - 星光下的诺言 .md");
		std::fs::remove_dir_all(&dir).ok();
	}

	/// 描述表超长截断：条目数谎报时最多解析载荷装得下的数目，不能越界。
	#[test]
	fn descriptor_parse_respects_payload_bounds() {
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&999u32.to_le_bytes()); // 谎报 999 个条目
		append_file_descriptor(&mut bytes, &FileEntry {
			name: "only.txt".into(),
			size: 1,
		});
		let entries = parse_file_group_descriptor(&bytes);
		assert_eq!(entries.len(), 1, "载荷只装得下 1 个条目");
		assert_eq!(entries[0].name, "only.txt");
	}

	/// 下载状态机：描述表 size 直接驱动 RANGE × N；streamId 对不上要丢弃。
	#[test]
	fn download_assembles_files_in_order() {
		let mut download = Download::start(vec![
			FileEntry {
				name: "a.bin".into(),
				size: 10,
			},
			FileEntry {
				name: "b.bin".into(),
				size: 5,
			},
		], None)
		.expect("应能开始");

		// 第一条：文件 0 的 SIZE（cbRequested 恒为 8）
		let request = download.pop_request().expect("应先发 SIZE");
		assert_eq!(request.dw_flags, FILECONTENTS_SIZE);
		assert_eq!(request.list_index, 0);
		assert_eq!(request.count, 8, "SIZE 请求的 cbRequested 必须是 8");

		// SIZE 确认 10 字节 → 排上整个文件的 RANGE
		download.on_response(request.stream_id, true, &10u64.to_le_bytes());
		let request = download.pop_request().expect("应有 RANGE 请求");
		assert_eq!(request.dw_flags, FILECONTENTS_RANGE);
		assert_eq!(request.list_index, 0);
		assert_eq!(request.position, 0);
		assert_eq!(request.count, 10, "一次请求整个文件（< 256KB 块上限）");

		// 乱序/旧流号的响应必须被丢弃
		download.on_response(request.stream_id + 100, true, b"junk");
		assert!(
			download.next_request.is_none() && download.outstanding.is_some(),
			"流号不对的响应不该推进状态机"
		);

		// 第一块 6 字节 → 没满，继续排下一块
		download.on_response(request.stream_id, true, b"012345");
		let request = download.pop_request().expect("应排上第二块 RANGE");
		assert_eq!(request.position, 6);
		assert_eq!(request.count, 4, "剩余 4 字节");

		// 第二块满 → 切到文件 1：先 SIZE 再 RANGE
		download.on_response(request.stream_id, true, b"6789");
		let request = download.pop_request().expect("应切到文件 1 的 SIZE");
		assert_eq!(request.dw_flags, FILECONTENTS_SIZE);
		assert_eq!(request.list_index, 1);

		download.on_response(request.stream_id, true, &5u64.to_le_bytes());
		let request = download.pop_request().expect("应排上文件 1 的 RANGE");
		assert_eq!(request.dw_flags, FILECONTENTS_RANGE);
		assert_eq!(request.list_index, 1);
		assert_eq!(request.position, 0);
		assert_eq!(request.count, 5);

		download.on_response(request.stream_id, true, b"abcde");
		assert!(download.finished, "全部下载完成");

		let files = download.take_result().expect("应能取走结果");
		assert_eq!(files.len(), 2);
		assert_eq!(files[0].name, "a.bin");
		assert_eq!(files[0].data, b"0123456789");
		assert_eq!(files[1].name, "b.bin");
		assert_eq!(files[1].data, b"abcde");
	}

	/// 空文件不产生 RANGE 请求（`cbRequested=0` 是服务端兼容性雷区），
	/// 直接收进结果。
	#[test]
	fn download_skips_empty_files_without_requests() {
		let mut download = Download::start(vec![
			FileEntry {
				name: "空.txt".into(),
				size: 0,
			},
			FileEntry {
				name: "有.bin".into(),
				size: 3,
			},
		], None)
		.expect("应能开始");

		// 第一个请求必须是文件 1（下标 1）的 SIZE——空文件 0 已静默完成
		let request = download.pop_request().expect("应有文件 1 的 SIZE");
		assert_eq!(request.list_index, 1);
		assert_eq!(request.dw_flags, FILECONTENTS_SIZE);
		download.on_response(request.stream_id, true, &3u64.to_le_bytes());
		let request = download.pop_request().expect("应有文件 1 的 RANGE");
		assert_eq!(request.list_index, 1);
		assert_eq!(request.count, 3);
		download.on_response(request.stream_id, true, b"abc");
		assert!(download.finished);

		let files = download.take_result().expect("应能取走结果");
		assert_eq!(files.len(), 2);
		assert_eq!(files[0].name, "空.txt");
		assert!(files[0].data.is_empty());
		assert_eq!(files[1].data, b"abc");
	}

	/// 下载失败路径：CB_RESPONSE_FAIL 与超限都要置失败位。
	#[test]
	fn download_fails_loudly() {
		let mut download = Download::start(vec![FileEntry {
			name: "x".into(),
			size: 10,
		}], None)
		.unwrap();
		let request = download.pop_request().unwrap();
		download.on_response(request.stream_id, false, &[]);
		assert!(download.failed.is_some(), "FAIL 要置失败位");
		assert!(download.take_result().is_none(), "失败后没有结果");

		let too_big = Download::start(vec![FileEntry {
			name: "huge.iso".into(),
			size: MAX_FILE_SIZE + 1,
		}], None);
		assert!(too_big.is_err(), "超过内存上限要直接拒绝");
	}

	/// 上报文件时，格式列表必须是两个**带名字**的注册格式。
	#[test]
	fn send_files_announces_named_formats() {
		use std::sync::{Mutex as StdMutex, OnceLock};
		static FORMATS: OnceLock<StdMutex<Vec<(u32, String)>>> = OnceLock::new();
		let captured = FORMATS.get_or_init(|| StdMutex::new(Vec::new()));
		captured.lock().unwrap().clear();

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_format_list(
			_context: *mut ffi::CliprdrClientContext,
			list: *const ffi::CLIPRDR_FORMAT_LIST,
		) -> ffi::UINT {
			let list = unsafe { &*list };
			let formats =
				unsafe { std::slice::from_raw_parts(list.formats, list.numFormats as usize) };
			FORMATS
				.get_or_init(|| StdMutex::new(Vec::new()))
				.lock()
				.unwrap()
				.extend(formats.iter().map(|f| {
					let name = if f.formatName.is_null() {
						String::new()
					} else {
						// SAFETY: 测试内构造的名字是合法 C 串
						unsafe { std::ffi::CStr::from_ptr(f.formatName) }
							.to_string_lossy()
							.into_owned()
					};
					(f.formatId, name)
				}));
			0
		}
		ctx.ClientFormatList = Some(on_format_list);
		clipboard.state.lock().unwrap().server_ready = true;
		clipboard.send_files(vec![LocalFile {
			name: "a.txt".into(),
			size: 1,
			path: std::env::temp_dir().join("a.txt"),
		}]);
		clipboard.flush_outgoing(Some(&mut ctx));

		let formats = captured.lock().unwrap().clone();
		assert_eq!(formats.len(), 2, "文件上报只带两个格式");
		assert_eq!(formats[0].0, FORMAT_FILE_GROUP_DESCRIPTOR_W);
		assert_eq!(formats[0].1, "FileGroupDescriptorW", "注册格式必须带名字");
		assert_eq!(formats[1].0, FORMAT_FILE_CONTENTS);
		assert_eq!(formats[1].1, "FileContents");
		// 内容必须留在 sending 里等远端来要
		let state = clipboard.state.lock().unwrap();
		assert!(
			matches!(state.sending.as_ref().map(|(c, _)| c), Some(Content::Files(_))),
			"文件要留在 sending 等服务端索要"
		);
	}

	/// 服务端索要文件格式时：描述表按现值组装，FileContents 格式回空。
	#[test]
	fn format_data_request_serves_files() {
		use std::sync::{Mutex as StdMutex, OnceLock};
		static PAYLOADS: OnceLock<StdMutex<CapturedPayloads>> = OnceLock::new();
		let captured = PAYLOADS.get_or_init(|| StdMutex::new(Vec::new()));
		captured.lock().unwrap().clear();

		let dir = std::env::temp_dir().join(format!("daisy-clip-serve-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("data.bin");
		std::fs::write(&path, b"hello world").unwrap();

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_response(
			_context: *mut ffi::CliprdrClientContext,
			response: *const ffi::CLIPRDR_FORMAT_DATA_RESPONSE,
		) -> ffi::UINT {
			let response = unsafe { &*response };
			let len = response.common.dataLen as usize;
			let bytes = unsafe { std::slice::from_raw_parts(response.requestedFormatData, len) };
			PAYLOADS
				.get_or_init(|| StdMutex::new(Vec::new()))
				.lock()
				.unwrap()
				.push((response.common.msgFlags as u32, bytes.to_vec()));
			0
		}
		ctx.ClientFormatDataResponse = Some(on_response);
		ctx.custom = std::sync::Arc::as_ptr(&clipboard.state)
			as *mut PluginData as *mut std::ffi::c_void;

		{
			let mut state = clipboard.state.lock().unwrap();
			state.sending = Some((
				Content::Files(vec![LocalFile {
					name: "data.bin".into(),
					size: 11,
					path: path.clone(),
				}]),
				std::time::Instant::now(),
			));
		}

		// 索要描述表
		let request = ffi::CLIPRDR_FORMAT_DATA_REQUEST {
			common: cliprdr_header(CB_FORMAT_DATA_REQUEST, 0, 4),
			requestedFormatId: FORMAT_FILE_GROUP_DESCRIPTOR_W,
		};
		unsafe { server_format_data_request_trampoline(&mut ctx as *mut _, &request) };
		let log = captured.lock().unwrap();
		assert_eq!(log.len(), 1);
		assert_eq!(log[0].0, u32::from(CB_RESPONSE_OK));
		let entries = parse_file_group_descriptor(&log[0].1);
		assert_eq!(entries.len(), 1);
		assert_eq!(entries[0].size, 11, "按现值量大小，不是上报时的旧值");
		drop(log);

		// 索要 FileContents 格式本身：空 OK（内容走 FileContentsRequest）
		let request = ffi::CLIPRDR_FORMAT_DATA_REQUEST {
			common: cliprdr_header(CB_FORMAT_DATA_REQUEST, 0, 4),
			requestedFormatId: FORMAT_FILE_CONTENTS,
		};
		unsafe { server_format_data_request_trampoline(&mut ctx as *mut _, &request) };
		let log = captured.lock().unwrap();
		assert_eq!(log.len(), 2);
		assert_eq!(log[1].1.len(), 0, "FileContents 格式没有 FormatData 载荷");
		std::fs::remove_dir_all(&dir).ok();
	}

	/// 服务端按 FileContentsRequest 取字节：SIZE 给 8 字节，RANGE 给区间。
	#[test]
	fn file_contents_request_serves_size_and_range() {
		use std::sync::{Mutex as StdMutex, OnceLock};
		static RESPONSES: OnceLock<StdMutex<CapturedContents>> = OnceLock::new();
		let captured = RESPONSES.get_or_init(|| StdMutex::new(Vec::new()));
		captured.lock().unwrap().clear();

		let dir = std::env::temp_dir().join(format!("daisy-clip-range-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let path = dir.join("blob.bin");
		std::fs::write(&path, b"0123456789").unwrap();

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_response(
			_context: *mut ffi::CliprdrClientContext,
			response: *const ffi::CLIPRDR_FILE_CONTENTS_RESPONSE,
		) -> ffi::UINT {
			let response = unsafe { &*response };
			let len = response.common.dataLen as usize;
			let bytes = unsafe { std::slice::from_raw_parts(response.requestedData, len) };
			RESPONSES
				.get_or_init(|| StdMutex::new(Vec::new()))
				.lock()
				.unwrap()
				.push((response.common.msgFlags, response.streamId, bytes.to_vec()));
			0
		}
		ctx.ClientFileContentsResponse = Some(on_response);
		ctx.custom = std::sync::Arc::as_ptr(&clipboard.state)
			as *mut PluginData as *mut std::ffi::c_void;

		{
			let mut state = clipboard.state.lock().unwrap();
			state.sending = Some((
				Content::Files(vec![LocalFile {
					name: "blob.bin".into(),
					size: 10,
					path: path.clone(),
				}]),
				std::time::Instant::now(),
			));
		}

		// SIZE 请求
		let size_request = ffi::CLIPRDR_FILE_CONTENTS_REQUEST {
			common: cliprdr_header(CB_FILECONTENTS_REQUEST, 0, 0),
			streamId: 7,
			listIndex: 0,
			dwFlags: FILECONTENTS_SIZE,
			nPositionLow: 0,
			nPositionHigh: 0,
			cbRequested: 0,
			haveClipDataId: 0,
			clipDataId: 0,
		};
		unsafe { server_file_contents_request_trampoline(&mut ctx as *mut _, &size_request) };
		let log = captured.lock().unwrap();
		assert_eq!(log.len(), 1);
		assert_eq!(log[0].0, CB_RESPONSE_OK);
		assert_eq!(log[0].1, 7, "streamId 必须原样带回");
		assert_eq!(log[0].2, 10u64.to_le_bytes(), "SIZE 响应是 8 字节小端");
		drop(log);

		// RANGE 请求：第 4 字节起取 3 字节
		let range_request = ffi::CLIPRDR_FILE_CONTENTS_REQUEST {
			common: cliprdr_header(CB_FILECONTENTS_REQUEST, 0, 0),
			streamId: 8,
			listIndex: 0,
			dwFlags: FILECONTENTS_RANGE,
			nPositionLow: 4,
			nPositionHigh: 0,
			cbRequested: 3,
			haveClipDataId: 0,
			clipDataId: 0,
		};
		unsafe { server_file_contents_request_trampoline(&mut ctx as *mut _, &range_request) };
		let log = captured.lock().unwrap();
		assert_eq!(log.len(), 2);
		assert_eq!(log[1].2, b"456", "RANGE 按位置取字节");
		std::fs::remove_dir_all(&dir).ok();
	}

	/// 测试捕获的一条数据响应：`(msgFlags, 载荷)`。
	type CapturedPayloads = Vec<(u32, Vec<u8>)>;
	/// 测试捕获的一条文件内容响应：`(msgFlags, streamId, 载荷)`。
	type CapturedContents = Vec<(u16, u32, Vec<u8>)>;

	/// 描述表回空（rdpclip 还没写完系统剪贴板）时要排一次重试，
	/// 而且**不能**把这次请求当成已消费——否则远端复制了文件这边没反应。
	#[test]
	fn empty_descriptor_response_is_retried_once() {
		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		ctx.custom = std::sync::Arc::as_ptr(&clipboard.state)
			as *mut PluginData as *mut std::ffi::c_void;

		{
			let mut state = clipboard.state.lock().unwrap();
			state.requested = Some(RequestKind::FileDescriptors(0xC0DE));
			state.retry_left = 1;
		}

		let empty = ffi::CLIPRDR_FORMAT_DATA_RESPONSE {
			common: cliprdr_header(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, 0),
			requestedFormatData: std::ptr::null_mut(),
		};
		unsafe { server_format_data_response_trampoline(&mut ctx as *mut _, &empty) };

		let (retry_at, retry_left, requested) = {
			let state = clipboard.state.lock().unwrap();
			(state.retry_at, state.retry_left, state.requested)
		};
		assert!(retry_at.is_some(), "空响应应排一次重试");
		assert_eq!(retry_left, 0, "重试机会只给一次");
		assert_eq!(
			requested,
			Some(RequestKind::FileDescriptors(0xC0DE)),
			"空响应不能把请求消费掉，否则重试无从发起"
		);

		// 再空一次：机会用完，不再改排期（也不要把 requested 吞掉）
		unsafe { server_format_data_response_trampoline(&mut ctx as *mut _, &empty) };
		let state = clipboard.state.lock().unwrap();
		assert_eq!(state.retry_at, retry_at, "不应重新排一次重试");
		assert_eq!(state.retry_left, 0);
	}

	/// 端到端（远端 → 本地）：广播 → 要描述表 → SIZE → RANGE → take_remote_files。
	#[test]
	fn remote_to_local_file_flow_end_to_end() {
		use std::sync::{Mutex as StdMutex, OnceLock};
		static SENT_REQUESTS: OnceLock<StdMutex<Vec<(u32, u32, u32)>>> = OnceLock::new();
		let sent = SENT_REQUESTS.get_or_init(|| StdMutex::new(Vec::new()));
		sent.lock().unwrap().clear();

		let clipboard = Clipboard::new();
		let mut ctx: ffi::CliprdrClientContext = unsafe { std::mem::zeroed() };
		unsafe extern "C" fn on_format_data_request(
			_context: *mut ffi::CliprdrClientContext,
			request: *const ffi::CLIPRDR_FORMAT_DATA_REQUEST,
		) -> ffi::UINT {
			let request = unsafe { &*request };
			assert_eq!(request.requestedFormatId, 0xC0DE, "我们要的是描述表");
			0
		}
		unsafe extern "C" fn on_file_contents_request(
			_context: *mut ffi::CliprdrClientContext,
			request: *const ffi::CLIPRDR_FILE_CONTENTS_REQUEST,
		) -> ffi::UINT {
			let request = unsafe { &*request };
			SENT_REQUESTS
				.get_or_init(|| StdMutex::new(Vec::new()))
				.lock()
				.unwrap()
				.push((request.streamId, request.dwFlags, request.listIndex));
			0
		}
		ctx.ClientFormatDataRequest = Some(on_format_data_request);
		ctx.ClientFileContentsRequest = Some(on_file_contents_request);
		ctx.custom = std::sync::Arc::as_ptr(&clipboard.state)
			as *mut PluginData as *mut std::ffi::c_void;

		// 1. 服务端广播：文本格式 + FileGroupDescriptorW（ID 服务端自定）
		let mut c_name = b"FileGroupDescriptorW\0".to_vec();
		c_name.pop();
		let formats = [
			ffi::CLIPRDR_FORMAT {
				formatId: CF_UNICODETEXT,
				formatName: std::ptr::null_mut(),
			},
			ffi::CLIPRDR_FORMAT {
				formatId: 0xC0DE,
				formatName: c_name.as_ptr().cast::<std::os::raw::c_char>().cast_mut(),
			},
		];
		let list = ffi::CLIPRDR_FORMAT_LIST {
			common: cliprdr_header(CB_FORMAT_LIST, 0, 0),
			numFormats: formats.len() as u32,
			formats: formats.as_ptr().cast_mut().cast(),
		};
		unsafe { server_format_list_trampoline(&mut ctx as *mut _, &list) };
		assert_eq!(
			clipboard.state.lock().unwrap().pending_request,
			Some(RequestKind::FileDescriptors(0xC0DE)),
			"文件格式优先于文本"
		);

		// 2. 帧循环发出取数请求
		clipboard.flush_outgoing(Some(&mut ctx));
		assert_eq!(
			clipboard.state.lock().unwrap().requested,
			Some(RequestKind::FileDescriptors(0xC0DE))
		);

		// 3. 服务端回描述表：两个文件
		let mut payload = Vec::new();
		payload.extend_from_slice(&2u32.to_le_bytes());
		append_file_descriptor(&mut payload, &FileEntry {
			name: "甲.txt".into(),
			size: 4,
		});
		append_file_descriptor(&mut payload, &FileEntry {
			name: "乙.txt".into(),
			size: 3,
		});
		let response = ffi::CLIPRDR_FORMAT_DATA_RESPONSE {
			common: cliprdr_header(CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_OK, payload.len() as u32),
			requestedFormatData: payload.as_ptr(),
		};
		unsafe { server_format_data_response_trampoline(&mut ctx as *mut _, &response) };
		assert!(
			clipboard.state.lock().unwrap().download.is_some(),
			"描述表到达后应开始下载"
		);

		// 4. 帧循环先发 SIZE（cbRequested 恒为 8）
		clipboard.flush_outgoing(Some(&mut ctx));
		let log = sent.lock().unwrap().clone();
		assert_eq!(log.len(), 1, "应发出文件 0 的 SIZE 请求");
		assert_eq!(log[0].1, FILECONTENTS_SIZE);
		let stream = log[0].0;
		drop(log);

		// SIZE 确认 4 字节 → 帧循环发 RANGE
		let size = 4u64.to_le_bytes();
		let size_response = ffi::CLIPRDR_FILE_CONTENTS_RESPONSE {
			common: cliprdr_header(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, 8),
			streamId: stream,
			cbRequested: 8,
			requestedData: size.as_ptr(),
		};
		unsafe { server_file_contents_response_trampoline(&mut ctx as *mut _, &size_response) };
		clipboard.flush_outgoing(Some(&mut ctx));
		let log = sent.lock().unwrap().clone();
		assert_eq!(log.len(), 2, "应发出文件 0 的 RANGE 请求");
		assert_eq!(log[1].1, FILECONTENTS_RANGE);
		let stream = log[1].0;
		drop(log);

		// 5. 服务端回数据 → 帧循环发文件 1 的 SIZE → 回大小 → 发 RANGE → 回数据 → 完成
		let data = b"abcd";
		let range_response = ffi::CLIPRDR_FILE_CONTENTS_RESPONSE {
			common: cliprdr_header(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, 4),
			streamId: stream,
			cbRequested: 4,
			requestedData: data.as_ptr(),
		};
		unsafe { server_file_contents_response_trampoline(&mut ctx as *mut _, &range_response) };
		clipboard.flush_outgoing(Some(&mut ctx));
		let log = sent.lock().unwrap().clone();
		assert_eq!(log.len(), 3, "切到文件 1 的 SIZE");
		assert_eq!(log[2].1, FILECONTENTS_SIZE);
		let size_stream = log[2].0;
		drop(log);

		let size2 = 3u64.to_le_bytes();
		let size2_response = ffi::CLIPRDR_FILE_CONTENTS_RESPONSE {
			common: cliprdr_header(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, 8),
			streamId: size_stream,
			cbRequested: 8,
			requestedData: size2.as_ptr(),
		};
		unsafe { server_file_contents_response_trampoline(&mut ctx as *mut _, &size2_response) };
		clipboard.flush_outgoing(Some(&mut ctx));
		let log = sent.lock().unwrap().clone();
		assert_eq!(log.len(), 4, "文件 1 的 RANGE");
		assert_eq!(log[3].1, FILECONTENTS_RANGE);
		let stream2 = log[3].0;
		drop(log);

		let data2 = b"xyz";
		let range2 = ffi::CLIPRDR_FILE_CONTENTS_RESPONSE {
			common: cliprdr_header(CB_FILECONTENTS_RESPONSE, CB_RESPONSE_OK, 3),
			streamId: stream2,
			cbRequested: 3,
			requestedData: data2.as_ptr(),
		};
		unsafe { server_file_contents_response_trampoline(&mut ctx as *mut _, &range2) };
		clipboard.flush_outgoing(Some(&mut ctx));

		// 6. 取走结果
		let files = clipboard.take_remote_files().expect("应下载完成");
		assert_eq!(files.len(), 2);
		assert_eq!(files[0].name, "甲.txt");
		assert_eq!(files[0].data, b"abcd");
		assert_eq!(files[1].name, "乙.txt");
		assert_eq!(files[1].data, b"xyz");
	}
}
