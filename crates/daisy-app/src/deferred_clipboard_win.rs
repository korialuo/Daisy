//! Windows 延迟渲染剪贴板（mstsc 同款）。
//!
//! 普通 `CF_HDROP` 必须指向已存在的本地文件，所以旧实现只能在远端 Ctrl+C
//! 时就把整个文件下载完。mstsc 放一个 OLE `IDataObject` 到剪贴板，声明
//! `CFSTR_FILEDESCRIPTORW`（文件名/大小）+ `CFSTR_FILECONTENTS`（内容走
//! `IStream`）；目标程序粘贴时才调 `IStream::Read`，那一刻才拉字节。
//!
//! - [`ChunkSource`] 抽象按需读取（生产是 `RdpSession`，单测可假实现）。
//! - [`install_deferred`] 建 `IDataObject` 并 `OleSetClipboard` 接管剪贴板。
//! - `IStream::Read` 调 `ChunkSource::read` 同步等 RDP 会话线程送字节。
//!
//! 线程：OLE 剪贴板要求 STA，`install_deferred` 必须在 UI 主线程调用。

use std::sync::{Arc, Mutex};

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{
	DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_FAIL, E_INVALIDARG, E_NOTIMPL,
	E_POINTER, HGLOBAL, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
	DATADIR_GET, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
	IEnumFORMATETC_Impl, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE,
	STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END,
	STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{OleInitialize, OleIsCurrentClipboard, OleSetClipboard};
use windows::Win32::UI::Shell::FILEDESCRIPTORW;
use windows::Win32::UI::WindowsAndMessaging::{
	DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};
use windows::core::{ComObject, ComObjectInner, HRESULT, Ref, implement, w};

use daisy_rdp::RemoteFileMeta;

/// shell 注册的两种格式 ID（每次启动可能不同，运行时查）。
#[derive(Clone, Copy, Debug)]
pub struct FormatIds {
	pub file_descriptor: u16,
	pub file_contents: u16,
}

fn register_formats() -> FormatIds {
	// SAFETY: 宽字符串字面量 NUL 结尾有效；函数无副作用依赖。
	let descriptor = unsafe { RegisterClipboardFormatW(w!("FileGroupDescriptorW")) };
	let contents = unsafe { RegisterClipboardFormatW(w!("FileContents")) };
	FormatIds {
		file_descriptor: descriptor as u16,
		file_contents: contents as u16,
	}
}

/// 按需读取字节的来源。`read` 同步阻塞直到拿到数据或失败。
pub trait ChunkSource: Send + Sync + 'static {
	fn read(&self, index: u32, offset: u64, len: u32) -> Result<Vec<u8>, String>;
}

#[derive(Clone, Debug)]
struct DeferredFile {
	meta: RemoteFileMeta,
}

/// 把远端文件的一段区间呈现为可读 IStream；`Read` 时才真正拉字节。
#[implement(IStream)]
struct DeferredStream {
	source: Arc<dyn ChunkSource>,
	index: u32,
	size: u64,
	pos: Mutex<u64>,
}

impl DeferredStream {
	fn new(source: Arc<dyn ChunkSource>, index: u32, size: u64) -> Self {
		Self {
			source,
			index,
			size,
			pos: Mutex::new(0),
		}
	}
}

impl ISequentialStream_Impl for DeferredStream_Impl {
	fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
		if pv.is_null() || cb == 0 {
			if !pcbread.is_null() {
				unsafe { *pcbread = 0 };
			}
			return S_OK;
		}
		let mut pos = self.pos.lock().unwrap();
		if *pos >= self.size {
			if !pcbread.is_null() {
				unsafe { *pcbread = 0 };
			}
			return S_OK; // EOF：0 字节 + S_OK
		}
		let want = (cb as u64).min(self.size - *pos) as u32;
		let offset = *pos;
		match self.source.read(self.index, offset, want) {
			Ok(bytes) => {
				// **绝不能写超过 cb**：目标程序（资源管理器）的缓冲只有 cb 字节，
				// 多写就是踩坏它的堆；`*pcbread > cb` 在 shell 里直接判 E_BOUNDS
				// （「数组绑定无效」）。只信请求长度，不信来源给的长度。
				let n = (bytes.len() as u64).min(want as u64) as u32;
				// SAFETY: 目标缓冲 >= cb 字节；n <= want <= cb。
				unsafe {
					std::ptr::copy_nonoverlapping(bytes.as_ptr(), pv.cast::<u8>(), n as usize)
				};
				*pos += n as u64;
				if !pcbread.is_null() {
					unsafe { *pcbread = n };
				}
				S_OK
			},
			Err(error) => {
				tracing::warn!("[剪贴板] 延迟渲染 Read 失败 index={}：{error}", self.index);
				if !pcbread.is_null() {
					unsafe { *pcbread = 0 };
				}
				E_FAIL
			},
		}
	}

	fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, pcbwritten: *mut u32) -> HRESULT {
		if !pcbwritten.is_null() {
			unsafe { *pcbwritten = 0 };
		}
		E_NOTIMPL
	}
}

