//! macOS 延迟渲染剪贴板（Finder File Promise）。
//!
//! Finder 的 File Promise：剪贴板只声明「有这些文件」，`NSFilePromiseProvider`
//! 的 delegate 在用户粘贴时才收到 `writePromiseToURL:completionHandler:`，
//! 那一刻才拉字节写文件——与 mstsc 延迟渲染等价。

use std::sync::{Arc, Mutex};

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class};
use objc2_app_kit::{
	NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSPasteboard, NSPasteboardWriting,
};
use objc2_foundation::{NSError, NSNumber, NSObject, NSObjectProtocol, NSString, NSURL};

use daisy_rdp::RemoteFileMeta;

/// 按需读取字节的来源（`read` 同步阻塞直到数据到达）。
pub trait ChunkSource: Send + Sync + 'static {
	fn read(&self, index: u32, offset: u64, len: u32) -> Result<Vec<u8>, String>;
}

struct DelegateState {
	source: Arc<dyn ChunkSource>,
	files: Vec<RemoteFileMeta>,
}

#[derive(Default)]
struct FilePromiseDelegateIvars {
	state: Mutex<Option<DelegateState>>,
}

define_class!(
	#[unsafe(super(NSObject))]
	#[name = "DaisyFilePromiseDelegate"]
	#[ivars = FilePromiseDelegateIvars]
	struct FilePromiseDelegate;

	unsafe impl NSObjectProtocol for FilePromiseDelegate {}

	#[allow(non_snake_case)] // ObjC selector 名，必须保持
	unsafe impl NSFilePromiseProviderDelegate for FilePromiseDelegate {
		#[unsafe(method_id(filePromiseProvider:fileNameForType:))]
		fn filePromiseProvider_fileNameForType(
			&self,
			provider: &NSFilePromiseProvider,
			_file_type: &NSString,
		) -> Retained<NSString> {
			let index = provider_index(provider);
			let guard = self.ivars().state.lock().ok();
			let name = guard
				.and_then(|s| {
					s.as_ref()?
						.files
						.get(index as usize)
						.map(|m| m.name.clone())
				})
				.unwrap_or_else(|| "file".to_string());
			NSString::from_str(&name)
		}

		#[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
		fn filePromiseProvider_writePromiseToURL_completionHandler(
			&self,
			provider: &NSFilePromiseProvider,
			url: &NSURL,
			completion_handler: &DynBlock<dyn Fn(*mut NSError)>,
		) {
			let index = provider_index(provider);
			let guard = self.ivars().state.lock().ok();
			let info = guard.as_ref().and_then(|s| {
				let s = s.as_ref()?;
				s.files
					.get(index as usize)
					.map(|m| (Arc::clone(&s.source), m.size, m.name.clone()))
			});
			// 锁只用来取快照，不能在写文件期间一直握着（同一批可能并发粘多个文件）。
			drop(guard);
			let Some((source, size, name)) = info else {
				tracing::warn!("[剪贴板] File Promise 收到未知序号 {index} 的写入请求，放弃");
				call_completion(completion_handler, true);
				return;
			};
			let Some(path) = url.path() else {
				tracing::warn!("[剪贴板] File Promise 第 {index} 个「{name}」没有可写路径");
				call_completion(completion_handler, true);
				return;
			};
			let path = path.to_string();
			// **这条日志就是「点粘贴才开始传输」的证据**：它出现之前一个字节都没拉过。
			tracing::info!(
				"[剪贴板] File Promise 开始写入第 {index} 个「{name}」→ {path}（远端 {size} 字节）"
			);
			let handler = SendCompletion(RcBlock::into_raw(completion_handler.copy()) as usize);
			let started = std::time::Instant::now();
			std::thread::spawn(move || {
				let result = write_file_from_source(&source, index, size, &path);
				match &result {
					Ok(written) => tracing::info!(
						"[剪贴板] File Promise 完成：第 {index} 个「{name}」{written} 字节，用时 {} ms",
						started.elapsed().as_millis()
					),
					Err(error) => tracing::warn!(
						"[剪贴板] File Promise 失败：第 {index} 个「{name}」{error}"
					),
				}
				// SAFETY: 裸指针来自 into_raw，系统持有 block 到会话结束。
				let block = unsafe { &*(handler.0 as *const block2::Block<dyn Fn(*mut NSError)>) };
				call_completion(block, result.is_err());
			});
		}
	}
);

impl FilePromiseDelegate {
	fn new_with_state(state: DelegateState) -> Retained<Self> {
		// objc2 的 ivars 是「未初始化」状态，必须用 `set_ivars` 建立，
		// 直接 `self.ivars()` 赋值会 panic（tried to access uninitialized
		// instance variable）。之后照常调 NSObject 的 init。
		let this = Self::alloc().set_ivars(FilePromiseDelegateIvars {
			state: Mutex::new(Some(state)),
		});
		// SAFETY: this 是新分配且 ivars 已初始化的实例。
		unsafe { objc2::msg_send![super(this), init] }
	}
}

