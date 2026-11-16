# macOS 开发环境清单

> 记录本机 macOS 开发环境的实际状态与安装方式，供新机器复现与 CI 对齐。
> 记录时间：2026-10-10

---

## 1. 基础环境

| 项 | 值 |
| --- | --- |
| 操作系统 | macOS 26.6.2 |
| 架构 | arm64（Apple Silicon） |
| Homebrew | 6.0.18，前缀 `/opt/homebrew` |
| Xcode Command Line Tools | `/Library/Developer/CommandLineTools` |
| git | 2.54.0 (Apple Git-157) |

---

## 2. Rust 工具链

通过 Homebrew 安装 `rustup`，再固定到精确版本。

| 项 | 值 |
| --- | --- |
| rustup | 1.29.1（`/opt/homebrew/opt/rustup/bin/rustup`） |
| 工具链 | **1.99.0**（固定，见项目根 `rust-toolchain.toml`） |
| rustc | 1.99.0 (b940084d7 2026-09-28) |
| cargo | 1.99.0 (5f94df478 2026-08-27) |
| rustfmt | 1.10.0-stable |
| clippy | 0.1.99 |

**已安装 target**

| target | 用途 |
| --- | --- |
| `aarch64-apple-darwin` | macOS Apple Silicon，本地开发主目标 |
| `x86_64-apple-darwin` | macOS Intel，发布兼容 |
| `x86_64-pc-windows-msvc` | Windows x64。开发期用于 `cargo check --target` 交叉检查；实际链接与打包在 S11 Windows 适配阶段完成 |

> 注意：在 macOS 上无法真正**链接** `x86_64-pc-windows-msvc` 产物（缺 MSVC 链接器）。所以开发期的用法是 `cargo check`，不是 `cargo build`。

---

## 3. 构建依赖

| 工具 | 版本 | 安装方式 | 用途 |
| --- | --- | --- | --- |
| cmake | 4.4.4 | `brew install cmake` | FreeRDP 构建 |
| ninja | 1.13.2 | `brew install ninja` | FreeRDP 构建后端（比 make 快） |
| pkgconf | 3.0.7 | `brew install pkgconf` | 提供 `pkg-config`，FreeRDP 依赖探测 |
| nasm | 3.02 | `brew install nasm` | OpenSSL / FreeRDP 汇编优化 |
| autoconf | 2.73 | `brew install autoconf` | 部分第三方依赖 |
| automake | 1.19 | `brew install automake` | 部分第三方依赖 |
| openssl@3 | 3.6.5 | `brew install openssl@3` | FreeRDP TLS |

---

## 4. 一键复现命令

```bash
brew install cmake ninja pkgconf nasm autoconf automake rustup openssl@3
rustup toolchain install 1.99.0 --component rustfmt clippy rust-src
rustup default 1.99.0
rustup target add --toolchain 1.99.0 \
  aarch64-apple-darwin x86_64-apple-darwin x86_64-pc-windows-msvc
```

---

## 5. 踩坑记录（重要）

### 5.1 rustup 是 keg-only，必须手动加 PATH

Homebrew 的 `rustup` 为 keg-only，不会软链到 `/opt/homebrew/bin`，且不会在 `~/.cargo/bin` 建代理。已在 `~/.zshrc` 写入：

```bash
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
```

不加这行，`cargo` / `rustc` 全部 command not found，`cargo clippy` 也会报 `no such command: clippy`（因为 PATH 里找不到 `cargo-clippy`）。

### 5.2 openssl@3 也是 keg-only，且会被 conda 的 openssl 抢占

`which openssl` 当前指向 **3.5.7**（来自 miniconda），不是 brew 的 3.6.5。FreeRDP 构建时若让 CMake 自动探测，可能链到 conda 的 OpenSSL，导致后续运行时库冲突。

构建 FreeRDP 时必须显式指定：