impl IStream_Impl for DeferredStream_Impl {
	fn Seek(
		&self,
		dlibmove: i64,
		dworigin: STREAM_SEEK,
		plibnewposition: *mut u64,
	) -> windows::core::Result<()> {
		let mut pos = self.pos.lock().unwrap();
		let base = if dworigin == STREAM_SEEK_SET {
			0i64
		} else if dworigin == STREAM_SEEK_CUR {
			*pos as i64
		} else if dworigin == STREAM_SEEK_END {
			self.size as i64
		} else {
			return Err(E_INVALIDARG.into());
		};
		let new_pos = base.saturating_add(dlibmove).max(0) as u64;
		*pos = new_pos.min(self.size);
		if !plibnewposition.is_null() {
			unsafe { *plibnewposition = *pos };
		}
		Ok(())
	}

	fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
		Err(E_NOTIMPL.into())
	}

	fn CopyTo(
		&self,
		_pstm: Ref<IStream>,
		_cb: u64,
		_pcbread: *mut u64,
		_pcbwritten: *mut u64,
	) -> windows::core::Result<()> {
		Err(E_NOTIMPL.into())
	}

	fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
		Ok(())
	}

	fn Revert(&self) -> windows::core::Result<()> {
		Ok(())
	}

	fn LockRegion(
		&self,
		_liboffset: u64,
		_cb: u64,
		_dwlocktype: &LOCKTYPE,
	) -> windows::core::Result<()> {
		Err(E_NOTIMPL.into())
	}

	fn UnlockRegion(
		&self,
		_liboffset: u64,
		_cb: u64,
		_dwlocktype: u32,
	) -> windows::core::Result<()> {
		Err(E_NOTIMPL.into())
	}

	fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
		if pstatstg.is_null() {
			return Err(E_POINTER.into());
		}
		// SAFETY: pstatstg 由调用方提供、可写。
		unsafe {
			// 整个结构先清零：pwcsName / type / 时间戳/ clsid 留 0 才是「合法空值」，
			// 让调用方拿到的不是上一轮残留的垃圾。type 必须是 STGTY_STREAM（2），
			// 否则资源管理器按 storage 解释这个流，取内容时就越界。
			std::ptr::write_bytes(pstatstg.cast::<u8>(), 0, std::mem::size_of::<STATSTG>());
			(*pstatstg).cbSize = self.size;
			// grfMode 保持清零即为 STGM_READ；只补 type。
			pstatstg
				.cast::<u8>()
				.add(std::mem::offset_of!(STATSTG, r#type))
				.cast::<u32>()
				.write_unaligned(2); // STGTY_STREAM
		}
		Ok(())
	}

	fn Clone(&self) -> windows::core::Result<IStream> {
		Err(E_NOTIMPL.into())
	}
}

/// 枚举我们声明的两种格式。
#[implement(IEnumFORMATETC)]
struct FormatEnumerator {
	formats: Vec<FORMATETC>,
	pos: Mutex<usize>,
}

impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
	fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
		if rgelt.is_null() {
			return E_POINTER;
		}
		let mut pos = self.pos.lock().unwrap();
		let mut fetched = 0u32;
		while fetched < celt && *pos < self.formats.len() {
			// SAFETY: 调用方保证 rgelt 有 celt 个 FORMATETC 的空间。
			unsafe { *rgelt.add(fetched as usize) = self.formats[*pos] };
			*pos += 1;
			fetched += 1;
		}
		if !pceltfetched.is_null() {
			unsafe { *pceltfetched = fetched };
		}
		if fetched < celt { S_FALSE } else { S_OK }
	}

	fn Skip(&self, celt: u32) -> windows::core::Result<()> {
		let mut pos = self.pos.lock().unwrap();
		*pos = (*pos + celt as usize).min(self.formats.len());
		Ok(())
	}

	fn Reset(&self) -> windows::core::Result<()> {
		*self.pos.lock().unwrap() = 0;
		Ok(())
	}

	fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
		let clone = FormatEnumerator {
			formats: self.formats.clone(),
			pos: Mutex::new(*self.pos.lock().unwrap()),
		};
		Ok(clone.into_object().into_interface())
	}
}

/// 放到 OLE 剪贴板上的数据对象。
#[implement(IDataObject)]
struct DeferredDataObject {
	files: Vec<DeferredFile>,
	source: Arc<dyn ChunkSource>,
	formats: FormatIds,
}

impl DeferredDataObject {
	fn format_etc(&self, cf: u16, tymed: u32, lindex: i32) -> FORMATETC {
		FORMATETC {
			cfFormat: cf,
			ptd: std::ptr::null_mut(),
			dwAspect: 1, // DVASPECT_CONTENT
			lindex,
			tymed,
		}
	}

	/// 构造 FILEGROUPDESCRIPTORW：cItems + N 个 FILEDESCRIPTORW。
	///
	/// `only = Some(i)` 时只输出第 i 条**单个** `FILEDESCRIPTORW`（不带 cItems）：
	/// 有些取数方（含资源管理器的部分路径）会用 `lindex >= 0` 单独索引描述符，
	/// 此时塞给它整个组会被当成单条解析——长度对不上就是 E_BOUNDS。
	fn build_descriptor(&self, only: Option<usize>) -> Option<Vec<u8>> {
		let selected: Vec<&DeferredFile> = match only {
			Some(index) => vec![self.files.get(index)?],
			None => self.files.iter().collect(),
		};
		let mut bytes = Vec::new();
		if only.is_none() {
			bytes.extend_from_slice(&(selected.len() as u32).to_le_bytes());
		}
		for file in selected {
			// FILEDESCRIPTORW 是 packed(1)，字段可能未对齐——逐字节按偏移写，
			// 不直接访问字段（Rust 对 packed 字段取引用会编译失败）。
			let mut d = FILEDESCRIPTORW::default();
			let base = &mut d as *mut FILEDESCRIPTORW as *mut u8;
			// SAFETY: base 指向 d 开头，用标准库的 offset_of! 拿字段偏移，
			// 逐字段非对齐写（packed 结构不能取字段引用）。
			unsafe {
				base.add(core::mem::offset_of!(FILEDESCRIPTORW, dwFlags))
					.cast::<u32>()
					.write_unaligned(0x0000_0004 | 0x0000_0040); // FD_ATTRIBUTES | FD_FILESIZE
				base.add(core::mem::offset_of!(FILEDESCRIPTORW, dwFileAttributes))
					.cast::<u32>()
					.write_unaligned(0x0000_0020); // FILE_ATTRIBUTE_ARCHIVE
				base.add(core::mem::offset_of!(FILEDESCRIPTORW, nFileSizeHigh))
					.cast::<u32>()
					.write_unaligned((file.meta.size >> 32) as u32);
				base.add(core::mem::offset_of!(FILEDESCRIPTORW, nFileSizeLow))
					.cast::<u32>()
					.write_unaligned(file.meta.size as u32);
			}
			let units: Vec<u16> = file.meta.name.encode_utf16().collect();
			let name_off = core::mem::offset_of!(FILEDESCRIPTORW, cFileName);
			// cFileName 是 [u16; 260]，留一个 NUL；不用 d.cFileName.len()（packed 不能取引用）。
			let copy = units.len().min(259);
			for (i, unit) in units[..copy].iter().enumerate() {
				// SAFETY: name_off + i*2 在 cFileName 范围内。
				unsafe {
					base.add(name_off + i * 2)
						.cast::<u16>()
						.write_unaligned(*unit)
				};
			}
			let raw = unsafe {
				std::slice::from_raw_parts(
					base as *const u8,
					std::mem::size_of::<FILEDESCRIPTORW>(),
				)
			};
			bytes.extend_from_slice(raw);
		}
		Some(bytes)
	}
}

