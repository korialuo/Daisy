#!/usr/bin/env python3
"""生成 Daisy 的应用图标（PNG + 原始 RGBA + macOS .icns + Windows .ico）。

为什么用代码画而不是生成式出图：应用图标要在 16×16 下依然可辨识，
元素必须少而粗、形状必须精确——这两点手工可控的矢量式绘制比生成更稳。

图形：深墨色圆角方块上一朵十二瓣雏菊（Daisy），花心是一块**大号**琥珀色圆盘，
盘里嵌着深色的 `>_<`——两个折线当眼睛、横杠当嘴，既是表情，也还是终端提示符。

- **雏菊** 是名字本身；花瓣短而圆，根部压在花心下面，读起来是「从花心里长出来的」，
  不是一圈围在外面的齿轮；
- **花心 + `>_<`** 是终端：琥珀是光标底色，符号是提示符，同色系里做正负形；
  三笔画也让花心有了表情，缩小时比两笔画更耐看；
- 符号放进花心而不是单独摆一行，是因为 1024 的画布上只有**一个**视觉焦点时
  缩放才不会散：原来花下面那行提示符在 32px 以下基本糊成一个小点；
- 全扁平：纯色块、无渐变、无阴影，小尺寸不会糊成一团。

用法（需要 Pillow）：

    python3 -m pip install pillow
    python3 scripts/make-icon.py
"""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw

# macOS Big Sur 之后的圆角半径，按图标边长的比例取
CORNER_RATIO = 0.2237

# 画布尺寸：图标按 1024 输出，再向下缩放出 iconset 各档
CANVAS = 1024

# 扁平配色：一块底色 + 两种前景色，不用渐变。
# 底色比应用自己的深色面板（#1C1C1E）亮一档：图标要贴在深色 Dock / 深色壁纸上，
# 跟背景同深就只剩花瓣浮在空中，圆角轮廓会消失。
BG_COLOR = (34, 37, 44)
PETAL_COLOR = (240, 244, 249)
ACCENT_COLOR = (246, 184, 68)

# 花：画布正中，十二瓣。
#
# 八瓣配「大花心」看起来像太阳（瓣太短、缝太大）；十二瓣每片更小更挤，
# 才有一朵盛开雏菊的样子。再多（16 瓣）在 16px 会糊成白圈，所以停在 12。
PETALS = 12
# 花瓣：椭圆，根部**压进花心**（内半径小于花心半径），所以看起来是从花心里长出来的。
# 长宽比 1.79（190/106）——太瘦会像齿轮，太圆会糊成一圈白边。
PETAL_INNER_R = 178
PETAL_OUTER_R = 368
PETAL_WIDTH = 106

# 花心：琥珀圆盘。花心直径占整个图案的 57%，比早期版本明显放大，
# 这样 `>_` 才有地方放得舒展。
CORE_RADIUS = 210

# 花心里的 `>_<`：两个折线当眼睛、横杠当嘴，读作一个表情，同时仍是终端提示符。
#
# 比例全部相对**花心直径**取。早期是用「span + 若干倍 span」推的，多一个符号
# 就整体变宽——视觉宽高比 2.2，用户反馈「太扁」。按直径定比例、并把折线拉高到
# 0.33 直径之后是 1.59；再拉高（0.36）横杠就被衬得像一条短线，读成 `>-<`。
FACE_CHEEK_RATIO = 0.145  # 单个折线宽 / 花心直径
FACE_BAR_RATIO = 0.220  # 横杠长 / 花心直径
FACE_GAP_RATIO = 0.045  # 符号间距 / 花心直径
FACE_HEIGHT_RATIO = 0.330  # 折线高 / 花心直径
FACE_STROKE_RATIO = 0.088  # 笔宽 / 花心直径
# `_` 在真实文本里落在基线之下，所以横杠比折线下端再低一点点
FACE_DROP_RATIO = 0.015

# 窗口图标用的原始 RGBA 边长（`include_bytes!` 嵌进二进制，无需解码库）
WINDOW_ICON_SIZE = 256

OUT_DIR = Path(__file__).resolve().parent.parent / "assets" / "icons"


def rounded_mask(size: int, radius: int) -> Image.Image:
	"""圆角矩形遮罩（L 模式，255 = 保留）。"""
	mask = Image.new("L", (size, size), 0)
	ImageDraw.Draw(mask).rounded_rectangle(
		[(0, 0), (size - 1, size - 1)], radius=radius, fill=255
	)
	return mask