```bash
export PKG_CONFIG_PATH="/opt/homebrew/opt/openssl@3/lib/pkgconfig:$PKG_CONFIG_PATH"
export CMAKE_PREFIX_PATH="/opt/homebrew/opt/openssl@3:$CMAKE_PREFIX_PATH"
```

### 5.3 沙箱与 Homebrew

本机执行 `brew install` 需绕过命令沙箱（写入 `/opt/homebrew` 属系统路径）。后台任务不会继承该权限，必须前台执行。

### 5.4 直连 GitHub 吞吐极低，第三方源码改用 tarball

实测本机访问 GitHub 的速度：

| 方式 | 吞吐 | 拉 FreeRDP 源码耗时 |
| --- | --- | --- |
| git 协议（shallow clone） | ~9 KB/s | 16 分钟仅 10 MB，**未完成** |
| codeload tarball（curl） | ~145 KB/s | **1 分 16 秒，11 MB** |

**结论：不使用 git submodule 管理第三方 C 依赖**，统一走 `scripts/fetch-freerdp.sh`：codeload tarball + 固定 SHA256 校验 + 解压到 `vendor/freerdp`。`vendor/` 整个目录不入库。

已就位：

| 项 | 值 |
| --- | --- |
| FreeRDP 版本 | 3.32.1 |
| tarball SHA256 | `8803dd26ec9660550252f255cf2d672a785ddd8f544bb475834993e96807c87f` |
| 解压后大小 | 76 MB |
| 缓存位置 | `vendor/.cache/freerdp-3.32.1.tar.gz` |

重新拉取：`scripts/fetch-freerdp.sh --force`（清缓存重下）或直接 `scripts/fetch-freerdp.sh`（命中缓存，秒级完成）。

### 5.5 后台任务一律被沙箱拦截，构建必须前台跑

**不只是 brew。** CMake / Ninja 在生成和构建时会删除自己构建目录内的临时文件，沙箱连工作区内部的 `file-delete` 也一并拒绝，报 `ninja: error: failed recompaction: Operation not permitted`。

结论：**所有涉及编译/构建的命令都要前台执行并申请免沙箱**。后台（`run_in_background`）启动的任务不会继承权限。

### 5.6 Homebrew 的 opus.pc 与 FreeRDP 的 include 风格不兼容

Homebrew 的 `opus.pc` 输出 `Cflags: -I${includedir}/opus`，鼓励写 `#include <opus.h>`；而 FreeRDP 源码写的是 `#include <opus/opus.h>`，需要的是 `${includedir}` 父目录。两者冲突，直接开启 `WITH_OPUS=ON` 会编译失败：

```
libfreerdp/codec/dsp.c:49:10: fatal error: 'opus/opus.h' file not found
```

处理方式：**默认关闭 opus**（`-DWITH_OPUS=OFF`）。Daisy v1 范围不含 RDP 音频，少一个动态库依赖也简化后续打包。需要时用 `WITH_OPUS=ON scripts/build-freerdp.sh`，脚本会自动补 `-I$(brew --prefix opus)/include`。

同类的还有 `libusb`：`channels/urbdrc`（USB 重定向）强依赖 libusb-1.0，用不到，直接 `-DCHANNEL_URBDRC=OFF` 跳过整个目录。

### 5.7 非交互式 shell 不加载 .zshrc，脚本里必须自己导 PATH

`.zshrc` 只在交互式 shell 生效。脚本、CI、自动化工具调用 `cargo` 时会直接报 `command not found: cargo`。

凡是脚本化调用 Rust 工具链的地方，都要显式加：

```bash
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
```

### 5.8 bindgen 需要 libclang，Xcode CLT 已自带

不需要额外 `brew install llvm`。libclang 位于：

```
/Library/Developer/CommandLineTools/usr/lib/libclang.dylib
```

只有在该文件缺失、或 bindgen 报找不到 libclang 时，才需要 `export LIBCLANG_PATH=...` 或安装 llvm。

---