impl IDataObject_Impl for DeferredDataObject_Impl {
	fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
		if pformatetcin.is_null() {
			return Err(E_POINTER.into());
		}
		// SAFETY: 调用方保证指针有效。
		let etc = unsafe { &*pformatetcin };
		// 这一行是排查「粘贴报错」的第一手证据：取数方到底按哪个格式、
		// 哪个序号、哪种介质类型来要数据（mstsc 也只提供同一组组合）。
		tracing::info!(
			"[剪贴板] GetData cf={:#06x} lindex={} tymed={:#06x}",
			etc.cfFormat,
			etc.lindex,
			etc.tymed
		);
		if etc.cfFormat == self.formats.file_descriptor {
			if etc.tymed & (TYMED_HGLOBAL.0 as u32) == 0 {
				tracing::warn!(
					"[剪贴板] 拒绝描述表：tymed={:#06x} 不含 HGLOBAL",
					etc.tymed
				);
				return Err(DV_E_TYMED.into());
			}
			let only = if etc.lindex >= 0 {
				Some(etc.lindex as usize)
			} else {
				None
			};
			let Some(bytes) = self.build_descriptor(only) else {
				return Err(DV_E_LINDEX.into());
			};
			// SAFETY: GMEM_MOVEABLE 是 SetClipboardData/STGMEDIUM 的硬性要求。
			let hglobal: HGLOBAL = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }?;
			let ptr = unsafe { GlobalLock(hglobal) };
			// SAFETY: 分配了 bytes.len()，ptr 可写。
			unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len()) };
			// GlobalUnlock 只解除锁，返回非 0 表示仍被锁（本处不会发生），忽略即可。
			let _ = unsafe { GlobalUnlock(hglobal) };
			Ok(STGMEDIUM {
				tymed: TYMED_HGLOBAL.0 as u32,
				u: STGMEDIUM_0 { hGlobal: hglobal },
				pUnkForRelease: ManuallyDrop::new(None),
			})
		} else if etc.cfFormat == self.formats.file_contents {
			// **只提供 IStream**：`QueryGetData` 里也必须照这个口径拒绝
			// `TYMED_HGLOBAL`。否则取数方以为能拿到内存块，`GetData` 却收到
			// 流指针，按 HGLOBAL 去解析——资源管理器在这里报的就是
			// 「数组绑定无效」（E_BOUNDS）。
			if etc.tymed & (TYMED_ISTREAM.0 as u32) == 0 {
				tracing::warn!(
					"[剪贴板] 拒绝文件内容：tymed={:#06x} 不含 ISTREAM",
					etc.tymed
				);
				return Err(DV_E_TYMED.into());
			}
			let index = if etc.lindex < 0 {
				0
			} else {
				etc.lindex as u32
			};
			let Some(file) = self.files.get(index as usize) else {
				return Err(DV_E_LINDEX.into());
			};
			let stream = DeferredStream::new(Arc::clone(&self.source), index, file.meta.size);
			let iface: IStream = stream.into_object().into_interface();
			// 所有权随 STGMEDIUM 交给 OLE / 目标程序（ManuallyDrop 防提前 Release）。
			Ok(STGMEDIUM {
				tymed: TYMED_ISTREAM.0 as u32,
				u: STGMEDIUM_0 {
					pstm: ManuallyDrop::new(Some(iface)),
				},
				pUnkForRelease: ManuallyDrop::new(None),
			})
		} else {
			Err(DV_E_FORMATETC.into())
		}
	}

	fn GetDataHere(
		&self,
		_pformatetc: *const FORMATETC,
		_pmedium: *mut STGMEDIUM,
	) -> windows::core::Result<()> {
		Err(E_NOTIMPL.into())
	}

	fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
		if pformatetc.is_null() {
			return E_POINTER;
		}
		let etc = unsafe { &*pformatetc };
		let index_ok = etc.lindex < 0 || (etc.lindex as usize) < self.files.len();
		if etc.cfFormat == self.formats.file_descriptor {
			if etc.tymed & (TYMED_HGLOBAL.0 as u32) == 0 {
				return DV_E_TYMED;
			}
			if index_ok {
				S_OK
			} else {
				DV_E_LINDEX
			}
		} else if etc.cfFormat == self.formats.file_contents {
			// 这里必须与 `GetData` 完全一致：声明只给 IStream，就不能对
			// `TYMED_HGLOBAL` 的询问回 S_OK。
			if etc.tymed & (TYMED_ISTREAM.0 as u32) == 0 {
				return DV_E_TYMED;
			}
			if index_ok {
				S_OK
			} else {
				DV_E_LINDEX
			}
		} else {
			S_FALSE
		}
	}

	fn GetCanonicalFormatEtc(
		&self,
		_pformatectin: *const FORMATETC,
		pformatetcout: *mut FORMATETC,
	) -> HRESULT {
		if !pformatetcout.is_null() {
			unsafe { *pformatetcout = std::mem::zeroed() };
		}
		DATA_S_SAMEFORMATETC
	}

	fn SetData(
		&self,
		_pformatetc: *const FORMATETC,
		_pmedium: *const STGMEDIUM,
		_frelease: windows::core::BOOL,
	) -> windows::core::Result<()> {
		Err(E_NOTIMPL.into())
	}

	fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
		if dwdirection != DATADIR_GET.0 as u32 {
			return Err(E_NOTIMPL.into());
		}
		// 一个文件一条 `FileContents`（lindex = 文件序号）。只给 lindex=0 时，
		// 只有第一个文件取得到内容，其余在组里「有描述符没有内容」，
		// shell 按描述符数量取内容就索引越界——同样是「数组绑定无效」。
		let mut formats = vec![self.format_etc(
			self.formats.file_descriptor,
			TYMED_HGLOBAL.0 as u32,
			-1,
		)];
		for index in 0..self.files.len() {
			formats.push(self.format_etc(
				self.formats.file_contents,
				TYMED_ISTREAM.0 as u32,
				index as i32,
			));
		}
		let enumerator = FormatEnumerator {
			formats,
			pos: Mutex::new(0),
		};
		Ok(enumerator.into_object().into_interface())
	}

	fn DAdvise(
		&self,
		_pformatetc: *const FORMATETC,
		_advf: u32,
		_padvsink: Ref<IAdviseSink>,
	) -> windows::core::Result<u32> {
		Err(E_NOTIMPL.into())
	}

	fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
		Err(E_NOTIMPL.into())
	}

	fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
		Err(E_NOTIMPL.into())
	}
}

