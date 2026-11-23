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
	DATA_S_SAMEFORMATETC, DV_E_FORMATETC, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, HGLOBAL,
	S_FALSE, S_OK,
};
use windows::Win32::System::Com::{
	DATADIR_GET, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
	IEnumFORMATETC_Impl, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE,
	STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END,
	STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{OleInitialize, OleSetClipboard};
use windows::Win32::UI::Shell::FILEDESCRIPTORW;
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
				let n = bytes.len() as u32;
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
		// 只填 cbSize（资源管理器据此建目标文件），其余留零。
		unsafe { (*pstatstg).cbSize = self.size };
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
	fn build_descriptor(&self) -> Vec<u8> {
		let n = self.files.len();
		let mut bytes = Vec::new();
		bytes.extend_from_slice(&(n as u32).to_le_bytes());
		for file in &self.files {
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
		bytes
	}
}

impl IDataObject_Impl for DeferredDataObject_Impl {
	fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
		if pformatetcin.is_null() {
			return Err(E_POINTER.into());
		}
		// SAFETY: 调用方保证指针有效。
		let etc = unsafe { &*pformatetcin };
		if etc.cfFormat == self.formats.file_descriptor {
			let bytes = self.build_descriptor();
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
			let index = etc.lindex.max(0) as u32;
			let Some(file) = self.files.get(index as usize) else {
				return Err(E_INVALIDARG.into());
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
		if etc.cfFormat == self.formats.file_descriptor
			|| etc.cfFormat == self.formats.file_contents
		{
			S_OK
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
		let enumerator = FormatEnumerator {
			formats: vec![
				self.format_etc(self.formats.file_descriptor, TYMED_HGLOBAL.0 as u32, -1),
				self.format_etc(self.formats.file_contents, TYMED_ISTREAM.0 as u32, 0),
			],
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

/// 把一批远端文件以延迟渲染方式挂上剪贴板。成功后目标程序粘贴时才拉字节。
/// 必须在 STA 线程（UI 主线程）调用。
pub fn install_deferred(
	files: Vec<RemoteFileMeta>,
	source: Arc<dyn ChunkSource>,
) -> windows::core::Result<()> {
	if files.is_empty() {
		return Ok(());
	}
	// SAFETY: 幂等；UI 主线程已是 STA。
	unsafe { OleInitialize(None).ok() };

	let formats = register_formats();
	let deferred_files = files
		.into_iter()
		.map(|meta| DeferredFile { meta })
		.collect();
	let data_object = DeferredDataObject {
		files: deferred_files,
		source,
		formats,
	};
	let com: ComObject<DeferredDataObject> = data_object.into_object();
	let iface: IDataObject = com.into_interface();
	// SAFETY: iface 在 OleSetClipboard 成功后由 OLE 持有引用。
	unsafe { OleSetClipboard(Some(&iface)) }
}
