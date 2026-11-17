//! 系统剪贴板的**文件**读写（S10，RDP 文件剪贴板的本机一侧）。
//!
//! `arboard` 只认文本与位图，Finder 里「复制文件」落在剪贴板上的
//! 文件 URL 列表它拿不到，远端下载完的文件也没法挂上去给 Finder 粘贴。
//! 这里直接走 `NSPasteboard` 的几个选择器：
//!
//! | 方向 | 调用 |
//! | --- | --- |
//! | 读（本地 → 远端） | `readObjectsForClasses:@[NSURL]` → 文件路径列表 |
//! | 写（远端 → 本地） | `clearContents` + `writeObjects:@[NSURL…]` |
//!
//! `changeCount` 是剪贴板的全局单调序号：**每变一次加一**。
//! 轮询方用它判断「有没有变过」，也用它识别「我们刚写进去的」——
//! 写完记下当时的序号，看到同一个序号就是自己人，不能再把这批
//! 文件推回远端（否则远端→本地→远端来回乒乓）。
//!
//! ## 线程与内存
//!
//! 调用须在**主线程**（AppKit 的常规约束；界面代码本来就在主线程）。
//! 内存上只借不持有：所有返回的 ObjC 对象都是 autoreleased，
//! 在本函数的 `autoreleasepool` 里活着，拷成 Rust 值之后随池释放，
//! 不做任何手动 retain/release。

/// 读到的一批本地文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardFiles {
	/// 读时的 `changeCount`。
	pub change_count: i64,
	/// 文件路径。**文件夹被跳过**：FileContents 协议按单文件取字节，
	/// 整棵目录树带不过去（Windows 侧描述表也没法展开目录）。
	pub paths: Vec<std::path::PathBuf>,
	/// 被跳过的条数（目前只有文件夹与取不到路径的文件 URL）。
	///
	/// 复制文件夹时 `paths` 为空但 `skipped > 0`——上层靠它区分
	/// 「剪贴板里根本没文件」与「有文件但我们带不过去」，后者必须提示，
	/// 否则用户看到的就是「按了 Cmd+C 什么反应都没有」。
	/// 非 file URL（浏览器复制的网页地址）不计入这里：那不是文件。
	pub skipped: usize,
}

#[cfg(target_os = "macos")]
mod imp {
	use objc2::class;
	use objc2::msg_send;
	use objc2::rc::autoreleasepool;
	use objc2::runtime::AnyObject;

	type Id = *mut AnyObject;

	/// `NSPasteboard.generalPasteboard`。
	fn general_pasteboard() -> Id {
		// SAFETY: ObjC 运行时调用；返回共享单例，本次调用内使用。
		unsafe { msg_send![class!(NSPasteboard), generalPasteboard] }
	}

	/// `NSString` → Rust 字符串（拷贝 UTF-8 字节，不持有对象）。
	///
	/// # Safety
	/// `string` 必须是有效的 `NSString`。
	unsafe fn nsstring_to_string(string: Id) -> String {
		if string.is_null() {
			return String::new();
		}
		// SAFETY: NSString 的 UTF8String 返回 NUL 结尾的 C 串，对象活着就有效。
		let utf8: *const std::os::raw::c_char = unsafe { msg_send![string, UTF8String] };
		if utf8.is_null() {
			return String::new();
		}
		// SAFETY: 指针来自 NSString，NUL 结尾。
		unsafe { std::ffi::CStr::from_ptr(utf8) }
			.to_string_lossy()
			.into_owned()
	}

	/// Rust 字符串 → `NSString`（autoreleased）。
	fn nsstring_from(text: &str) -> Id {
		let c = std::ffi::CString::new(text).unwrap_or_default();
		// SAFETY: stringWithUTF8String 返回 autoreleased 对象。
		unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
	}

