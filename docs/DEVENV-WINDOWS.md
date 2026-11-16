# Windows 开发环境（S11 步骤 80）

> 目标：一台能 `cargo build -p daisy-app` 出可运行 exe 的 Windows 机器。
> 最低 **Windows 10 x64**（Rust 的 MSVC 目标本身就以 Win10 为下限）。
> 需要真机验收时对照 `docs/PLATFORM-WINDOWS.md` §5 的清单。

---

## 1. 要装什么，为什么

| 工具 | 用途 | 不装的后果 |
| --- | --- | --- |
| **Visual Studio 2019 或 2022 Build Tools**（MSVC v142/v143 + Windows 10/11 SDK） | 链接器 `link.exe`、资源编译器 `rc.exe`、C 运行库 | 任何 `windows-msvc` 目标都编不出来；exe 也没有图标与版本信息 |
| **Rust（rustup）** | 工具链，版本由 `rust-toolchain.toml` 决定 | — |
| **CMake + Ninja** | 构建 FreeRDP | `scripts/build-freerdp.ps1` 第一步就退出 |
| **vcpkg**（`VCPKG_ROOT`） | OpenSSL、zlib 的**静态**版 | FreeRDP 配置阶段找不到 OpenSSL；或者退化成动态链接，换机器就崩 |
| **Strawberry Perl** | OpenSSL 的 `Configure` 脚本 | vcpkg 编译 OpenSSL 失败（`perl is not recognized`） |
| **NASM** | OpenSSL 的 x86-64 汇编优化 | 同上；`nasm not found` |
| **LLVM**（`LIBCLANG_PATH`） | `bindgen` 解析 FreeRDP 头文件 | `daisy-rdp` 的 build.rs 报「Unable to find libclang」 |

一条命令装齐（`cmd` 或 PowerShell，管理员）：

```powershell
winget install Microsoft.VisualStudio.2022.BuildTools
winget install Rustlang.Rustup
winget install Kitware.CMake Ninja-build.Ninja NASM.NASM StrawberryPerl.StrawberryPerl
winget install LLVM.LLVM
```

> VS Build Tools 安装时要勾 **「使用 C++ 的桌面开发」**，否则没有 MSVC 与 Windows SDK。
> `winget install` 默认静默安装可能不带这个工作负载，装完用 §2 的命令验证。

### 1.1 用 VS2019（v142）也可以

整条链的最低要求只到 VS2019：Rust 的 msvc 目标要 MSVC 2015+ 的链接器，FreeRDP 3.x 只用 C
（`cmake_minimum_required(VERSION 3.13)`，源码里没有 MSVC 版本下限检查），WinPR 的原子操作
走 `Interlocked*`，不依赖 C11 `<stdatomic.h>`。**唯一的额外动作是钉住 vcpkg 的工具集**——
它默认按 v143 找编译器，只有 v142 时会报「could not find v143」。
`build-freerdp.ps1` 已按 `cl.exe` 版本自动判定（19.2x → v142，19.3x → v143），无需手改。

VS2019 安装器里勾这些（工作负载页勾第一项即可，其余是它自带的）：

| 位置 | 勾选项 | 说明 |
| --- | --- | --- |
| 工作负载 | ☑ **使用 C++ 的桌面开发**（Build Tools 版显示为「C++ 生成工具」） | 唯一的必须项，连带装上后两项 |
| 单个组件 | ☑ MSVC v142 - VS 2019 C++ x64/x86 生成工具（最新） | 提供 `cl.exe` / `link.exe` |
| 单个组件 | ☑ Windows 10 SDK (10.0.19041.0) 或更高 | 提供 `rc.exe` 与 `windows-sys` 要的导入库 |
| 单个组件 | ☐ C++ CMake tools for Windows | **不用勾**：我们用单独装的 CMake + Ninja 生成器 |
| 单个组件 | ☐ MSVC v141/v140、ATL、MFC、C++/CLI | 不用 |

装完 `cl` 的版本应是 `19.29.x`（VS2019 16.11）。VS2017（`19.1x`）不行——脚本会直接拦下。

---

## 2. 装完先验证（缺一项就别往下走）

```powershell
# 1) MSVC 链接器（必须在「x64 Native Tools Command Prompt」里，或先跑 vcvars64.bat）
where.exe link.exe
where.exe rc.exe

# 2) Rust 与目标
rustup show
rustc -V                       # 应等于 rust-toolchain.toml 里的 1.99.0
rustup target list --installed # 应含 x86_64-pc-windows-msvc

# 3) 构建工具
cmake --version
ninja --version
nasm -v
perl -v

# 4) vcpkg
echo $env:VCPKG_ROOT           # 应指向 vcpkg 目录
& "$env:VCPKG_ROOT\vcpkg.exe" version

# 5) libclang
Test-Path 'C:\Program Files\LLVM\bin\libclang.dll'
```