def draw_petals(layer: Image.Image, center: tuple[float, float]) -> None:
	"""八个花瓣：把一个竖直椭圆绕中心转八次。

	花瓣之间必须留得出缝：缝隙没了就成了一朵「实心菊」，16px 下只剩一个白团。
	十二等分时花瓣中段相邻的槽宽约 131px，瓣宽 106 还留 25px 的缝，够分开。
	"""
	cx, cy = center
	radial = (PETAL_OUTER_R - PETAL_INNER_R) / 2
	mid = (PETAL_OUTER_R + PETAL_INNER_R) / 2

	petal = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
	ImageDraw.Draw(petal).ellipse(
		[
			cx - PETAL_WIDTH / 2,
			cy - mid - radial,
			cx + PETAL_WIDTH / 2,
			cy - mid + radial,
		],
		fill=PETAL_COLOR,
	)

	for index in range(PETALS):
		# 旋转后的抗锯齿边缘要靠 BICUBIC，NEAREST 会在斜边上留下锯齿
		rotated = petal.rotate(
			index * 360 / PETALS,
			center=(cx, cy),
			resample=Image.BICUBIC,
		)
		layer.alpha_composite(rotated)


def draw_face(layer: Image.Image, center: tuple[float, float]) -> None:
	"""在花心里挖出深色的 `>_<`。

	用底色画而不是换一种前景色：正负形的关系更像「光标块上的字」，
	也比再加一个颜色干净。六个尺寸全部由花心直径推出来，改一处就整体缩放。
	"""
	cx, cy = center
	diameter = CORE_RADIUS * 2
	cheek = diameter * FACE_CHEEK_RATIO
	bar = diameter * FACE_BAR_RATIO
	gap = diameter * FACE_GAP_RATIO
	height = diameter * FACE_HEIGHT_RATIO
	stroke = diameter * FACE_STROKE_RATIO
	drop = diameter * FACE_DROP_RATIO

	total = cheek * 2 + bar + gap * 2
	left = cx - total / 2
	right = cx + total / 2
	top = cy - height / 2
	bottom = cy + height / 2
	radius = stroke / 2

	draw = ImageDraw.Draw(layer)

	def polyline(points: list[tuple[float, float]]) -> None:
		"""折线 + 三个圆头。PIL 的线端是方的，斜线两端会露尖角。"""
		draw.line(points, fill=BG_COLOR, width=round(stroke), joint="curve")
		for point_x, point_y in points:
			draw.ellipse(
				[point_x - radius, point_y - radius, point_x + radius, point_y + radius],
				fill=BG_COLOR,
			)

	# 左 `>`：顶点朝右
	polyline([(left, top), (left + cheek, cy), (left, bottom)])
	# 右 `<`：顶点朝左
	polyline([(right, top), (right - cheek, cy), (right, bottom)])

	# 横杠（`_`）：居中夹在两个折线之间，比折线下端再低一点
	bar_left = left + cheek + gap
	draw.rounded_rectangle(
		[bar_left, bottom + drop - radius, bar_left + bar, bottom + drop + radius],
		radius=radius,
		fill=BG_COLOR,
	)


def build_master() -> Image.Image:
	"""生成 1024×1024 的主图。"""
	center = (CANVAS / 2, CANVAS / 2)

	foreground = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
	# 顺序：先花瓣、再花心——花瓣根部要压在花心下面
	draw_petals(foreground, center)
	ImageDraw.Draw(foreground).ellipse(
		[
			center[0] - CORE_RADIUS,
			center[1] - CORE_RADIUS,
			center[0] + CORE_RADIUS,
			center[1] + CORE_RADIUS,
		],
		fill=ACCENT_COLOR,
	)
	draw_face(foreground, center)

	background = Image.new("RGBA", (CANVAS, CANVAS), (*BG_COLOR, 255))
	composed = Image.alpha_composite(background, foreground)

	# 圆角裁切：圆角之外置透明
	result = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
	result.paste(composed, (0, 0), rounded_mask(CANVAS, round(CANVAS * CORNER_RATIO)))
	return result


# iconset 需要的档位：(文件名, 边长)
ICONSET_SIZES = [
	("icon_16x16.png", 16),
	("icon_16x16@2x.png", 32),
	("icon_32x32.png", 32),
	("icon_32x32@2x.png", 64),
	("icon_128x128.png", 128),
	("icon_128x128@2x.png", 256),
	("icon_256x256.png", 256),
	("icon_256x256@2x.png", 512),
	("icon_512x512.png", 512),
	("icon_512x512@2x.png", 1024),
]


