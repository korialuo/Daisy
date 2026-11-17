//! 构建脚本：bindgen 生成 FreeRDP 绑定 + 静态链接。
//!
//! 与 `spike/freerdp_rs/build.rs` 同源，但绑定面扩大——S8 需要完整的
//! `freerdp` / `rdpContext` / `rdpSettings` / `rdpGdi` 结构与设置读写函数。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 在 `vendor/dist` 下找指定平台最新的 FreeRDP 产物目录。
///
/// `kind` 取 `macos` 或 `windows`，对应构建脚本里的前缀
/// （`freerdp-macos-<arch>` / `freerdp-windows-<arch>`）。
fn find_freerdp_prefix(root: &Path, kind: &str) -> PathBuf {
	let dist = root.join("vendor/dist");
	let marker = format!("freerdp-{kind}-");

	let mut candidates: Vec<PathBuf> = fs::read_dir(&dist)
		.unwrap_or_else(|e| panic!("无法读取 {}：{e}", dist.display()))
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|p| {
			p.is_dir()
				&& p.file_name()
					.and_then(|n| n.to_str())
					.is_some_and(|n| n.starts_with(&marker))
		})
		.collect();

	candidates.sort();
	candidates.pop().unwrap_or_else(|| {
		let script = if kind == "windows" {
			"scripts/build-freerdp.ps1（PowerShell，需 VS Build Tools + CMake + vcpkg）"
		} else {
			"scripts/build-freerdp.sh"
		};
		panic!(
			"在 {} 下找不到 {marker}* 产物，请先执行 {script}",
			dist.display()
		)
	})
}

fn brew_prefix(formula: &str) -> Option<PathBuf> {
	let out = Command::new("brew")
		.arg("--prefix")
		.arg(formula)
		.output()
		.ok()?;
	if !out.status.success() {
		return None;
	}
	let path = String::from_utf8(out.stdout).ok()?;
	let path = path.trim();
	if path.is_empty() {
		None
	} else {
		Some(PathBuf::from(path))
	}
}

/// 找宿主目标上一次真实生成的 `bindings.rs`，供交叉 check 复用。
///
/// 只认 `target/{debug,release}/build/daisy-rdp-*/out/bindings.rs` 里
/// 体积像样的文件（占位文件只有一行注释，会被 `> 1024` 挡掉）；
/// 有多个时取最新修改的那个。
fn find_host_bindings(root: &Path) -> Option<PathBuf> {
	let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;

	for profile in ["debug", "release"] {
		let build_dir = root.join("target").join(profile).join("build");
		let Ok(entries) = fs::read_dir(&build_dir) else {
			continue;
		};
		for entry in entries.filter_map(Result::ok) {
			let name = entry.file_name();
			if !name.to_string_lossy().starts_with("daisy-rdp-") {
				continue;
			}
			let candidate = entry.path().join("out").join("bindings.rs");
			let Ok(meta) = fs::metadata(&candidate) else {
				continue;
			};
			if meta.len() <= 1024 {
				continue;
			}
			let Ok(modified) = meta.modified() else {
				continue;
			};
			if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
				newest = Some((modified, candidate));
			}
		}
	}

	newest.map(|(_, path)| path)
}