	/// 路径是不是已存在的目录。
	///
	/// # Safety
	/// `path` 必须是有效的 `NSString`。
	unsafe fn path_is_directory(path: Id) -> bool {
		// SAFETY: defaultManager 是共享单例。
		let manager: Id = unsafe { msg_send![class!(NSFileManager), defaultManager] };
		// arm64 上 ObjC 的 BOOL 是 `objc2::runtime::Bool`（类型码 'B'）。
		// 输出参数要用它的包装类型，裸 `&mut bool` 不满足 objc2 的
		// ConvertArgument 约束。
		let mut is_dir = objc2::runtime::Bool::new(false);
		// SAFETY: 输出参数是 BOOL*，按 objc2 的 Bool 包装传。
		let exists: bool = unsafe {
			msg_send![manager,
				fileExistsAtPath: path,
				isDirectory: &mut is_dir]
		};
		exists && is_dir.as_bool()
	}

	/// 读剪贴板上的文件 URL 列表。
	///
	/// # Safety
	/// 必须在主线程调用（AppKit 约束）。
	pub unsafe fn read_files() -> Option<super::ClipboardFiles> {
		autoreleasepool(|_pool| {
			let pasteboard = general_pasteboard();
			if pasteboard.is_null() {
				return None;
			}
			// SAFETY: 剪贴板序号是普通整数。
			let change_count: i64 = unsafe { msg_send![pasteboard, changeCount] };

			// 只认 NSURL 这一类型的对象；options 传 nil
			let classes: Id = unsafe { msg_send![class!(NSArray), arrayWithObject: class!(NSURL)] };
			let objects: Id = unsafe {
				msg_send![pasteboard,
					readObjectsForClasses: classes,
					options: std::ptr::null::<AnyObject>()]
			};
			if objects.is_null() {
				return None;
			}
			// SAFETY: count 是 NSUInteger。
			let count: usize = unsafe { msg_send![objects, count] };

			let mut paths = Vec::new();
			let mut skipped = 0usize;
			for index in 0..count {
				// SAFETY: index 在 count 范围内。
				let url: Id = unsafe { msg_send![objects, objectAtIndex: index] };
				if url.is_null() {
					skipped += 1;
					continue;
				}
				// 不是文件 URL（例如浏览器复制的网页地址也是 NSURL）：
				// 直接忽略，**不计入 skipped**——否则会被当成「复制了文件夹」，
				// 连带把文本上报也挡掉（纯文本剪贴板反而传不过去）。
				// SAFETY: url 是有效 NSURL。
				let is_file_url: bool = unsafe { msg_send![url, isFileURL] };
				if !is_file_url {
					continue;
				}
				// path 属性：已解码的本地路径字符串
				// SAFETY: url 是有效 NSURL。
				let path: Id = unsafe { msg_send![url, path] };
				let text = unsafe { nsstring_to_string(path) };
				if text.is_empty() || unsafe { path_is_directory(path) } {
					skipped += 1;
					continue;
				}
				paths.push(std::path::PathBuf::from(text));
			}
			Some(super::ClipboardFiles {
				change_count,
				paths,
				skipped,
			})
		})
	}