# 尺寸核对图：文档里引用的那张。浅底与深底各一行，外加 32/16 的像素放大。
SHOWCASE_SIZES = [256, 128, 64, 48, 32, 16]


def write_showcase(master: Image.Image) -> Path:
	"""出 docs/icon-showcase.png：验证各档尺寸下还认不认得出。"""
	dark = (28, 30, 36)
	light = (235, 238, 242)
	gap = 22
	margin = 24
	zoom = 5
	row_h = max(SHOWCASE_SIZES) + gap * 2

	width = margin * 2 + sum(SHOWCASE_SIZES) + gap * (len(SHOWCASE_SIZES) - 1)
	width = max(width, margin * 2 + 32 * zoom + gap + 16 * zoom)
	height = margin + row_h * 2 + 16 * zoom + gap * 2

	sheet = Image.new("RGB", (width, height), dark)
	draw = ImageDraw.Draw(sheet)

	# 两行底色：深底看「贴深色 Dock」，浅底看「贴浅色壁纸」
	for row, background in enumerate((dark, light)):
		top = margin + row * row_h
		draw.rectangle([(0, top), (width, top + row_h)], fill=background)
		x = margin
		for size in SHOWCASE_SIZES:
			sheet.paste(
				master.resize((size, size), Image.LANCZOS).convert("RGB"),
				(x, top + (row_h - size) // 2),
			)
			x += size + gap

	# 像素放大：看 32 / 16 究竟是哪几颗像素在表达花形
	top = margin + row_h * 2
	x = margin
	for size in (32, 16):
		sheet.paste(
			master.resize((size, size), Image.LANCZOS)
			.resize((size * zoom, size * zoom), Image.NEAREST)
			.convert("RGB"),
			(x, top),
		)
		x += size * zoom + gap

	path = OUT_DIR.parent.parent / "docs" / "icon-showcase.png"
	path.parent.mkdir(parents=True, exist_ok=True)
	sheet.save(path)
	return path


def main() -> int:
	OUT_DIR.mkdir(parents=True, exist_ok=True)
	master = build_master()

	master_path = OUT_DIR / "daisy-icon.png"
	master.save(master_path, "PNG")
	print(f"主图：{master_path}")

	# 窗口图标：原始 RGBA 字节，主程序用 include_bytes! 嵌入。
	# 存原始像素是为了不引入 PNG 解码依赖——文件大一点，省一个 crate。
	rgba_path = OUT_DIR / "daisy-icon.rgba"
	small = master.resize((WINDOW_ICON_SIZE, WINDOW_ICON_SIZE), Image.LANCZOS)
	rgba_path.write_bytes(small.tobytes())
	print(
		f"窗口图标：{rgba_path}"
		f"（{WINDOW_ICON_SIZE}×{WINDOW_ICON_SIZE} 原始 RGBA，{rgba_path.stat().st_size} 字节）"
	)

	# iconset 是 iconutil 要求的目录结构
	iconset = OUT_DIR / "daisy.iconset"
	if iconset.exists():
		shutil.rmtree(iconset)
	iconset.mkdir()

	for name, size in ICONSET_SIZES:
		master.resize((size, size), Image.LANCZOS).save(iconset / name, "PNG")

	icns_path = OUT_DIR / "daisy.icns"
	subprocess.run(
		["iconutil", "-c", "icns", str(iconset), "-o", str(icns_path)],
		check=True,
	)
	shutil.rmtree(iconset)
	print(f"icns：{icns_path}")

	# Windows 的 .ico：exe 里的资源图标（任务栏、资源管理器、快捷方式）。
	# 单文件里塞多档尺寸，系统按显示场景自己挑：16/20/24 是列表与标题栏，
	# 32 是桌面小图标，48 是中等图标，256 是「大图标/特大图标」视图。
	# 这一份和 .icns 来自同一张主图，两个平台的图标才不会长得不一样。
	ico_path = OUT_DIR / "daisy-icon.ico"
	master.save(
		ico_path,
		format="ICO",
		sizes=[(16, 16), (20, 20), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
	)
	print(f"ico：{ico_path}（{ico_path.stat().st_size} 字节）")

	showcase = write_showcase(master)
	print(f"尺寸核对图：{showcase}")
	return 0


if __name__ == "__main__":
	sys.exit(main())