**找不到 `link.exe` 的常见原因**：没在 VS 开发者环境里。三种解法——
用「x64 Native Tools Command Prompt」开终端，或在普通 PowerShell 里先执行 vcvars，
或者干脆不管它：`build-freerdp.ps1` 会自己用 `vswhere` 找到 `vcvars64.bat` 并导入环境。

```powershell
# VS2022（Community / BuildTools 按实际目录改）
& "${env:ProgramFiles}\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
# VS2019（注意在 Program Files (x86) 下）
& "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2019\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
```

---

## 3. 装完能做什么（按依赖从少到多）

```powershell
# A. 纯 Rust 的 crate：不需要 FreeRDP，也不需要链接
cargo test -p daisy-core      # 含 DPAPI 的 5 条用例（只在 Windows 上跑）
cargo test -p daisy-net -p daisy-term

# B. 链接 FreeRDP：先把静态库编出来（约 10–20 分钟，首次更久）
pwsh -File scripts\build-freerdp.ps1 -Clean

# C. 出 exe
cargo build -p daisy-app
# D. 打包
pwsh -File scripts\build-windows.ps1
```

---

## 4. 与 macOS 侧的差异（省得来回找）

| 项目 | macOS | Windows |
| --- | --- | --- |
| FreeRDP 构建 | `scripts/build-freerdp.sh`（Homebrew OpenSSL） | `scripts/build-freerdp.ps1`（vcpkg 静态 OpenSSL/zlib） |
| 产物目录 | `vendor/dist/freerdp-macos-<arch>` | `vendor/dist/freerdp-windows-x64` |
| CRT | 系统 libc | **静态 CRT**：FreeRDP 用 `CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded`，Rust 侧靠仓库根的 `.cargo/config.toml` 加 `+crt-static` 对齐（msvc 目标默认是 `/MD`），不一致就 LNK2038 |
| 主密钥 | `EmbeddedKey`（混淆） | `DpapiKey`（真加密，`key.bin`） |
| 本地 shell | `$SHELL -l` | `powershell.exe -NoLogo -NoExit -Command ...` |
| exe/窗口图标 | `daisy.icns` + `with_icon` | `daisy-icon.ico`（资源）+ `with_icon`（窗口） |
| 控制台 | 终端里跑，日志能看见 stderr | GUI 子系统，**stderr 没有去处**，全看 `%APPDATA%\Daisy\logs` |

---

## 5. 已知坑

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| `could not find v143 toolset`（vcpkg 编 OpenSSL 时） | 机器上只有 VS2019 的 v142 | 设 `VCPKG_PLATFORM_TOOLSET=v142`；`build-freerdp.ps1` 已自动判定 |
| `No CMAKE_C_COMPILER could be found` | 不在 VS 开发者环境里（Ninja 生成器不自动探测 MSVC） | 脚本会自动导入 `vcvars64.bat`；仍失败就手动开「x64 Native Tools Command Prompt」 |
| `perl reported failure ... doesn't produce Windows like paths` | 用 macOS 的 perl 给 MSVC 目标配置 OpenSSL | 只在 Windows 上构建；交叉检查走 `scripts/check-windows.sh`，它不编 OpenSSL |
| `LNK2038: mismatch detected for 'RuntimeLibrary'` | FreeRDP 用动态 CRT、Rust 用静态 CRT（或反过来） | 两侧都已钉好：FreeRDP 靠 `-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded`，Rust 靠 `.cargo/config.toml` 的 `+crt-static`。仍报错说明 FreeRDP 的开关没生效——**清构建目录重配**（CMake 会缓存这个变量） |
| `unresolved external symbol __imp_...` | 缺 Win32 系统库 | 把库名加进 `crates/daisy-rdp/build.rs::configure_windows` 的数组 |
| `Unable to find libclang` | 没装 LLVM 或没设 `LIBCLANG_PATH` | `winget install LLVM.LLVM`；`build-windows.ps1` 会自动探测默认安装路径 |
| 双击 exe 弹黑框 | 缺 `windows_subsystem` | 已修：`crates/daisy-app/src/main.rs` 顶部 |
| exe 图标是白纸 | `rc.exe` 没跑成功 | 看构建输出里的 `cargo:warning=Windows 资源编译失败` |
| 终端中文乱码 | ConPTY 按 GBK 转字节 | 已默认设 UTF-8；仍乱就 `$env:DAISY_SHELL='cmd.exe'` 对比 |
