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

### 1.2 Win10 LTSC / 没有 winget 的机器

LTSC 与企业版镜像默认不带 Microsoft Store，**winget 缺失是正常的**，不是装坏了。
补装 winget 要 App Installer 的 msixbundle 加一串依赖，在 LTSC 上经常半死不活；
这类机器直接用 **Chocolatey**：

```powershell
# 管理员 PowerShell
Set-ExecutionPolicy Bypass -Scope Process -Force
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
iex ((New-Object Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
```

装完重开终端，`scripts\install-deps-windows.ps1` 会自动检测到 choco 并走它。
不想用脚本时，一条命令装齐（包名与 winget 不同，别混）：

```powershell
choco install -y --no-progress cmake ninja llvm nasm strawberryperl rustup.install powershell-core git
```

> **LTSC 2019（build 17763）**：VS2022 装不上（要求 1909+），只能用 VS2019；
> 而且要在 VS Installer 的「单个组件」里勾 **Windows 10 SDK 10.0.19041 或更高**，
> 自带的 17763 SDK 太老，`windows-sys` 的导入库会缺。

### 1.3 实测记录：korialuo-dev（2026-11-17，Win10 IoT LTSC 2021 / 19044）

这一轮是**远程经 RDP 无人值守装完的**，踩到的坑都记在这里，换机器照着走能省一天。

**先说结论**：这台机器上最后能用的组合是
`VS2022 Build Tools @ C:\BT2022（v143 / MSVC 14.44）` + `VS2022 自带的 cmake+ninja` +
`C:\dev\perl`（portable）+ `C:\dev\nasm` + `C:\LLVM` + `C:\dev\vcpkg`（openssl 3.6.5、zlib 1.4 静态）。
`scripts/build-freerdp.ps1 -ConfigureOnly` 通过（75 s），完整编译随后通过。