fn main() {
	let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
	// crates/daisy-rdp -> 向上两级到仓库根
	let root = manifest
		.parent()
		.and_then(Path::parent)
		.expect("工程目录结构异常")
		.to_path_buf();

	let out_dir = PathBuf::from(env::var("OUT_DIR").expect("缺少 OUT_DIR"));
	let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
	// 宿主三元组（如 `x86_64-pc-windows-msvc`）。判「是不是真机构建」必须用它：
	// 只看目标 OS 会把「Windows 上编 Windows」也当成交叉 check——那正是
	// 2026-11-17 在真机上踩到的坑：整片 FFI 类型缺失，222 个编译错误。
	let host = env::var("HOST").unwrap_or_default();

	println!("cargo:rerun-if-changed=wrapper.h");
	println!("cargo:rerun-if-changed=build.rs");

	let native = match target_os.as_str() {
		"windows" => host.contains("windows"),
		"macos" => host.contains("apple-darwin"),
		_ => false,
	};

	// 交叉 check：目标与宿主不同平台时跳过 bindgen 与链接。
	// bindgen 需按目标平台解析系统头，macOS 上没有 Windows SDK。
	if !native {
		// 但**不能只写个空占位**——那样 daisy-rdp 与依赖它的 daisy-app
		// 整片都编译不过，交叉 check 就永远看不到应用层的平台错误。
		// 这里复用宿主上一次真实生成（也就是 macOS 那份）的绑定做类型检查。
		//
		// 诚实的边界：FreeRDP / WinPR 自己的结构定义是平台无关的，
		// 所以拿来查 `cfg(windows)` 分支的类型错误是有效的；但 **Windows 上
		// 的真实函数签名（如 `WaitForMultipleObjects` 的实现差异）以真机构建为准**。
		match find_host_bindings(&root) {
			Some(source) => {
				fs::copy(&source, out_dir.join("bindings.rs")).expect("复制宿主绑定失败");
				println!(
					"cargo:warning=交叉 check：复用宿主绑定 {} 做类型检查；\
					 Windows 真实签名与链接以 Windows 机器上的构建为准",
					source.display()
				);
			},
			None => {
				println!(
					"cargo:warning=目标平台 {target_os} 与宿主不同，且找不到宿主已生成的\
					 绑定，只能出占位文件——先在宿主目标上 `cargo check -p daisy-rdp` 一次"
				);
				fs::write(out_dir.join("bindings.rs"), "// 交叉 check：未生成绑定\n")
					.expect("写占位 bindings.rs 失败");
			},
		}
		return;
	}

	// 真机构建：Windows 宿主 + Windows 目标走 Windows 分支；
	// 其余（开发机 macOS）走 macOS 分支。
	if target_os == "windows" {
		configure_windows(&manifest, &root, &out_dir);
	} else {
		configure_macos(&manifest, &root, &out_dir);
	}
}

/// Windows 上 libclang 需要的标准头目录。
///
/// 优先用 `INCLUDE`（VS 开发者环境已经设好）；没有就自己找：
/// vswhere → `VC/Tools/MSVC/<最新>/include`，以及 Windows Kits 的
/// `Include/<最新>/{ucrt,shared,um,winrt}`。
fn windows_include_dirs() -> Vec<PathBuf> {
	if let Ok(include) = env::var("INCLUDE") {
		let dirs: Vec<PathBuf> = include
			.split(';')
			.filter(|part| !part.is_empty())
			.map(PathBuf::from)
			.filter(|path| path.is_dir())
			.collect();
		if !dirs.is_empty() {
			return dirs;
		}
	}

	let mut dirs = Vec::new();
	if let Some(msvc) = vswhere_install_path().and_then(|vs| newest_dir(&vs.join("VC/Tools/MSVC")))
	{
		dirs.push(msvc.join("include"));
	}
	if let Some(kits) = env::var("ProgramFiles(x86)")
		.ok()
		.and_then(|pf86| newest_dir(&PathBuf::from(pf86).join("Windows Kits/10/Include")))
	{
		for sub in ["ucrt", "shared", "um", "winrt"] {
			let dir = kits.join(sub);
			if dir.is_dir() {
				dirs.push(dir);
			}
		}
	}
	if dirs.is_empty() {
		println!(
			"cargo:warning=没能定位 MSVC / Windows SDK 的头文件目录，\
			 bindgen 可能找不到 stdlib.h；在「x64 Native Tools」提示符里构建最稳"
		);
	}
	dirs
}

