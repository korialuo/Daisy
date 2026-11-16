//! 构建脚本：把 Windows 的 exe 资源（图标 + 版本信息）编进可执行文件。
//!
//! 只有 **Windows 宿主 + Windows 目标** 才真的做这件事：
//!
//! - 交叉 check（macOS 上 `--target x86_64-pc-windows-msvc`）连链接都不做，
//!   更不能指望有 `rc.exe`，所以整段用 `#[cfg(windows)]` 排除；
//! - 在 Windows 上构建时，MSVC 工具链里本来就有 `rc.exe`（与 `link.exe` 同目录），
//!   由 `winresource` 自己找。
//!
//! 没有这个资源，exe 在资源管理器与任务栏里是一张白纸默认图标——
//! egui 的 `with_icon` 只管**窗口**图标，管不到**文件**图标，两者是两套东西。

fn main() {
	println!("cargo:rerun-if-changed=../../assets/icons/daisy-icon.ico");
	println!("cargo:rerun-if-changed=build.rs");

	// 用 cfg 而不是运行时判断：`winresource` 只在 Windows 宿主的构建依赖里，
	// 写成 `if !cfg!(windows) { return; }` 仍然会在编译期解析这个路径然后报错。
	#[cfg(windows)]
	compile_resource();
}

/// 编译 `.ico` 与版本信息进可执行文件。
#[cfg(windows)]
fn compile_resource() {
	let mut resource = winresource::WindowsResource::new();
	resource.set_icon("../../assets/icons/daisy-icon.ico");
	// 版本号由 winresource 从 Cargo 环境读取（CARGO_PKG_VERSION），
	// 不在 build.rs 里二次维护，避免与 Cargo.toml 漂移。
	resource.set("FileDescription", "Daisy 远程连接客户端");
	resource.set("ProductName", "Daisy");
	resource.set("OriginalFilename", "daisy.exe");

	if let Err(error) = resource.compile() {
		// 图标缺失不该让整个构建在最后一步失败：说清原因，让构建继续，
		// 产物只是少了文件图标与版本信息。
		println!("cargo:warning=Windows 资源编译失败（exe 图标与版本信息将缺失）：{error}");
	}
}
