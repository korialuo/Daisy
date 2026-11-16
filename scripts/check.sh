#!/usr/bin/env bash
#
# Daisy 本地质量门。提交前必跑，CI 跑同一套。
#
# 覆盖：
#   1. 格式检查
#   2. clippy 零警告
#   3. 单元测试
#   4. Windows 交叉 check（只编译不链接，提前拦截平台相关错误）
#   5. 界面烟雾测试（窗口能正常启动并退出）
#
# 用法：
#   scripts/check.sh              全部跑
#   scripts/check.sh --no-smoke   跳过界面烟雾测试（无显示环境时用）
#
set -euo pipefail

# 非交互式 shell 不加载 .zshrc，必须自己导 PATH
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RUN_SMOKE=1
for arg in "$@"; do
  case "$arg" in
    --no-smoke) RUN_SMOKE=0 ;;
    *) echo "未知参数：$arg"; exit 2 ;;
  esac
done

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

step "cargo fmt --check"
cargo fmt --all -- --check

step "cargo clippy (零警告)"
cargo clippy --workspace --all-targets -- -D warnings

step "cargo test"
cargo test --workspace

# ---------------------------------------------------------------
# Windows 交叉 check 交给 scripts/check-windows.sh。
#
# 历史包袱说明：早期这里只挑三个「纯 Rust」crate 检查，因为带原生 C 依赖的
# crate（store 的 vendored OpenSSL、ssh 的 ring、rdp 的 FreeRDP/bindgen）
# 在 macOS 上根本走不到我们的代码就构建失败了。现在那条路绕过去了
# （桩编译器 + 假 OpenSSL 安装目录 + 复用宿主绑定），所以改成**全工作区**，
# 应用层的平台错误也能提前拦住。详见 scripts/check-windows.sh 与
# PLAN.md 8.1 的 D1b。
# ---------------------------------------------------------------
step "Windows 目标交叉检查（全工作区，仅编译）"
scripts/check-windows.sh

if [[ "$RUN_SMOKE" == "1" ]]; then
  step "界面烟雾测试（3 秒后自动关闭窗口）"
  DAISY_SMOKE_MS=3000 cargo run -q -p daisy-app
fi

printf '\n\033[1;32m全部通过\033[0m\n'
