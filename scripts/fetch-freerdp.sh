#!/usr/bin/env bash
#
# 拉取并解压 FreeRDP 源码到 vendor/freerdp
#
# 为什么不用 git submodule：
#   本机直连 GitHub 的 git 协议吞吐约 9 KB/s，shallow clone 跑 16 分钟只拿到 10 MB。
#   改用 codeload tarball（约 145 KB/s），同样的源码 1 分钟出头即可拿到。
#   tarball 带固定 SHA256 校验，可复现性不打折。
#
# 用法：
#   scripts/fetch-freerdp.sh            # 有缓存则直接用缓存
#   scripts/fetch-freerdp.sh --force    # 强制重新下载并覆盖
#
set -euo pipefail

VERSION="3.32.1"
SHA256="8803dd26ec9660550252f255cf2d672a785ddd8f544bb475834993e96807c87f"
URL="https://codeload.github.com/FreeRDP/FreeRDP/tar.gz/refs/tags/${VERSION}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE="$ROOT/vendor/.cache"
TARBALL="$CACHE/freerdp-${VERSION}.tar.gz"
DEST="$ROOT/vendor/freerdp"

log() { printf '[fetch-freerdp] %s\n' "$*"; }

FORCE=0
[[ "${1:-}" == "--force" ]] && FORCE=1

if [[ "$FORCE" == "1" ]]; then
  log "强制模式：清除缓存与已解压源码"
  rm -f "$TARBALL"
  rm -rf "$DEST"
fi

mkdir -p "$CACHE"

if [[ ! -f "$TARBALL" ]]; then
  log "下载 FreeRDP ${VERSION} ..."
  curl -fL --retry 3 --retry-delay 2 -C - --progress-bar -o "$TARBALL" "$URL"
else
  log "命中缓存：$TARBALL"
fi

log "校验 SHA256 ..."
ACTUAL="$(shasum -a 256 "$TARBALL" | awk '{print $1}')"
if [[ "$ACTUAL" != "$SHA256" ]]; then
  log "校验失败"
  log "  期望：$SHA256"
  log "  实际：$ACTUAL"
  log "缓存文件可能损坏，删除后重试：rm '$TARBALL'"
  exit 1
fi

log "解压到 vendor/freerdp ..."
rm -rf "$DEST"
mkdir -p "$DEST"
tar -xzf "$TARBALL" -C "$DEST" --strip-components=1

if [[ ! -f "$DEST/CMakeLists.txt" ]]; then
  log "解压结果异常：未在 $DEST 找到 CMakeLists.txt"
  exit 1
fi

log "完成：$DEST"
log "源码大小：$(du -sh "$DEST" | awk '{print $1}')"