| 现象 | 真实原因 | 处理 |
| --- | --- | --- |
| 以为装了 VS2019，`C:\Program Files (x86)\Microsoft Visual Studio` 下却只有 `Installer`/`Shared` | 实际装的是 **VS2026 Community 18.12**，在 `C:\Program Files\Microsoft Visual Studio\18\Community`，而且**没有 C++ 工作负载**（没有 `VC` 目录） | 别修它：另装 VS2022 Build Tools 到 `C:\BT2022`。`vswhere -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64` 会把没有 C++ 的实例过滤掉 |
| VS 安装器 `setup.exe modify` 立刻 `exit=87` | 会话没提权（UAC 开着，`admin=False`） | 全部安装脚本走 `Start-Process -Verb RunAs`，UAC 弹窗用键盘「←、Enter」确认 |
| choco 引导脚本"成功"但 `choco.exe` 不存在、`choco install` 静默 0 | 同一个原因：没提权 | 同上；LTSC 上干脆不用 choco，直接下官方安装包 |
| `Invoke-WebRequest` / `WebClient.DownloadFile` 对 GitHub 全失败 | 这台机到 GitHub 的 TLS 直接断（`curl error 35 SSL connect error`） | 用系统自带 `curl.exe` + `ghfast.top` 代理；**不要**把函数起名叫 `Curl`（PowerShell 内置别名 `curl` 优先于函数，调用会静默走错） |
| 下载某个 release 资产时 0 字节挂住 | `github.com/<repo>/archive/refs/tags/<tag>.tar.gz` 这种形态代理不转发 | 改用 `archive/<tag>.tar.gz`（实测 800 KB/s） |
| vcpkg 卡在 `Downloading ... -> xxx.part`（0 字节） | vcpkg 自己的 curl 同样 SSL 35 | 手动下好放进 `C:\dev\vcpkg\downloads\`，**文件名必须带 vcpkg 的短哈希**（如 `openssl-openssl-openssl-3-40f0b346.6.5.tar.gz`、`strawberry-perl-5-e37c541b.42.2.1-64bit-portable.zip`）；名字不对它就当没有，哈希不对它会打印 `Expected/Actual` |
| 代理下回来的 FreeRDP/openssl **tar.gz 哈希对不上** | codeload 是按请求生成的 tar.gz，代理可能重新打包 | release **资产**（zip/exe）字节一致，可以放；archive tar.gz 只能挑镜像碰运气（`hub.gitmirror.com` / `gh-proxy.com` 都试） |
| rustup 34 KB/s / `rust-1.99.0-x86_64-pc-windows-msvc` 404 | 直连 `static.rust-lang.org` 慢；TUNA 只镜像当前 stable，没有 1.99.0 | `RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static`（**USTC / SJTUG 有 1.99.0**，TUNA、北外、阿里云都没有） |
| `cargo build` 报 `the server unexpectedly provided an obsolete version of the distribution manifest` | `rust-toolchain.toml` 里列了 `aarch64-apple-darwin`、`x86_64-apple-darwin` 两个 mac 目标，Windows 上 rustup 要去下载它们 | 在仓库根 `rustup override set 1.99.0`（目录级 override 优先于 toolchain 文件），或把这两个目标装上 |
| `daisy-rdp` 报 222 个 `cannot find ... in ffi` | `build.rs` 的判据写成 `if target_os != "macos"` 就走去「交叉 check」分支（复用宿主绑定/写占位），**Windows 原生构建永远进不去** | 改按 **宿主** 判断：`windows` 目标只有在宿主也是 windows 时才走真机分支（已修） |
| bindgen: `fatal error: 'stdlib.h' file not found` | libclang 不会自己找 MSVC / UCRT 的头 | `build.rs` 现在优先读 `INCLUDE`，没有就用 vswhere + Windows Kits 自己定位（已修） |
| 链接期 `cannot open file 'zlib.lib'` | vcpkg 的 zlib 1.4 起静态库改名 `libzs.lib` | `build.rs` 按目录里实际存在的名字挑（`zlib`/`libzs`/`zs`，已修） |
| 新开的 cmd 里 `LIBCLANG_PATH` / PATH 不生效 | Explorer 还没收到环境变量变更通知 | 重启或重新登录；临时用 `set LIBCLANG_PATH=C:\LLVM\bin` 顶一下 |
| `vcpkg install` 报 `Could not locate a manifest (vcpkg.json)` / `does not have a classic mode instance` | 2025 之后的 vcpkg-tool 在没有清单的目录里默认清单模式 | 加 `--classic` |
| 同一句 vcpkg 命令手工能跑，脚本里必失败 | `vcvars64.bat` 把 `VCPKG_ROOT` 改成了 `<VS>\VC\vcpkg`（VS 自带的**只有清单模式**的实例） | 导入 vcvars 之前先把 `VCPKG_ROOT` 存进变量，导入之后写回（`build-freerdp.ps1` 已修） |
| 脚本"什么都没干就退出" | `$ErrorActionPreference='Stop'` + PS 5.1 把原生程序 stderr 包成 ErrorRecord：`& where.exe cl.exe 2>$null` 找不到就终结整个脚本 | 走 `& cmd /c 'where cl.exe 2>nul'`（`build-freerdp.ps1` 已修） |
| cmake 警告 `Ignoring extra path from command line: ...vcpkg.cmake`，随后 `Could NOT find ZLIB` | 数组里 `'前缀' + (Join-Path ...)` 被拆成两个 argv，工具链没生效 | 用 `('-DCMAKE_TOOLCHAIN_FILE={0}' -f $toolchainFile)` 拼成单个字符串（已修） |

**远端无人值守怎么操作**：AppleScript 点不了窗口、合成 HID 也不被允许，但这台机器
可以通过 **Daisy 自己的 RDP 会话**驱动——
`cargo run -p daisy-app --example rdp_drive -- <主机名> '<步骤>'`
支持 `wait:` / `key:win+r` / `type:` / `enter` / `shot:`（存 BMP）/ `sendclip:<文件>` / `paste` / `readclip`。
典型用法：把长脚本用 `sendclip` 送进远端剪贴板，在 cmd 里
`powershell -Command "Get-Clipboard -Raw | Set-Content C:\dev\setup.ps1 -Encoding UTF8"` 落盘，
再 `Start-Process -Verb RunAs` 提权执行，日志写文件、用 `type ... | clip` 读回（先 `chcp 65001`）。
> `readclip` 之前必须先 `arm`：rdpclip 在客户端上报过格式列表之前不监听会话剪贴板。

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
| 链接报 29 个 `LNK2019`（`freerdp_new`、`cliprdr_VirtualChannelEntryEx`…），而 `-L` 搜索路径明明在链接行上 | 静态库被 rustc **bundle 进 rlib**（默认 `+bundle`）。FreeRDP 的 Windows 构建开着 IPO，`freerdp3.lib` 是 92 MB 的 `/GL` 归档，打包再取出后 link.exe 看不到符号；`-l` 不会出现在链接行，只有 `-L` 在——极易误判成「库没找到」 | 改 `cargo:rustc-link-lib=static:-bundle=<名>`（已修）。自查方法：`cargo build -v` 的链接行里搜库名，0 次命中就是被 bundle 了 |
| 剩 3 个 `LNK2001: remdesk_write_ctl_header` / `remdesk_prepare_ctl_header` / `remdesk_read_channel_header` | 静态 addin 表把 remdesk 通道入口拖进了链接，而这三个辅助函数在**单独安装**的 `remdesk-common.lib` 里，FreeRDP 不把它并进 `freerdp-client3` | 显式链 `remdesk-common`（已修）。注意安装位置：Windows 在 prefix **根目录**，macOS 在 `lib/freerdp3/`——macOS 分支的搜索路径本来就有后者，所以这个坑只在 Windows 上现形 |
| `Unable to find libclang` | 没装 LLVM 或没设 `LIBCLANG_PATH` | `winget install LLVM.LLVM`；`build-windows.ps1` 会自动探测默认安装路径 |
| 双击 exe 弹黑框 | 缺 `windows_subsystem` | 已修：`crates/daisy-app/src/main.rs` 顶部 |
| exe 图标是白纸 | `rc.exe` 没跑成功 | 看构建输出里的 `cargo:warning=Windows 资源编译失败` |
| 终端中文乱码 | ConPTY 按 GBK 转字节 | 已默认设 UTF-8；仍乱就 `$env:DAISY_SHELL='cmd.exe'` 对比 |