/// 用 vswhere 找带 C++ 工具集的 VS 安装根（与 build-freerdp.ps1 同一判据）。
fn vswhere_install_path() -> Option<PathBuf> {
	let pf86 = env::var("ProgramFiles(x86)").ok()?;
	let vswhere = PathBuf::from(pf86).join("Microsoft Visual Studio/Installer/vswhere.exe");
	if !vswhere.is_file() {
		return None;
	}
	let out = Command::new(vswhere)
		.args([
			"-latest",
			"-products",
			"*",
			"-requires",
			"Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
			"-property",
			"installationPath",
		])
		.output()
		.ok()?;
	let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
	(!path.is_empty()).then(|| PathBuf::from(path))
}

/// 目录下名字最大的子目录（`14.44.35207`、`10.0.28000.0` 这类版本目录）。
fn newest_dir(parent: &Path) -> Option<PathBuf> {
	let mut found: Vec<PathBuf> = fs::read_dir(parent)
		.ok()?
		.filter_map(Result::ok)
		.map(|entry| entry.path())
		.filter(|path| path.is_dir())
		.collect();
	found.sort();
	found.pop()
}

/// 用 bindgen 生成 FreeRDP 绑定。
///
/// 两个平台共用同一张 allowlist——它描述的是「Daisy 要用哪些 API」，
/// 与平台无关；差别只在 include 路径与 clang 的目标。
fn generate_bindings(manifest: &Path, prefix: &Path, out_dir: &Path) {
	let inc_freerdp = prefix.join("include/freerdp3");
	let inc_winpr = prefix.join("include/winpr3");

	let mut builder = bindgen::Builder::default()
		.header(manifest.join("wrapper.h").display().to_string())
		.clang_arg(format!("-I{}", inc_freerdp.display()))
		.clang_arg(format!("-I{}", inc_winpr.display()))
		.clang_arg("-I/usr/include");

	// Windows：libclang **不会**自己去翻 MSVC 与 UCRT 的头文件，连 `stdlib.h`
	// 都会 `file not found`。在「x64 Native Tools」提示符里跑时 INCLUDE 已经齐备，
	// 直接用；普通命令行里就自己去 vswhere 与 Windows Kits 找一遍。
	if cfg!(windows) {
		for dir in windows_include_dirs() {
			builder = builder.clang_arg(format!("-I{}", dir.display()));
		}
	}

	let bindings = builder
		// 顶层实例与上下文
		.allowlist_type("freerdp")
		.allowlist_type("rdpContext")
		.allowlist_type("rdpSettings")
		.allowlist_type("rdpGdi")
		.allowlist_type("rdpUpdate")
		.allowlist_type("gdiBitmap")
		.allowlist_type("rdpHdc")
		.allowlist_type("rdpHwnd")
		.allowlist_type("rdpClientContext")
		// 输入注入：`context->input` 指向它，键鼠事件都从这儿发
		.allowlist_type("rdpInput")
		// 剪贴板通道（S8-63）。`context->channels` 指向不透明的 rdpChannels，
		// 只需要拿到指针传给通道加载函数，不需要它的定义。
		.allowlist_type("rdpChannels")
		.allowlist_type("CliprdrClientContext")
		.allowlist_type("CLIPRDR_FORMAT_LIST")
		.allowlist_type("CLIPRDR_FORMAT_DATA_RESPONSE")
		.allowlist_type("CLIPRDR_FORMAT_NAME")
		.allowlist_type("CLIPRDR_MONITOR_READY")
		.allowlist_type("CLIPRDR_CAPABILITIES")
		// `VirtualChannelEntryEx` 的函数指针类型：通道入口签名
		.allowlist_type("PVIRTUALCHANNELENTRYEX")
		.allowlist_type("CHANNEL_ENTRY_POINTS_FREERDP_EX")
		.allowlist_type("PCHANNEL_ENTRY_POINTS_EX")
		// WinPR 基础类型（被上面的结构引用）
		.allowlist_type("HANDLE")
		.allowlist_type("DWORD")
		.allowlist_type("BOOL")
		.allowlist_type("BYTE")
		.allowlist_type("UINT32")
		.allowlist_type("UINT16")
		.allowlist_type("INT32")
		// 实例生命周期
		.allowlist_function("freerdp_new")
		.allowlist_function("freerdp_free")
		.allowlist_function("freerdp_context_new")
		.allowlist_function("freerdp_context_free")
		.allowlist_function("freerdp_connect")
		.allowlist_function("freerdp_disconnect")
		.allowlist_function("freerdp_shall_disconnect_context")
		.allowlist_function("freerdp_get_event_handles")
		.allowlist_function("freerdp_check_event_handles")
		.allowlist_function("freerdp_get_last_error")
		.allowlist_function("freerdp_get_last_error_string")
		// 设置读写
		.allowlist_function("freerdp_settings_set_string")
		.allowlist_function("freerdp_settings_set_string_len")
		.allowlist_function("freerdp_settings_set_uint32")
		.allowlist_function("freerdp_settings_set_bool")
		.allowlist_function("freerdp_settings_get_uint32")
		// 输入注入
		.allowlist_function("freerdp_input_send_keyboard_event_ex")
		.allowlist_function("freerdp_input_send_mouse_event")
		// 剪贴板通道（S8-63）：按名加载 + 直连接口两条路都留
		.allowlist_function("freerdp_channels_client_load_ex")
		.allowlist_function("freerdp_channels_client_load")
		.allowlist_function("freerdp_channels_load_plugin")
		.allowlist_function("freerdp_register_addin_provider")
		.allowlist_function("freerdp_channels_get_id_by_name")
		.allowlist_function("freerdp_channels_get_static_channel_interface")
		// 入口在 .c 里 #define 改名，公开头没有声明，由 wrapper.h 补声明
		.allowlist_function("cliprdr_VirtualChannelEntryEx")
		// S8-64 动态分辨率：disp 是 DVC 插件，得先装 drdynvc 并让 addin
		// provider 能按名字找到它（库模式不会自动注册 provider）。
		.allowlist_type("DispClientContext")
		.allowlist_type("DrdynvcClientContext")
		.allowlist_type("DISPLAY_CONTROL_MONITOR_LAYOUT")
		.allowlist_type("ChannelConnectedEventArgs")
		.allowlist_type("FREERDP_LOAD_CHANNEL_ADDIN_ENTRY_FN")
		.allowlist_function("drdynvc_VirtualChannelEntryEx")
		.allowlist_function("disp_DVCPluginEntry")
		// 不 allowlist `freerdp_channels_load_static_addin_entry`：它引用的
		// `CLIENT_STATIC_ADDIN_TABLE` 会把每个通道的 .o 都拖进链接，
		// 其中 remdesk 依赖几个没被引用的辅助符号，直接链接失败。
		// 我们自己实现 provider（display.rs），只认 disp 一支。
		.allowlist_function("freerdp_client_add_dynamic_channel")
		// GDI
		.allowlist_function("gdi_init")
		.allowlist_function("gdi_init_ex")
		.allowlist_function("gdi_free")
		.allowlist_function("gdi_resize")
		.allowlist_function("gdi_resize_ex")
		// 线程等待
		.allowlist_function("WaitForMultipleObjects")
		// S8-63 排查：打开 FreeRDP 自己的 cliprdr 协议日志。
		// Daisy 的回调日志只能证明「回调有没有被调用」，
		// 而回调不触发时分不清「服务端没发」与「发了被 FreeRDP 丢弃」。
		// 只有这个 tag 的 DEBUG 会逐条打出报文。
		.allowlist_type("wLog")
		.allowlist_function("WLog_Get")
		.allowlist_function("WLog_SetStringLogLevel")
		.allowlist_function("WLog_SetLogLevel")
		// 不要给枚举/常量生成 layout test，减少编译面
		.layout_tests(false)
		// FreeRDP 头里有大量位域与匿名结构，按 Rust 1.x 的 repr 生成
		.rustified_enum(".*")
		.generate()
		.expect("bindgen 生成绑定失败");

	bindings
		.write_to_file(out_dir.join("bindings.rs"))
		.expect("写入 bindings.rs 失败");
}

