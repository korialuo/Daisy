#!/usr/bin/env bash
#
# 把 Daisy 打包成 macOS 的 .app 与 .dmg。
#
# 用法：
#   scripts/build-macos.sh            # 完整打包（.app + .dmg）
#   scripts/build-macos.sh --app-only # 只出 .app，跳过 dmg
#
# 产物：
#   dist/Daisy.app
#   dist/Daisy-<版本>.dmg
#
# 关于构建 profile：默认 release（发布形态）。`DAISY_PACKAGE_PROFILE=debug`
# 可用 debug 二进制打包——用于在受限环境里验证打包流程本身，
# 产物不可对外分发。
#
# 关于签名：这里只做 ad-hoc 签名（`codesign -s -`），本机与内部测试可用；
# 对外分发需要 Developer ID 证书 + 公证，那属于发布流程，不在本脚本范围。
#
# 关于依赖（PLAN.md R9）：脚本会校验二进制**没有**指向 Homebrew /
# /usr/local 的 dylib。测试机上装了 Homebrew 的话，一旦某个依赖退化成
# 动态链接，本机跑得好好的、换台干净机器就崩——所以在打包这一步就拦。
#
# **每次代码改动后都要跑这个脚本**：用户习惯双击 `dist/Daisy.app` 直接验证，
# 只交代码不更新包，他打开的还是旧版本。收尾固定为「提交 → 打包 → 给出 dmg」
# 三步。脚本末尾会比对二进制与最近一次提交的时间，落后就告警——
# 本项目踩过这个坑：用户双击后报「RDP 会话正在收尾」，根因就是 dist 里
# 躺着改动前的二进制。

set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="Daisy"
BIN_NAME="daisy-app"
BUNDLE_ID="io.github.korialuo.daisy"
# Apple Silicon 最低就是 macOS 11，与 eframe 0.36 的要求取交集
MIN_MACOS="11.0"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
PROFILE="${DAISY_PACKAGE_PROFILE:-release}"
TARGET_DIR="target/${PROFILE}"
OUT_DIR="dist"
APP_DIR="${OUT_DIR}/${APP_NAME}.app"
DMG_PATH="${OUT_DIR}/${APP_NAME}-${VERSION}-${PROFILE}.dmg"

APP_ONLY=0
for arg in "$@"; do
	case "$arg" in
		--app-only) APP_ONLY=1 ;;
		-h | --help)
			sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
			exit 0
			;;
		*)
			echo "未知参数：$arg" >&2
			exit 2
			;;
	esac
done

echo "==> 构建 ${PROFILE}（版本 ${VERSION}）"
if [ "${PROFILE}" = "release" ]; then
	cargo build --release --bin "${BIN_NAME}"
else
	cargo build --bin "${BIN_NAME}"
fi

echo "==> 组装 ${APP_DIR}"
rm -rf "${APP_DIR}"
mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources"

cp "${TARGET_DIR}/${BIN_NAME}" "${APP_DIR}/Contents/MacOS/${BIN_NAME}"
cp assets/icons/daisy.icns "${APP_DIR}/Contents/Resources/daisy.icns"

cat >"${APP_DIR}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>${APP_NAME}</string>
	<key>CFBundleDisplayName</key>
	<string>${APP_NAME}</string>
	<key>CFBundleIdentifier</key>
	<string>${BUNDLE_ID}</string>
	<key>CFBundleExecutable</key>
	<string>${BIN_NAME}</string>
	<key>CFBundleIconFile</key>
	<string>daisy.icns</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
	<key>CFBundleVersion</key>
	<string>${VERSION}</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>LSMinimumSystemVersion</key>
	<string>${MIN_MACOS}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSSupportsAutomaticGraphicsSwitching</key>
	<true/>
</dict>
</plist>
PLIST

# PkgInfo 是老格式的遗留，但有些工具仍会读
printf 'APPL????' >"${APP_DIR}/Contents/PkgInfo"

echo "==> 校验没有 Homebrew 依赖（R9）"
external="$(otool -L "${APP_DIR}/Contents/MacOS/${BIN_NAME}" | grep -E '/opt/homebrew|/usr/local' || true)"
if [ -n "$external" ]; then
	echo "错误：二进制依赖了非系统路径的库，换到干净机器会启动失败：" >&2
	echo "$external" >&2
	echo "对策：把该依赖改为静态链接（vendor 构建），或一并塞进 Resources 并修 rpath。" >&2
	exit 1
fi
echo "    通过：只依赖系统框架与 /usr/lib"

echo "==> ad-hoc 签名"
codesign --force --sign - "${APP_DIR}"
codesign --verify --verbose=1 "${APP_DIR}"

if [ "${APP_ONLY}" -eq 1 ]; then
	echo "完成：${APP_DIR}"
	exit 0
fi

echo "==> 生成 dmg"
rm -f "${DMG_PATH}"
hdiutil create -volname "${APP_NAME}" -srcfolder "${APP_DIR}" -ov -format UDZO "${DMG_PATH}" >/dev/null

echo
echo "完成："
echo "  ${APP_DIR}"
echo "  ${DMG_PATH} ($(du -h "${DMG_PATH}" | cut -f1))"

# 确认包里的二进制确实比最近一次提交新——旧包会让用户以为改动没生效
# （本项目踩过：用户双击后报"RDP 会话正在收尾"，根因就是 dist 里躺着旧二进制）
if git rev-parse --git-dir >/dev/null 2>&1; then
	commit_time=$(git log -1 --format=%ct)
	binary_time=$(stat -f %m "${APP_DIR}/Contents/MacOS/${BIN_NAME}")
	if [ "${binary_time}" -lt "${commit_time}" ]; then
		echo >&2
		echo "警告：${APP_DIR} 比最近一次提交还旧，用户验证到的会是改动前的版本。" >&2
		echo "     （提交时间 ${commit_time} > 二进制时间 ${binary_time}）" >&2
	fi
fi