	/// 把一组本地文件挂上剪贴板（Finder 里即可 Cmd+V）。
	///
	/// 返回写入时的 `changeCount`；失败返回 `None`。
	///
	/// # Safety
	/// 必须在主线程调用。
	pub unsafe fn write_files(paths: &[std::path::PathBuf]) -> Option<i64> {
		if paths.is_empty() {
			return None;
		}
		autoreleasepool(|_pool| {
			let pasteboard = general_pasteboard();
			if pasteboard.is_null() {
				return None;
			}
			// clearContents 之后 writeObjects 覆盖写——多类型剪贴板的
			// 规范用法；不 clear 会残留旧类型，Finder 可能粘出旧内容。
			// **它返回 NSInteger（新的 changeCount），不是 void**：
			// objc2 的 msg_send 对返回类型做运行时校验，声明成 ()
			// 会在真机上直接 panic（invalid message send … expected 'q'）。
			// SAFETY: 返回普通整数。
			let _cleared: i64 = unsafe { msg_send![pasteboard, clearContents] };

			// 路径 → NSURL fileURLWithPath:
			let mut urls: Vec<Id> = Vec::with_capacity(paths.len());
			for path in paths {
				let Some(text) = path.to_str() else {
					continue;
				};
				let ns_path = nsstring_from(text);
				// SAFETY: 输入是有效 NSString。
				let url: Id = unsafe { msg_send![class!(NSURL), fileURLWithPath: ns_path] };
				if !url.is_null() {
					urls.push(url);
				}
			}
			if urls.is_empty() {
				return None;
			}
			// SAFETY: urls 数组在调用期间有效；返回 autoreleased 数组。
			let array: Id = unsafe {
				msg_send![class!(NSArray), arrayWithObjects: urls.as_ptr(), count: urls.len()]
			};
			// SAFETY: array 是有效 NSArray。
			let ok: bool = unsafe { msg_send![pasteboard, writeObjects: array] };
			if !ok {
				tracing::warn!(
					"[剪贴板] 写系统剪贴板失败（{} 个文件），Finder 里将无法粘贴",
					paths.len()
				);
				return None;
			}

			// **再显式声明一次老格式 `NSFilenamesPboardType`（路径字符串数组）。**
			//
			// `writeObjects:@[NSURL]` 只声明 `public.file-url`，Finder 的
			// 「粘贴项」判定对这个新格式并不总是买账（真机回报：右键菜单
			// 里没有粘贴项、Cmd+V 只出一声 beep，而剪贴板里的
			// `readObjectsForClasses:@[NSURL]` 却能读到文件）。
			// `NSFilenamesPboardType` 是 Finder 认识二十年的老格式，
			// 显式写上双保险——两个类型同时存在时 Finder 一定认为可粘贴。
			let mut names: Vec<Id> = Vec::with_capacity(paths.len());
			for path in paths {
				if let Some(text) = path.to_str() {
					let ns_text = nsstring_from(text);
					if !ns_text.is_null() {
						names.push(ns_text);
					}
				}
			}
			if !names.is_empty() {
				// SAFETY: names 在调用期间有效。
				let name_array: Id = unsafe {
					msg_send![class!(NSArray), arrayWithObjects: names.as_ptr(), count: names.len()]
				};
				let names_type = nsstring_from("NSFilenamesPboardType");
				// SAFETY: 属性列表（这里是 NSString 数组）由 AppKit 序列化。
				let _written: bool = unsafe {
					msg_send![pasteboard, setPropertyList: name_array, forType: names_type]
				};
			}

			// SAFETY: 普通整数。
			let change_count: i64 = unsafe { msg_send![pasteboard, changeCount] };
			tracing::info!(
				"[剪贴板] 已把 {} 个文件挂上系统剪贴板（changeCount={change_count}），Finder 里可直接粘贴",
				paths.len()
			);
			Some(change_count)
		})
	}
}

#[cfg(target_os = "macos")]
pub use imp::{read_files, write_files};

#[cfg(target_os = "windows")]
mod imp {
	use std::os::windows::ffi::{OsStrExt, OsStringExt};
	use std::path::PathBuf;

	use windows_sys::Win32::Foundation::{BOOL, GlobalFree, HANDLE, HGLOBAL};
	use windows_sys::Win32::System::DataExchange::{
		CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
		IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
	};
	use windows_sys::Win32::System::Memory::{
		GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
	};
	use windows_sys::Win32::System::Ole::CF_HDROP;
	use windows_sys::Win32::UI::Shell::{DROPFILES, DragQueryFileW};

	/// 剪贴板的 `changeCount`：Windows 用系统级单调序号，语义与 macOS 的
	/// `changeCount` 完全一样（每次剪贴板变化就变），可直接复用上层判重逻辑。
	/// `OpenClipboard` 不一定非要成功才能读序号——序号是全局的。
	fn sequence() -> i64 {
		// SAFETY: 无参数、无副作用，返回 u32 序号。
		unsafe { GetClipboardSequenceNumber() as i64 }
	}