/// macOS 构建：bindgen + 静态链接 FreeRDP 与它依赖的系统框架。
fn configure_macos(manifest: &Path, root: &Path, out_dir: &Path) {
	let prefix = find_freerdp_prefix(root, "macos");
	let libdir = prefix.join("lib");
	println!("cargo:prefix={}", prefix.display());

	generate_bindings(manifest, &prefix, out_dir);

	// ---------- 链接 ----------
	// 库模式不链 client common（S1 坑1：`freerdp_client_context_new` 需要
	// `rdpClientContext` 的 sizeof，而它在私有头里，树外代码拿不到）。
	// 但 S8-63 的剪贴板通道入口 `cliprdr_VirtualChannelEntryEx` 与静态插件表
	// 都在 libfreerdp-client3.a 里，所以**要把它加上**——只加这一个库，
	// 不走 client common 那套高层次 API。
	println!("cargo:rustc-link-search=native={}", libdir.display());
	println!(
		"cargo:rustc-link-search=native={}",
		prefix.join("lib/freerdp3").display()
	);
	println!("cargo:rustc-link-lib=static=freerdp3");
	println!("cargo:rustc-link-lib=static=winpr3");
	println!("cargo:rustc-link-lib=static=freerdp-client3");

	// OpenSSL：FreeRDP 静态库自身无法解析
	if let Some(ossl) = brew_prefix("openssl@3") {
		println!(
			"cargo:rustc-link-search=native={}",
			ossl.join("lib").display()
		);
	}
	println!("cargo:rustc-link-lib=dylib=ssl");
	println!("cargo:rustc-link-lib=dylib=crypto");
	println!("cargo:rustc-link-lib=dylib=z");

	// winpr 硬依赖的 macOS 系统框架
	for framework in [
		"CoreFoundation",
		"Security",
		"SystemConfiguration",
		"CoreServices",
		"IOKit",
		"Foundation",
	] {
		println!("cargo:rustc-link-lib=framework={framework}");
	}
}

