#!/usr/bin/env bash
#
# 在 macOS 上构建 FreeRDP 静态库（供 Daisy 以库模式集成）
#
# 设计取舍：
#   - 只构建库，不构建 xfreerdp / SDL 客户端可执行文件（Daisy 自己管窗口和渲染）
#   - 关闭 FFmpeg / SWScale / Cairo / CUPS / PCSC / GSSAPI，减少第三方依赖
#   - 关闭 X11 / Wayland，macOS 用不到
#   - 静态库（BUILD_SHARED_LIBS=OFF），便于 Rust 侧链接，避免 dylib 路径问题
#
# 用法：
#   scripts/build-freerdp.sh                    # 配置 + 构建 + 安装
#   CONFIGURE_ONLY=1 scripts/build-freerdp.sh   # 只配置，快速验证选项
#   CLEAN=1 scripts/build-freerdp.sh            # 先清空构建目录再重来
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="$(uname -m)"
SRC="$ROOT/vendor/freerdp"
BUILD="$ROOT/vendor/build/freerdp-macos-$ARCH"
PREFIX="$ROOT/vendor/dist/freerdp-macos-$ARCH"

OPENSSL_PREFIX="$(brew --prefix openssl@3)"

log() { printf '\n[build-freerdp] %s\n' "$*"; }

if [[ ! -f "$SRC/CMakeLists.txt" ]]; then
  log "未找到 FreeRDP 源码，请先执行 scripts/fetch-freerdp.sh"
  exit 1
fi

if [[ "${CLEAN:-0}" == "1" ]]; then
  log "清空构建目录 $BUILD"
  rm -rf "$BUILD"
fi

# 本机 which openssl 指向 conda 的 3.5.7，必须显式把 brew 的 openssl@3 顶到最前
export PKG_CONFIG_PATH="$OPENSSL_PREFIX/lib/pkgconfig"
export CMAKE_PREFIX_PATH="$OPENSSL_PREFIX"

# Opus 音频编解码，默认关闭。
# 原因有二：
#   1. Daisy v1 范围只要求复制粘贴 / 远控 / 分辨率自适应，不含 RDP 音频；
#   2. Homebrew 的 opus.pc 输出 -I.../include/opus，而 FreeRDP 源码写的是
#      #include <opus/opus.h>，需要的是父目录，两者不兼容，直接开会导致编译失败。
# 需要音频时用 WITH_OPUS=ON 运行本脚本，下面的 EXTRA_CMAKE_ARGS 会补上父目录。
WITH_OPUS="${WITH_OPUS:-OFF}"
EXTRA_CMAKE_ARGS=""
if [[ "$WITH_OPUS" == "ON" ]]; then
  EXTRA_CMAKE_ARGS="-DCMAKE_C_FLAGS=-I$(brew --prefix opus)/include"
  log "Opus 已启用，注入额外 include 路径"
fi

log "配置：源码=$SRC"
log "配置：构建=$BUILD"
log "配置：安装=$PREFIX"
log "配置：OpenSSL=$OPENSSL_PREFIX"

# `WITH_VERBOSE_WINPR_ASSERT=OFF`：预研阶段这个 runtime-check 默认开着，实连时
# FreeRDP 自己会警告「might slow down the application」，画面刷新率受牵连（PLAN R18）。
# 它只影响断言宏，不改任何结构体布局，绑定无需重新生成。
cmake -S "$SRC" -B "$BUILD" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DWITH_VERBOSE_WINPR_ASSERT=OFF \
  -DCMAKE_INSTALL_PREFIX="$PREFIX" \
  -DCMAKE_OSX_DEPLOYMENT_TARGET=13.0 \
  -DBUILD_SHARED_LIBS=OFF \
  -DFREERDP_UNIFIED_BUILD=ON \
  -DWITH_CLIENT_COMMON=ON \
  -DWITH_CLIENT=OFF \
  -DWITH_SERVER=OFF \
  -DWITH_CHANNELS=ON \
  -DWITH_CLIENT_CHANNELS=ON \
  -DCHANNEL_URBDRC=OFF \
  -DWITH_SAMPLE=OFF \
  -DWITH_MANPAGES=OFF \
  -DWITH_FFMPEG=OFF \
  -DWITH_SWSCALE=OFF \
  -DWITH_CAIRO=OFF \
  -DWITH_CUPS=OFF \
  -DWITH_PCSC=OFF \
  -DWITH_GSSAPI=OFF \
  -DWITH_X11=OFF \
  -DWITH_WAYLAND=OFF \
  -DWITH_MACAUDIO=OFF \
  -DWITH_OPENSSL=ON \
  -DWITH_MBEDTLS=OFF \
  -DWITH_LIBRESSL=OFF \
  -DWITH_OPUS="$WITH_OPUS" \
  -DBUILD_TESTING=OFF \
  -DWITH_CCACHE=OFF \
  -DWITH_CLANG_FORMAT=OFF \
  $EXTRA_CMAKE_ARGS \
  2>&1 | tail -60

if [[ "${CONFIGURE_ONLY:-0}" == "1" ]]; then
  log "仅配置模式，结束"
  exit 0
fi

log "构建"
cmake --build "$BUILD" --parallel

log "安装到 $PREFIX"
cmake --install "$BUILD"

log "完成"
log "头文件：$(find "$PREFIX/include" -name '*.h' 2>/dev/null | wc -l | tr -d ' ') 个"
log "静态库："
find "$PREFIX/lib" -maxdepth 1 -name '*.a' -exec ls -lh {} \; 2>/dev/null | awk '{print "  " $9 "  " $5}'