## 6. FreeRDP 构建产物

由 `scripts/build-freerdp.sh` 在 macOS arm64 上构建，**静态库**模式。

| 项 | 值 |
| --- | --- |
| 构建方式 | CMake + Ninja，`BUILD_SHARED_LIBS=OFF` |
| 构建目录 | `vendor/build/freerdp-macos-arm64` |
| 安装目录 | `vendor/dist/freerdp-macos-arm64` |
| 头文件 | 265 个 |
| 静态库 | `libfreerdp3.a` (6.2M)、`libwinpr3.a` (1.9M)、`libfreerdp-client3.a` (1.5M)、`libwinpr-tools3.a` (41K) |

**启用的关键通道**：`cliprdr`（剪贴板）、`drdynvc`、`disp`（显示更新）、`rdpgfx`、`rdpdr`、`rdpsnd`、`rdpei`、`drive`。

**关闭的能力**：X11 / Wayland 客户端、自带客户端可执行文件、服务端、FFmpeg、SWScale、Cairo、CUPS、PCSC、GSSAPI、Opus、URBDRC、Man pages、单元测试。

### 6.1 链接方式

产物自带 pkg-config 文件，链接时用它最省事：

```bash
export PKG_CONFIG_PATH="$PWD/vendor/dist/freerdp-macos-arm64/lib/pkgconfig:$(brew --prefix openssl@3)/lib/pkgconfig:$(brew --prefix)/lib/pkgconfig"
cc $(pkg-config --cflags freerdp3 freerdp-client3) ... $(pkg-config --libs --static freerdp3 freerdp-client3) \
   -framework CoreFoundation -framework Security -framework SystemConfiguration \
   -framework CoreServices -framework IOKit -framework Foundation
```

macOS 系统框架这六个是 winpr 的硬需求，pkg-config 里没有，必须手动加。

### 6.2 遗留的运行时依赖（重要）

链接出的二进制仍然**动态依赖 Homebrew 的 OpenSSL**：

```
/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib
/opt/homebrew/opt/openssl@3/lib/libcrypto.3.dylib
```

S10 打包 .app 时必须把这两个 dylib 一起塞进 `Contents/Frameworks/` 并修正 rpath，否则换台机器就崩。这是 PLAN.md 里 R9 的具体来源。

**该依赖同样适用于 Rust 产物**：`spike/freerdp_rs` 编译出的二进制经 `otool -L` 确认，同样动态依赖这两个 dylib。

### 6.3 Rust 侧链接

`spike/freerdp_rs/build.rs` 已跑通完整链接配置，S8 可直接复用：

1. `rustc-link-search` 指向 `vendor/dist/freerdp-macos-*/lib` 与 `lib/freerdp3`
2. 静态库顺序：`freerdp-client3` → `freerdp3` → `winpr3`（依赖方在前）
3. OpenSSL 通过 `brew --prefix openssl@3` 动态定位，`-lssl -lcrypto -lz`
4. 六个 macOS 系统框架：CoreFoundation / Security / SystemConfiguration / CoreServices / IOKit / Foundation

---

## 7. 尚未安装

### 7.1 macOS 后续阶段需要（brew 安装）

| 工具 | 需要的阶段 | 用途 |
| --- | --- | --- |
| `cargo-bundle` | S10 | macOS 打包 .app / .dmg |

### 7.2 Windows 适配阶段需要（S11，非 brew）

**开发阶段全程只用 macOS，这些现在都不装。** 但 S11 是必经阶段，届时需要：

| 工具 | 用途 |
| --- | --- |
| Visual Studio Build Tools | MSVC 编译器与链接器 |
| CMake（Windows 版） | 构建 FreeRDP |
| vcpkg | 提供 OpenSSL / zlib 静态库 |
| Strawberry Perl | 编译 vendored OpenSSL（SQLCipher 依赖） |
| NASM | OpenSSL 汇编优化 |
| WiX 或 NSIS | 打包 .msi / .exe |