	/// 资源管理器里「复制文件」落的是 `CF_HDROP`；`DragQueryFileW` 是唯一
	/// 能把它展开成路径列表的 API（传 `0xFFFFFFFF` 先问条数）。
	///
	/// # Safety
	/// 调用线程最好是主线程（剪贴板 API 本身不要求，但 COM 化的格式要求 STA）。
	pub unsafe fn read_files() -> Option<super::ClipboardFiles> {
		// SAFETY: 空 HWND 表示当前线程 owned；主线程调用。
		if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
			tracing::debug!("[剪贴板] OpenClipboard 失败（读），跳过");
			return None;
		}
		// 任何提前 return 都必须关掉剪贴板，否则别的程序全粘不了。
		let result = (|| {
			if unsafe { IsClipboardFormatAvailable(CF_HDROP as u32) } == 0 {
				return None;
			}
			// SAFETY: 格式可用，句柄在剪贴板关闭前有效。
			let handle: HANDLE = unsafe { GetClipboardData(CF_HDROP as u32) };
			if handle.is_null() {
				return None;
			}
			// SAFETY: HDROP 就是 GetClipboardData(CF_HDROP as u32) 的返回句柄。
			let count = unsafe { DragQueryFileW(handle, 0xFFFF_FFFF, std::ptr::null_mut(), 0) };
			if count == 0 || count == 0xFFFF_FFFF {
				return None;
			}

			let mut paths = Vec::with_capacity(count as usize);
			let mut skipped = 0usize;
			for index in 0..count {
				// SAFETY: 先传空缓冲问长度（含结尾 NUL）。
				let needed = unsafe { DragQueryFileW(handle, index, std::ptr::null_mut(), 0) };
				if needed == 0 || needed == 0xFFFF_FFFF {
					skipped += 1;
					continue;
				}
				let mut wide = vec![0u16; needed as usize + 1];
				// SAFETY: 缓冲区按 needed 分配，DragQueryFileW 写入不超过它。
				let written =
					unsafe { DragQueryFileW(handle, index, wide.as_mut_ptr(), wide.len() as u32) };
				if written == 0 {
					skipped += 1;
					continue;
				}
				wide.truncate(wide.iter().position(|c| *c == 0).unwrap_or(wide.len()));
				let path = PathBuf::from(std::ffi::OsString::from_wide(&wide));
				// 与 macOS 一致：文件夹整棵带不过去（FileContents 按单文件取字节）。
				if path.is_dir() || path.as_os_str().is_empty() {
					skipped += 1;
					continue;
				}
				paths.push(path);
			}
			Some(super::ClipboardFiles {
				change_count: sequence(),
				paths,
				skipped,
			})
		})();
		// SAFETY: 前面 OpenClipboard 成功，这里必须配对关闭。
		unsafe { CloseClipboard() };
		result
	}

	/// 构造 `CF_HDROP` 的字节布局：20 字节 packed 头 + UTF-16LE 路径表（双 NUL）。
	/// 抽成纯函数是为了单测能核对——错一个字节资源管理器就读不到文件。
	pub(super) fn build_dropfiles(paths: &[PathBuf]) -> Vec<u8> {
		// DROPFILES 是 packed(1)，必须按字节写，不能用结构体指针赋值
		// （Rust 默认对齐后续字段，偏移会和 Win32 约定不符）。
		let header_size = std::mem::size_of::<DROPFILES>();
		let mut bytes: Vec<u8> = Vec::new();
		bytes.extend_from_slice(&(header_size as u32).to_le_bytes()); // pFiles
		bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.x
		bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.y
		bytes.extend_from_slice(&0i32.to_le_bytes()); // fNC
		bytes.extend_from_slice(&1i32.to_le_bytes()); // fWide = TRUE
		debug_assert_eq!(
			bytes.len(),
			header_size,
			"DROPFILES 头必须是 packed 20 字节"
		);

		for path in paths {
			for unit in path.as_os_str().encode_wide() {
				bytes.extend_from_slice(&unit.to_le_bytes());
			}
			bytes.extend_from_slice(&0u16.to_le_bytes()); // 每个路径的 NUL
		}
		bytes.extend_from_slice(&0u16.to_le_bytes()); // 整个表的额外 NUL
		bytes
	}

	/// 把远端下载好的文件挂到系统剪贴板：`CF_HDROP` + UTF-16LE 双 NUL 路径表。
	/// 资源管理器里按 Ctrl+V 就能粘到用户选的目录。
	///
	/// # Safety
	/// 主线程调用；`SetClipboardData` 成功后内存所有权交给系统，不能再 `GlobalFree`。
	pub unsafe fn write_files(paths: &[PathBuf]) -> Option<i64> {
		if paths.is_empty() {
			return None;
		}

		let bytes = build_dropfiles(paths);

		// SAFETY: GMEM_MOVEABLE 是 SetClipboardData 的硬性要求。
		let global: HGLOBAL = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) };
		if global.is_null() {
			tracing::warn!("[剪贴板] GlobalAlloc 失败（{} 字节）", bytes.len());
			return None;
		}
		// SAFETY: 刚分配的句柄，锁一次取可写指针。
		let ptr = unsafe { GlobalLock(global) } as *mut u8;
		if ptr.is_null() {
			// SAFETY: 独占句柄，锁失败说明它仍可释放。
			unsafe { GlobalFree(global) };
			return None;
		}
		// SAFETY: 分配了 bytes.len()，逐字节写进去。
		unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) };
		// SAFETY: 与上面的 GlobalLock 配对。
		unsafe { GlobalUnlock(global) };

		// SAFETY: 主线程；失败时要自己释放句柄（成功则系统接管）。
		if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
			unsafe { GlobalFree(global) };
			tracing::warn!("[剪贴板] OpenClipboard 失败（写）");
			return None;
		}
		let result = (|| {
			// SAFETY: 打开剪贴板后才能清空。
			if unsafe { EmptyClipboard() } == 0 {
				return None;
			}
			// SAFETY: global 里是完整的 CF_HDROP 数据；成功后所有权转给系统。
			let set: HANDLE = unsafe { SetClipboardData(CF_HDROP as u32, global) };
			if set.is_null() {
				return None;
			}
			Some(sequence())
		})();
		// SAFETY: 与 OpenClipboard 配对。
		unsafe { CloseClipboard() };

		if result.is_none() {
			// SetClipboardData 失败时系统没接管，要自己释放。
			unsafe { GlobalFree(global) };
		} else {
			tracing::info!(
				"[剪贴板] 已把 {} 个文件挂上系统剪贴板（CF_HDROP），资源管理器里可直接粘贴",
				paths.len()
			);
		}
		result
	}

	// 只用到导入但类型检查需要确认的常量；编译期断言 packed 布局符合 Win32。
	const _: () = {
		assert!(
			std::mem::size_of::<DROPFILES>() == 20,
			"DROPFILES 必须是 20 字节"
		);
		let _: BOOL = 0;
	};
}