thread_local! {
	/// 我们最近一次挂上去的对象。撤下前要用 `OleIsCurrentClipboard` 确认
	/// 剪贴板里仍是它——用户后来复制了别的东西时，无条件清空会把用户的
	/// 剪贴板抹掉。
	///
	/// **只在剪贴板 STA 线程里读写**（见 [`clipboard_thread_main`]），
	/// 所以是 thread_local 而不是全局锁。
	static CURRENT_OBJECT: std::cell::RefCell<Option<IDataObject>> =
		const { std::cell::RefCell::new(None) };
}

/// 投给剪贴板 STA 线程的指令。
enum StaCommand {
	Install {
		files: Vec<RemoteFileMeta>,
		source: Arc<dyn ChunkSource>,
		reply: std::sync::mpsc::Sender<Result<(), String>>,
	},
	Clear,
}

/// 取（必要时启动）剪贴板 STA 线程的发送端。
fn sta_sender() -> Option<&'static std::sync::mpsc::Sender<StaCommand>> {
	use std::sync::OnceLock;
	static SENDER: OnceLock<Option<std::sync::mpsc::Sender<StaCommand>>> = OnceLock::new();
	SENDER.get_or_init(spawn_clipboard_thread).as_ref()
}

fn spawn_clipboard_thread() -> Option<std::sync::mpsc::Sender<StaCommand>> {
	let (tx, rx) = std::sync::mpsc::channel::<StaCommand>();
	match std::thread::Builder::new()
		.name("daisy-clipboard-sta".to_string())
		.spawn(move || clipboard_thread_main(rx))
	{
		Ok(_) => Some(tx),
		Err(error) => {
			tracing::warn!("[剪贴板] 起剪贴板 STA 线程失败：{error}");
			None
		},
	}
}