> 清单来源：STEPS.md 的 S11，以及 B3 阻塞项。

### 7.3 开发期的跨平台自检

不需要 Windows 机器就能提前拦截大部分平台相关编译错误：

```bash
cargo check --target x86_64-pc-windows-msvc
```

只做类型检查与编译，不做链接，因此 macOS 上可直接执行。**每个阶段结束时跑一次**（PLAN.md 8.1 的 D1）。Windows 的 std 已在 S0 装好（`rustup target add`），无需额外准备。

**已知例外（实测踩到三条）**：带**原生 C 依赖**的 crate 无法这样交叉检查。

1. 调用 bindgen 的 crate（`daisy-rdp`）——bindgen 必须按**目标平台**解析系统头文件，macOS 上没有 Windows SDK：

```
fatal error: 'stdlib.h' file not found
  --> vendor/dist/freerdp-macos-arm64/include/winpr3/winpr/platform.h:23:10
```

处置：`build.rs` 判断 `CARGO_CFG_TARGET_OS`，非宿主平台时跳过 bindgen 与链接配置，只写占位绑定文件。

2. 带 `cc` 编译的 C 依赖的 crate（`daisy-store`）——`rusqlite(bundled-sqlcipher)` 拉入 `openssl-sys(vendored)`，要用宿主 perl 给 `VC-WIN64A` 配置 OpenSSL：

```
This perl implementation doesn't produce Windows like paths (with backward
slash directory separators).  Please use an implementation that matches your
building platform.
Error configuring OpenSSL build: 'perl' reported failure with exit status: 255
```

这条**无法处置**，只能整个排除出交叉 check。

3. 同样是 `cc` 编译 C（`daisy-ssh`）——`russh` 的 `ring` 后端要为目标平台编 C，找不到 Windows SDK 的头：

```
ring@0.17.14: crypto/curve25519/../internal.h:20:
  include/ring-core/check.h:27:11: fatal error: 'assert.h' file not found
error: failed to run custom build command for `ring v0.17.14`
```

同样**无法处置**，排除出交叉 check。

> **代价要说清楚**：这三类 crate 的交叉 check 或缺失、或只是「Rust 侧能过」，**它们的 Windows 兼容性必须在 S11 的 Windows 机器上真正验证**。纯 Rust 的 crate 没有这个限制。
>
> `scripts/check.sh` 里有一个 `CROSS_CHECK_CRATES` 白名单维护这件事——新增 crate 时，纯 Rust 的加进去，带 C 依赖的不要加。

### 7.4 russh 的 crypto 后端选择

`russh` 必须启用一个后端（`ring` 或 `aws-lc-rs`），否则直接 `compile_error!`。默认是 `aws-lc-rs`。

Daisy 选了 **`ring`**（`default-features = false` + `features = ["ring", "rsa", "flate2"]`）：

- `aws-lc-sys` 的构建要 cmake + 一整套 C 工具链，S11 的 Windows 构建会多一份环境依赖
- `ring` 只需 `cc`，且在 Windows MSVC 目标上有预生成的汇编，不需要 NASM
- `rsa` 必须显式开着，否则 RSA 私钥认证不可用（步骤 42）
- `flate2` 是 zlib 压缩，老服务器常要求

### 7.5 SSH 测试密钥夹具

`crates/daisy-ssh/tests/fixtures/` 下有一次性的**弃用密钥**，用于验证三种私钥格式的解析与端到端认证。它们不保护任何真实资源。

用真实工具链生成（`ssh-keygen` / `openssl genrsa -traditional` / `openssl pkcs8 -topk8`），因为只有真实输出才能覆盖真实编码差异——手写字符串测不出这些。

`.gitignore` 的 `*.pem` / `id_rsa*` 规则对本目录**做了例外放行**（`!crates/daisy-ssh/tests/fixtures/**`），详见该目录的 `README.md`。

