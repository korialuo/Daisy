//! 诊断：dump 当前系统剪贴板的真实内容（S10 排查用）。
//!
//! 打印剪贴板里登记的所有类型名，再用与 `pasteboard_files::read_files`
//! 完全相同的方式取 NSURL 列表，并逐项打印 `path` / `filePathURL.path` /
//! `absoluteString` 三个取值——`path` 对 file reference URL 会返回 nil，
//! 这正是「Finder Cmd+C 读到空列表」的典型成因。
//!
//! 运行：`cargo run -p daisy-app --example pb_dump`

#[cfg(target_os = "macos")]
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

/// 按 **Finder Cmd+C 的真实做法**写剪贴板：先写文件 URL，再往同一块
/// 剪贴板上**追加**纯文本（Finder 会把这个文件的显示名/路径放进去）。
///
/// 这是「Finder 复制文件 → 远端 Ctrl+V 无反应」的复现器：只有同时存在
/// 两种类型的剪贴板才能暴露「文本格式列表覆盖文件格式列表」的问题。
#[cfg(target_os = "macos")]
fn write_like_finder(paths: &[String]) {
	use objc2::class;
	use objc2::msg_send;
	use objc2::rc::autoreleasepool;
	use objc2::runtime::AnyObject;

	type Id = *mut AnyObject;

	fn nsstring_from(text: &str) -> Id {
		let c = std::ffi::CString::new(text).unwrap_or_default();
		// SAFETY: 返回 autoreleased 对象。
		unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
	}

	autoreleasepool(|_| unsafe {
		let pasteboard: Id = msg_send![class!(NSPasteboard), generalPasteboard];
		let _: i64 = msg_send![pasteboard, clearContents];

		let urls: Vec<Id> = paths
			.iter()
			.map(|p| msg_send![class!(NSURL), fileURLWithPath: nsstring_from(p)])
			.collect();
		let array: Id = msg_send![
			class!(NSArray),
			arrayWithObjects: urls.as_ptr(),
			count: urls.len()
		];
		let ok: bool = msg_send![pasteboard, writeObjects: array];
		println!("写文件 URL：{ok}（{} 项）", urls.len());

		// 追加文本：不清空剪贴板，只是多一种类型——Finder 就是这么干的
		let name = paths
			.first()
			.and_then(|p| p.rsplit('/').next())
			.unwrap_or("clipboard")
			.to_string();
		let wrote: bool = msg_send![
			pasteboard,
			setString: nsstring_from(&name),
			forType: nsstring_from("public.utf8-plain-text")
		];
		println!("追加纯文本「{name}」：{wrote}");

		let change_count: i64 = msg_send![pasteboard, changeCount];
		println!("changeCount = {change_count}");
	});
}