/// 剪贴板 STA 线程主体。
///
/// **为什么必须另起线程**：`OleSetClipboard` 之后，取数方（资源管理器）
/// 的 `GetData` / `IStream::Read` 会通过 COM 回到**持有对象的那个 STA 线程**
/// 上执行。挂在 UI 线程上时，每读一块（256 KB，慢链路 200~500 ms）就把
/// 界面冻一次；40 MB 要读 160 块，等于整窗「未响应」半分钟——用户只会去
/// 关窗口/断开会话，于是后续读取全部拿到「会话已断开」。
/// mstsc 同样把数据对象放在独立 STA 线程，这里对齐。
fn clipboard_thread_main(rx: std::sync::mpsc::Receiver<StaCommand>) {
	// SAFETY: 本线程独占，第一次调用即把线程初始化成 STA。
	let _ole = unsafe { OleInitialize(None) };
	loop {
		while let Ok(command) = rx.try_recv() {
			match command {
				StaCommand::Install {
					files,
					source,
					reply,
				} => {
					let result = install_on_this_thread(files, source);
					let _ = reply.send(result);
				},
				StaCommand::Clear => {
					let still_ours = CURRENT_OBJECT.with(|slot| {
						let slot = slot.borrow();
						match slot.as_ref() {
							// SAFETY: 同一 STA 线程，OLE 已初始化。
							Some(object) => unsafe { OleIsCurrentClipboard(object).is_ok() },
							None => false,
						}
					});
					if still_ours {
						// SAFETY: 同上。
						unsafe {
							let _ = OleSetClipboard(None);
						}
						tracing::info!("[剪贴板] 已撤下延迟渲染剪贴板");
					}
					CURRENT_OBJECT.with(|slot| *slot.borrow_mut() = None);
				},
			}
		}
		// 泵消息：跨进程的 COM 调用要靠这里派发。5ms 轮询足够，且不至于空转。
		// SAFETY: 线程内调用，参数合法。
		unsafe {
			let mut message = MSG::default();
			while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
				let _ = TranslateMessage(&message);
				DispatchMessageW(&message);
			}
		}
		std::thread::sleep(std::time::Duration::from_millis(5));
	}
}

/// 在本（STA）线程内建对象并交给 OLE。
fn install_on_this_thread(
	files: Vec<RemoteFileMeta>,
	source: Arc<dyn ChunkSource>,
) -> Result<(), String> {
	let formats = register_formats();
	let data_object = DeferredDataObject {
		files: files
			.into_iter()
			.map(|meta| DeferredFile { meta })
			.collect(),
		source,
		formats,
	};
	let com: ComObject<DeferredDataObject> = data_object.into_object();
	let iface: IDataObject = com.into_interface();
	// SAFETY: 本线程是已初始化的 STA。
	unsafe { OleSetClipboard(Some(&iface)) }
		.map_err(|error| format!("OleSetClipboard 失败：{error}"))?;
	CURRENT_OBJECT.with(|slot| *slot.borrow_mut() = Some(iface));
	Ok(())
}

/// 撤掉我们自己挂的延迟渲染剪贴板。
///
/// 会话断开/退出时必须调用：否则 OLE 仍持有 `IDataObject`，系统做剪贴板
/// flush 时会挨个找我们要 `CFSTR_FILECONTENTS`，而字节已经拉不到了——
/// 表现就是关窗口卡在「未响应」。**异步投递，不等回执**，免得关闭流程
/// 被 STA 线程拖住。
pub fn clear_deferred() {
	if let Some(tx) = sta_sender() {
		let _ = tx.send(StaCommand::Clear);
	}
}

/// 把一批远端文件以延迟渲染方式挂上剪贴板。成功后目标程序粘贴时才拉字节。
///
/// 实际工作投给剪贴板 STA 线程（见 [`clipboard_thread_main`]），本函数只等
/// 一次「挂上了没有」的回执，通常几毫秒。
pub fn install_deferred(
	files: Vec<RemoteFileMeta>,
	source: Arc<dyn ChunkSource>,
) -> Result<(), String> {
	if files.is_empty() {
		return Ok(());
	}
	let Some(tx) = sta_sender() else {
		return Err("剪贴板线程不可用".to_string());
	};
	let (reply, reply_rx) = std::sync::mpsc::channel();
	tx.send(StaCommand::Install {
		files,
		source,
		reply,
	})
	.map_err(|_| "剪贴板线程已退出".to_string())?;
	match reply_rx.recv_timeout(std::time::Duration::from_secs(5)) {
		Ok(result) => result,
		Err(_) => Err("剪贴板线程无响应（挂载超时）".to_string()),
	}
}