#[cfg(target_os = "windows")]
pub use imp::{read_files, write_files};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
	/// 其它平台：暂无实现，给空实现占位。
	pub fn read_files() -> Option<super::ClipboardFiles> {
		None
	}

	pub fn write_files(_paths: &[std::path::PathBuf]) -> Option<i64> {
		None
	}
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use imp::{read_files, write_files};

/// 读剪贴板上的文件列表（封装掉平台差异与 unsafe 标注）。
pub fn clipboard_files() -> Option<ClipboardFiles> {
	// 原生实现里有平台 API 调用；其它平台的占位是安全函数，
	// `unsafe` 会报 unused_unsafe，所以只在有实现的平台包。
	#[cfg(any(target_os = "macos", target_os = "windows"))]
	{
		// SAFETY: 主线程调用（界面代码调用方保证）。
		unsafe { read_files() }
	}
	#[cfg(not(any(target_os = "macos", target_os = "windows")))]
	{
		read_files()
	}
}

/// 把本地文件挂上剪贴板，返回写入时的 `changeCount`。
pub fn put_files(paths: &[std::path::PathBuf]) -> Option<i64> {
	#[cfg(any(target_os = "macos", target_os = "windows"))]
	{
		// SAFETY: 主线程调用（界面代码调用方保证）。
		unsafe { write_files(paths) }
	}
	#[cfg(not(any(target_os = "macos", target_os = "windows")))]
	{
		write_files(paths)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Windows 的 `CF_HDROP` 字节布局：头 20 字节、pFiles=20、fWide=1、
	/// 每个路径 UTF-16LE 后跟 NUL、整个表再跟一个 NUL。
	/// 这一步纯布局检查，不碰系统剪贴板，CI 里也能跑。
	#[cfg(target_os = "windows")]
	#[test]
	fn dropfiles_layout() {
		use imp::build_dropfiles;
		let paths = vec![
			std::path::PathBuf::from(r"C:\dev\a.txt"),
			std::path::PathBuf::from(r"C:\dev\b.md"),
		];
		let bytes = build_dropfiles(&paths);

		assert_eq!(
			u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
			20,
			"pFiles 应为 20"
		);
		assert_eq!(
			u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
			1,
			"fWide 应为 1"
		);
		// 头部之后的 UTF-16LE 内容：把路径段拼回来核对。
		let wide: Vec<u16> = bytes[20..]
			.chunks_exact(2)
			.map(|c| u16::from_le_bytes([c[0], c[1]]))
			.collect();
		let text = String::from_utf16(&wide).unwrap();
		assert!(text.contains(r"C:\dev\a.txt"), "路径表缺 a.txt：{text:?}");
		assert!(text.contains(r"C:\dev\b.md"), "路径表缺 b.md：{text:?}");
		assert_eq!(
			wide.iter().filter(|c| **c == 0).count(),
			3,
			"两条路径 + 表尾共 3 个 NUL"
		);
	}

	/// 写入 → 读回的往返。要求剪贴板可用（CI 无头环境跳过）。
	///
	/// **默认不跑**：它会改写用户的系统剪贴板，而且留下的临时文件随即被删，
	/// 剪贴板上就多出一批**死链接**——Daisy 的剪贴板桥接会把它当成
	/// 「有文件要传」，于是拒绝「开嗓」（`should_arm`），远端复制整个哑掉。
	/// 2026-11-14 就是这么把用户正在测的会话搞挂的。
	///
	/// 需要时手动跑：`cargo test -p daisy-app -- --ignored pasteboard`，
	/// 跑完记得在 Finder 里随便复制点东西把剪贴板换回来。
	#[test]
	#[ignore = "会改写用户系统剪贴板，默认不跑"]
	fn file_urls_round_trip() {
		let dir = std::env::temp_dir().join(format!("pb-rt-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let a = dir.join("往返A.txt");
		let b = dir.join("roundtrip-b.md");
		std::fs::write(&a, b"a").unwrap();
		std::fs::write(&b, b"b").unwrap();

		// 写
		let change_count = put_files(&[a.clone(), b.clone()]);
		if change_count.is_none() {
			// 剪贴板不可用（无头 CI）——不强求
			return;
		}

		// 读
		let read = clipboard_files().expect("刚写完应能读回");
		assert!(
			read.paths.contains(&a) && read.paths.contains(&b),
			"读回的文件列表缺项：{:?}",
			read.paths
		);
		std::fs::remove_dir_all(&dir).ok();
	}
}