/// Windows 构建：bindgen + 静态链接 FreeRDP 与 Win32 系统库。
///
/// 前提是 `vendor/dist/freerdp-windows-*` 已经由 `scripts/build-freerdp.ps1`
/// 产出（MSVC 静态库、静态 CRT）。
///
/// # 第一次在真机上构建大概率要补库
///
/// 静态链接的符号缺口只有链接器能告诉我们，这里给的是按 FreeRDP 源码里
/// 用到的 Win32 API 归纳出来的第一版清单。真机首次链接报
/// `unresolved external symbol X` 时，把 X 所属的库加进下面的数组即可
/// ——不要去改业务代码，缺的是链接参数不是 Rust 代码。
fn configure_windows(manifest: &Path, root: &Path, out_dir: &Path) {
	let prefix = find_freerdp_prefix(root, "windows");
	let libdir = prefix.join("lib");
	println!("cargo:prefix={}", prefix.display());
	// 库换了就重新链接
	println!("cargo:rerun-if-changed={}", libdir.display());

	generate_bindings(manifest, &prefix, out_dir);

	println!("cargo:rustc-link-search=native={}", libdir.display());
	// `-bundle`：**不要**把静态库塞进 rlib，让 rustc 在最终链接时把
	// `freerdp3.lib` 原样交给 link.exe。
	//
	// 踩过的坑（2026-11-17 真机）：默认的 `+bundle` 会把这几个库当「行李」
	// 打包进 rlib，而 FreeRDP 的 Windows 构建开着 IPO（归档里全是 /GL 模块，
	// 92 MB）。打进 rlib 后再取出，link.exe 就**完全看不到这些符号**——
	// 表现为 29 个 LNK2019（`freerdp_new`、`cliprdr_VirtualChannelEntryEx`…），
	// 而 `-L` 搜索路径却正常传到了链接行（只有 `-l` 消失），很容易误判成
	// 「库没找到」。同一个 .lib 用最小 C 程序链接是好的，所以问题只在打包这一步。
	for lib in ["freerdp3", "winpr3", "freerdp-client3"] {
		println!("cargo:rustc-link-lib=static:-bundle={lib}");
	}

	// `remdesk-common`：静态 addin 表（`CLIENT_STATIC_ADDIN_TABLE`）会把 remdesk
	// 通道的入口一起拖进链接，而它的辅助函数（`remdesk_write_ctl_header` 等）
	// 在**另一个**静态库里，FreeRDP 只把它单独安装、不并进 `freerdp-client3`。
	// 不显式链上就是 3 个 LNK2001。
	//
	// 安装位置两个平台不一样，文件名也跟着平台变（Windows `remdesk-common.lib`
	// / macOS `libremdesk-common.a`）：macOS 在 `lib/freerdp3/`（macOS 分支的
	// 搜索路径里本来就有这一条，所以那边一直没暴露），Windows 直接躺在 prefix
	// 根目录。这里把两种可能都试一遍，谁在就用谁。
	for dir in [
		prefix.clone(),
		prefix.join("lib"),
		prefix.join("lib/freerdp3"),
	] {
		let found =
			dir.join("remdesk-common.lib").exists() || dir.join("libremdesk-common.a").exists();
		if found {
			println!("cargo:rustc-link-search=native={}", dir.display());
			println!("cargo:rustc-link-lib=static:-bundle=remdesk-common");
			break;
		}
	}

	// FreeRDP 静态库自身的外部依赖：OpenSSL 与 zlib。
	// Windows 上这两样由 vcpkg 提供静态版（scripts/build-freerdp.ps1 会装）。
	// `VCPKG_ROOT` 是 vcpkg 的标准安装位置变量；没设就提示，不猜路径。
	match env::var("VCPKG_ROOT") {
		Ok(root) => {
			let vcpkg_lib = PathBuf::from(root).join("installed/x64-windows-static/lib");
			println!("cargo:rustc-link-search=native={}", vcpkg_lib.display());
		},
		Err(_) => println!(
			"cargo:warning=未设置 VCPKG_ROOT，OpenSSL / zlib 的库目录找不到；\
			 若链接报 ssl / crypto / zlib 相关符号缺失，先设这个环境变量"
		),
	}
	// 同样 `-bundle`：理由见上面 freerdp3 那段。这三份也在 vcpkg 的静态目录里，
	// 打包进 rlib 一样会丢符号。
	println!("cargo:rustc-link-lib=static:-bundle=libssl");
	println!("cargo:rustc-link-lib=static:-bundle=libcrypto");
	// 注意库名：vcpkg 的 zlib 1.4 起把静态库改名成 `libzs.lib`（旧的 vcpkg
	// 与自编 zlib 是 `zlib.lib`），照旧写死 `zlib` 会在真机上得到
	// `LNK1104: cannot open file 'zlib.lib'`。按目录里实际存在的挑一个。
	let zlib_name = ["zlib", "libzs", "zs"]
		.iter()
		.find(|name| {
			env::var("VCPKG_ROOT").is_ok_and(|root| {
				PathBuf::from(root)
					.join("installed/x64-windows-static/lib")
					.join(format!("{name}.lib"))
					.exists()
			})
		})
		.copied()
		.unwrap_or("zlib");
	println!("cargo:rustc-link-lib=static:-bundle={zlib_name}");

	// Win32 系统库。按用途分组，缺哪个补哪个：
	//   网络：ws2_32 dnsapi iphlpapi
	//   加密/凭据：crypt32 bcrypt ncrypt secur32 credui wintrust
	//   图形/显示：user32 gdi32 d3d11 dxgi shcore
	//   杂项：advapi32 ole32 oleaut32 uuid comdlg32 shell32 version msimg32 winmm
	for lib in [
		"ws2_32", "dnsapi", "iphlpapi", "crypt32", "bcrypt", "ncrypt", "secur32", "credui",
		"wintrust", "user32", "gdi32", "d3d11", "dxgi", "shcore", "advapi32", "ole32", "oleaut32",
		"uuid", "comdlg32", "shell32", "version", "msimg32", "winmm",
	] {
		println!("cargo:rustc-link-lib=dylib={lib}");
	}
}
