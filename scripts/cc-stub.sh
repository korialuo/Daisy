#!/bin/bash
# 桩编译器 —— 只服务于 `cargo check --target x86_64-pc-windows-msvc`。
#
# 解决的问题：交叉检查 Windows 目标时，`ring` / `sqlcipher` / `openssl` 这些
# 带 C 代码的依赖会真的去调编译器，而 macOS 上既没有 MSVC 也没有 Windows SDK，
# 构建脚本直接失败，**整条 cargo check 就在第三方依赖处断掉**，
# 我们自己代码里的 `cfg(windows)` 错误一个都露不出来。
#
# `cargo check` 不链接，所以那些 .obj / .lib 的**内容**没人读。
# 本脚本做两件事：
#   1. 把构建脚本期望出现的产物文件建出来（空的），让它顺利往下走；
#   2. 遇到**预处理**（`-E`）时转交宿主 clang —— openssl-sys 要靠预处理
#      expando.c 认出 OpenSSL 版本，拿不到版本它会 panic。
#
# **产物不能用于 cargo build / cargo run**：真正的 Windows 构建在 Windows
# 机器上做（S11 步骤 81–82）。见 scripts/check-windows.sh。

set -u

# 建出一个空产物。父目录可能还不存在（cc-rs 有时把 -out: 指向未建的子目录）。
make_empty() {
	local dir
	dir=$(dirname "$1")
	[ -d "$dir" ] || mkdir -p "$dir" 2>/dev/null || return 0
	: >"$1" 2>/dev/null || true
}

# ---- 预处理：转交宿主 clang ----
# 只读宏、不产生目标代码，所以宿主 clang 完全够用。
# 但要**只保留预处理器认识的参数**：cc-rs 给 MSVC 加的是 `-nologo` / `-Z7`
# 这类 cl.exe 专用开关，clang 会直接报 unknown argument；
# `--target=` 也必须去掉，否则 clang 会去找不存在的 Windows SDK 头文件。
for arg in "$@"; do
	if [ "$arg" = "-E" ] || [ "$arg" = "/E" ]; then
		host_args=()
		want_include_path=0
		for a in "$@"; do
			if [ "$want_include_path" = 1 ]; then
				host_args+=("-I" "$a")
				want_include_path=0
				continue
			fi
			case "$a" in
				-E) continue ;;
				-I) want_include_path=1 ;;
				-I* | -D* | -U*) host_args+=("$a") ;;
				*.c) host_args+=("$a") ;;
			esac
		done
		host_args+=("-E")
		exec /usr/bin/clang "${host_args[@]}"
	fi
done

# ---- 其余调用：只建产物文件 ----
prev=""
for arg in "$@"; do
	case "$arg" in
		# 归档（MSVC lib.exe 风格）：/OUT:path.lib
		/OUT:*) make_empty "${arg#/OUT:}" ;;
		# 编译（cl.exe 风格）：/Fopath.obj
		/Fo*) make_empty "${arg#/Fo}" ;;
		# 归档（cc-rs 内部拼的 ar 风格）：-out:path.a
		-out:*) make_empty "${arg#-out:}" ;;
		# 裸产物路径：只有以 .a / .lib 结尾才认，避免把源码名当产物
		*.a | *.lib)
			case "$arg" in -*) ;; *) make_empty "$arg" ;; esac
			;;
	esac

	# 独立参数的 -o path（gcc 风格），路径在下一个参数上
	if [ "$prev" = "-o" ]; then
		make_empty "$arg"
	fi
	prev="$arg"
done

exit 0
