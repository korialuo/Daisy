#!/usr/bin/env bash
#
# Windows 目标交叉检查（只编译、不链接），在 macOS 上跑。
#
# 为什么要有这个脚本：`cargo check --target x86_64-pc-windows-msvc` 在
# macOS 上**原本是跑不起来的**——三个带原生 C 代码的依赖会在构建脚本阶段
# 直接失败，于是应用层的平台错误一个都露不出来（PLAN.md 8.1 的 D1b 曾据此
# 判定「这几个 crate 只能等真机」）。本脚本把这三处绕过去：
#
#   1. `ring`（russh 的加密后端）—— 用 scripts/cc-stub.sh 冒充编译器，
#      建出空 .o / .a 让构建脚本走完。
#   2. `openssl-sys`（SQLCipher 的加密后端）—— 同样用桩编译器，但它的
#      版本探测需要**真的预处理**，桩会把 `-E` 转交宿主 clang；
#      再给它一个只有头文件与空库的假安装目录（见 prepare_openssl_shim）。
#   3. `daisy-rdp` 的 bindgen —— 由 crates/daisy-rdp/build.rs 自己处理：
#      交叉时复用宿主上一次真实生成的绑定做类型检查。
#
# **产物不可运行**：空 .a 链接不出二进制。真正的 Windows 构建在 Windows
# 机器上做（S11 步骤 81–82，scripts/build-windows.ps1）。
#
# 用法：
#   scripts/check-windows.sh            工作区 + 全部 target（含测试、示例）
#   scripts/check-windows.sh --clippy   额外跑 clippy（-D warnings）
#
set -euo pipefail

export PATH="/opt/homebrew/opt/rustup/bin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="x86_64-pc-windows-msvc"
# 环境变量名是「目标三元组大写 + 下划线」前缀，cargo 交叉编译时依赖这个形式
TARGET_PREFIX="X86_64_PC_WINDOWS_MSVC"
STUB="$ROOT/scripts/cc-stub.sh"
SHIM="$ROOT/target/windows-check/openssl"

RUN_CLIPPY=0
for arg in "$@"; do
	case "$arg" in
		--clippy) RUN_CLIPPY=1 ;;
		-h | --help)
			sed -n '2,30p' "$0"
			exit 0
			;;
		*)
			echo "未知参数：$arg" >&2
			exit 2
			;;
	esac
done

chmod +x "$STUB"

# ---------------------------------------------------------------
# 假 OpenSSL：只要头文件和空库
#
# openssl-sys 交叉编译时会按目标平台找 `lib` 下的 `*.lib`，宿主 Homebrew
# 的 libssl.dylib 对它来说「不存在」。但它真正要的只有三样：
#   * include/openssl/*.h   —— 头文件与平台无关，直接借宿主那份
#   * lib/libssl.lib 等     —— cargo check 不链接，空文件就够
#   * 能预处理的编译器        —— 桩脚本里的 -E 分支转交 clang
# 目录建在 target/ 下，不入库，删了会自动重建。
# ---------------------------------------------------------------
prepare_openssl_shim() {
	local brew_openssl
	brew_openssl="$(brew --prefix openssl@3 2>/dev/null || true)"
	if [[ -z "$brew_openssl" || ! -d "$brew_openssl/include/openssl" ]]; then
		echo "找不到 OpenSSL 头文件（brew install openssl@3），无法准备交叉检查用的假安装目录" >&2
		exit 1
	fi

	mkdir -p "$SHIM/lib"
	ln -sfn "$brew_openssl/include" "$SHIM/include"
	# 名字覆盖两种查找习惯：msvc 的 foo.lib 与 gnu 的 libfoo.a
	for name in ssl libssl crypto libcrypto; do
		: >"$SHIM/lib/$name.lib"
		: >"$SHIM/lib/$name.a"
	done
}

prepare_openssl_shim

export "${TARGET_PREFIX}_OPENSSL_NO_VENDOR=1"
export "${TARGET_PREFIX}_OPENSSL_DIR=$SHIM"
export CC_x86_64_pc_windows_msvc="$STUB"
export AR_x86_64_pc_windows_msvc="$STUB"

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

# daisy-rdp 的交叉检查要复用宿主生成的绑定。缓存被清掉时它会是空的，
# 这里补一次宿主 check（命中缓存时是毫秒级）。它注定输出那条「复用宿主绑定」
# 的警告，所以单独跑并静音输出，失败则照原样暴露。
step "宿主侧就绪（为 daisy-rdp 准备绑定的来源）"
cargo check -p daisy-rdp --quiet

step "cargo check --workspace --all-targets --target $TARGET"
cargo check --workspace --all-targets --target "$TARGET"

if [[ "$RUN_CLIPPY" == "1" ]]; then
	step "cargo clippy --workspace --all-targets --target ${TARGET}（-D warnings）"
	cargo clippy --workspace --all-targets --target "$TARGET" -- -D warnings
fi

printf '\n\033[1;32mWindows 目标交叉检查通过（仅编译，不代表可运行）\033[0m\n'