#[cfg(target_os = "macos")]
fn main() {
	// `pb_dump finder <路径…>`：先按 Finder 的方式写剪贴板，再 dump 一遍
	let args: Vec<String> = std::env::args().collect();
	if args.len() > 2 && args[1] == "finder" {
		write_like_finder(&args[2..]);
		println!("--- 写入后的剪贴板 ---");
	}
	use objc2::class;
	use objc2::msg_send;
	use objc2::rc::autoreleasepool;
	use objc2::runtime::AnyObject;

	type Id = *mut AnyObject;

	autoreleasepool(|_| unsafe {
		let pasteboard: Id = msg_send![class!(NSPasteboard), generalPasteboard];
		let change_count: i64 = msg_send![pasteboard, changeCount];
		println!("changeCount = {change_count}");

		// 1. 所有登记的类型名
		let types: Id = msg_send![pasteboard, types];
		if !types.is_null() {
			let count: usize = msg_send![types, count];
			println!("types ({count}):");
			for i in 0..count {
				let name: Id = msg_send![types, objectAtIndex: i];
				let utf8: *const std::os::raw::c_char = msg_send![name, UTF8String];
				let text = if utf8.is_null() {
					"<nil>".to_string()
				} else {
					std::ffi::CStr::from_ptr(utf8)
						.to_string_lossy()
						.into_owned()
				};
				println!("  - {text}");
			}
		} else {
			println!("types = nil（剪贴板为空）");
		}

		// 2. 与 read_files 相同的方式取 NSURL
		let classes: Id = msg_send![class!(NSArray), arrayWithObject: class!(NSURL)];
		let objects: Id = msg_send![
			pasteboard,
			readObjectsForClasses: classes,
			options: std::ptr::null::<AnyObject>()
		];
		if objects.is_null() {
			println!("readObjectsForClasses: NSURL = nil");
			return;
		}
		let count: usize = msg_send![objects, count];
		println!("NSURL objects = {count}");
		for i in 0..count {
			let url: Id = msg_send![objects, objectAtIndex: i];

			fn describe(obj: Id) -> String {
				if obj.is_null() {
					return "<nil>".to_string();
				}
				// SAFETY: 调用方只传 NSString 或 nil（已排除）。
				let s: *const std::os::raw::c_char = unsafe { msg_send![obj, UTF8String] };
				if s.is_null() {
					"<nil>".to_string()
				} else {
					// SAFETY: NUL 结尾。
					unsafe { std::ffi::CStr::from_ptr(s) }
						.to_string_lossy()
						.into_owned()
				}
			}

			let path: Id = msg_send![url, path];
			let absolute: Id = msg_send![url, absoluteString];
			let file_path_url: Id = msg_send![url, filePathURL];
			let inner_path: Id = if file_path_url.is_null() {
				std::ptr::null_mut()
			} else {
				msg_send![file_path_url, path]
			};
			// isFileURL 决定 path 是否有意义
			let is_file_url: bool = msg_send![url, isFileURL];
			println!("  [{i}] isFileURL = {is_file_url}");
			println!("      path             = {}", describe(path));
			println!("      filePathURL.path = {}", describe(inner_path));
			println!("      absoluteString   = {}", describe(absolute));
		}
	});

	// 文本侧：arboard 在这份剪贴板上读到什么（决定文件列表会不会被文本覆盖）
	match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
		Ok(text) => println!("arboard text = {text:?}（{} 字）", text.chars().count()),
		Err(error) => println!("arboard text: {error}"),
	}

	// Finder 判「能不能粘贴」看的是**类型里有没有可读数据**，
	// 而不是类型名在不在列表里——这里把各类型的实际取值打出来。
	unsafe {
		let pasteboard: Id = msg_send![class!(NSPasteboard), generalPasteboard];

		fn nsstr(text: &str) -> Id {
			let c = std::ffi::CString::new(text).unwrap_or_default();
			// SAFETY: 返回 autoreleased NSString。
			unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
		}

		let describe = |obj: Id| -> String {
			if obj.is_null() {
				return "<nil>".to_string();
			}
			// 外层已是 unsafe 块（只传 NSString / nil）。
			let s: *const std::os::raw::c_char = msg_send![obj, UTF8String];
			if s.is_null() {
				"<nil>".to_string()
			} else {
				std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned()
			}
		};

		let wanted: Id = msg_send![
			class!(NSArray),
			arrayWithObjects: [nsstr("NSFilenamesPboardType"), nsstr("public.file-url"), nsstr("public.url")]
				.as_ptr(),
			count: 3usize
		];
		let best: Id = msg_send![pasteboard, availableTypeFromArray: wanted];
		println!("availableType(文件类) = {}", describe(best));

		// NSFilenamesPboardType 的 property list（Finder 读的就是这个）
		let names_type = nsstr("NSFilenamesPboardType");
		let plist: Id = msg_send![pasteboard, propertyListForType: names_type];
		if plist.is_null() {
			println!("NSFilenamesPboardType 数据 = <nil>（Finder 会认为没有可粘贴的项）");
		} else {
			let count: usize = msg_send![plist, count];
			println!("NSFilenamesPboardType 数据 = {count} 项");
			for i in 0..count {
				let item: Id = msg_send![plist, objectAtIndex: i];
				println!("   {i}: {}", describe(item));
			}
		}
	}
}

#[cfg(not(target_os = "macos"))]
fn main() {
	println!("仅 macOS 可用");
}
