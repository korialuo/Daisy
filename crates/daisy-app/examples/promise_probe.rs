//! 最小复现：只调 macOS File Promise 的 install_deferred，用来定位空指针崩溃。
//!
//! 跑：`cargo run -p daisy-app --example promise_probe`

#[cfg(target_os = "macos")]
#[path = "../src/deferred_clipboard_mac.rs"]
mod deferred_clipboard_mac;

#[cfg(target_os = "macos")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
struct MockSource;

#[cfg(target_os = "macos")]
impl deferred_clipboard_mac::ChunkSource for MockSource {
	fn read(&self, _index: u32, _offset: u64, len: u32) -> Result<Vec<u8>, String> {
		Ok(vec![7u8; len as usize])
	}
}

fn main() {
	#[cfg(target_os = "macos")]
	{
		println!("STEP 1: before install_deferred");
		let files = vec![daisy_rdp::RemoteFileMeta {
			name: "probe.txt".to_string(),
			size: 4096,
		}];
		match deferred_clipboard_mac::install_deferred(files, Arc::new(MockSource)) {
			Ok(change) => println!("STEP 2: install ok change={change}"),
			Err(error) => println!("STEP 2: install err {error}"),
		}
		println!("STEP 3: after install_deferred");
		std::thread::sleep(std::time::Duration::from_millis(500));
		println!("STEP 4: done");
	}
	#[cfg(not(target_os = "macos"))]
	println!("skip: macos only");
}