fn provider_index(provider: &NSFilePromiseProvider) -> u32 {
	provider
		.userInfo()
		.and_then(|info| info.downcast::<NSNumber>().ok())
		.map(|n| n.as_u32())
		.unwrap_or(0)
}

/// Block 本身非 Send，但 AppKit 的 completion handler 允许从任意线程调用。
/// 存裸指针 + ManuallyDrop（不在此处释放，交给系统），显式标记 Send。
struct SendCompletion(usize);
// SAFETY: completion handler 由 AppKit 持有，跨线程调用是 File Promise 的约定用法；
// 我们不 drop 它（ManuallyDrop 语义），生命周期由系统保证。
unsafe impl Send for SendCompletion {}

fn call_completion(handler: &DynBlock<dyn Fn(*mut NSError)>, failed: bool) {
	if failed {
		let error = unsafe {
			NSError::errorWithDomain_code_userInfo(&NSString::from_str("DaisyClipboard"), 1, None)
		};
		let error_ptr: *mut NSError = Retained::as_ptr(&error) as *mut NSError;
		handler.call((error_ptr,));
	} else {
		handler.call((std::ptr::null_mut(),));
	}
}

fn write_file_from_source(
	source: &Arc<dyn ChunkSource>,
	index: u32,
	size: u64,
	path: &str,
) -> Result<u64, String> {
	use std::io::Write;
	let mut file = std::fs::File::create(path).map_err(|e| format!("创建目标文件失败：{e}"))?;
	const CHUNK: u32 = 256 * 1024;
	let mut offset = 0u64;
	while offset < size {
		let len = (CHUNK as u64).min(size - offset) as u32;
		let bytes = source.read(index, offset, len)?;
		if bytes.is_empty() {
			return Err(format!("读取到空块（offset={offset}），提前终止"));
		}
		tracing::debug!(
			"[剪贴板] File Promise 拉块 index={index} offset={offset} 得到 {} 字节",
			bytes.len()
		);
		file.write_all(&bytes)
			.map_err(|e| format!("写文件失败：{e}"))?;
		offset += bytes.len() as u64;
	}
	Ok(offset)
}

thread_local! {
	/// `NSFilePromiseProvider.delegate` 是 **weak** 属性（Apple 文档明示），
	/// 自己不强持有的话函数一返回 delegate 就没了，粘贴时永远等不到回调。
	/// 剪贴板是全局单例，所以这里只需保留「当前这一批」。
	static LIVE_DELEGATES: std::cell::RefCell<
		Vec<Retained<ProtocolObject<dyn NSFilePromiseProviderDelegate>>>,
	> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 把远端文件清单以 File Promise 方式挂上系统剪贴板，返回 changeCount。
/// 必须在主线程调用。
pub fn install_deferred(
	files: Vec<RemoteFileMeta>,
	source: Arc<dyn ChunkSource>,
) -> Result<i64, String> {
	if files.is_empty() {
		return Err("清单为空".to_string());
	}
	let delegate = FilePromiseDelegate::new_with_state(DelegateState {
		source,
		files: files.clone(),
	});
	let protocol: Retained<ProtocolObject<dyn NSFilePromiseProviderDelegate>> =
		ProtocolObject::from_retained(delegate);
	// 强持有到下一批替换为止（见 LIVE_DELEGATES 说明）。
	LIVE_DELEGATES.with(|slot| {
		let mut slot = slot.borrow_mut();
		slot.clear();
		slot.push(Retained::clone(&protocol));
	});

	let mut providers: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> =
		Vec::with_capacity(files.len());
	for i in 0..files.len() {
		let provider = NSFilePromiseProvider::initWithFileType_delegate(
			NSFilePromiseProvider::alloc(),
			&NSString::from_str("public.data"),
			&protocol,
		);
		// SAFETY: userInfo 是我们建的 NSNumber，provider retain 它。
		unsafe { provider.setUserInfo(Some(&NSNumber::new_u32(i as u32))) };
		providers.push(ProtocolObject::from_retained(provider));
	}

	let pasteboard = NSPasteboard::generalPasteboard();
	pasteboard.clearContents();
	let array = objc2_foundation::NSArray::from_retained_slice(&providers);
	if !pasteboard.writeObjects(&array) {
		return Err("writeObjects 失败，Finder 里将无法粘贴".to_string());
	}
	let change = pasteboard.changeCount();
	let listing = files
		.iter()
		.map(|f| format!("{}（{} 字节）", f.name, f.size))
		.collect::<Vec<_>>()
		.join("、");
	tracing::info!(
		"[剪贴板] 已挂 {} 个 File Promise（changeCount={change}）：{listing}，粘贴时才拉字节",
		files.len()
	);
	Ok(change as i64)
}
