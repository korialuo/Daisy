#!/usr/bin/env python3
"""生成 dmg 打开时 Finder 的窗口布局（`.DS_Store` 模板）。

用法：

    python3 -m pip install --user ds_store      # 一次性，纯 Python，无编译
    python3 scripts/dmg-layout.py               # 默认写 assets/dmg/ds_store.template

为什么做成「预生成模板 + 打包时拷进去」而不是打包时现写：

- 现写的常规做法是挂载可写 dmg，用 AppleScript 指挥 Finder 摆图标，再把
  `.DS_Store` 读回来。**这条路要求「自动化 → Finder」权限**，在无 GUI 的
  环境里直接报 `-10004 权限违例`，脚本会静默产出一个没有布局的包。
- `.DS_Store` 里的窗口尺寸与图标坐标只跟**文件名**绑定（`Daisy.app`、
  `Applications`），与具体卷、具体机器无关，所以一份模板可以长期复用。
  模板在打包时随镜像一起写进去，Finder 打开时就按模板摆——全程不碰 GUI。

什么时候需要重跑：改了窗口尺寸、图标大小、图标位置，或者改了 app 的文件名。
改完把新生成的 `assets/dmg/ds_store.template` 一起提交。
"""

import sys
from pathlib import Path

try:
    from ds_store import DSStore
except ImportError:  # pragma: no cover - 只在缺依赖时走到
    sys.exit(
        "缺少 ds_store。安装：python3 -m pip install --user ds_store"
    )

# 窗口尺寸（逻辑点）。两个图标加一点留白就够了——开太大在 Retina 上很占地方，
# 用户还得多拖一下才能看到「应用程序」快捷方式。
WINDOW_SIZE = (560, 360)
WINDOW_ORIGIN = (200, 180)
ICON_SIZE = 128
# 图标中心在窗口内的坐标。左「app」右「Applications」，中间留出拖拽路径。
ICON_POSITIONS = {
    "Daisy.app": (150, 160),
    "Applications": (410, 160),
}

DEFAULT_OUT = Path("assets/dmg/ds_store.template")


def main() -> None:
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_OUT
    out.parent.mkdir(parents=True, exist_ok=True)
    if out.exists():
        out.unlink()

    bwsp = {
        "WindowBounds": "{{%d, %d}, {%d, %d}}"
        % (WINDOW_ORIGIN[0], WINDOW_ORIGIN[1], WINDOW_SIZE[0], WINDOW_SIZE[1]),
        "ShowStatusBar": False,
        "ShowTabView": False,
        "ShowToolbar": False,
        "ShowPathbar": False,
        "ShowSidebar": False,
        "ContainerShowSidebar": False,
        "PreviewPaneVisibility": False,
        "SidebarWidth": 0,
    }
    icvp = {
        "viewOptionsVersion": 1,
        "backgroundType": 1,  # 1 = 纯色背景
        "backgroundColorRed": 1.0,
        "backgroundColorGreen": 1.0,
        "backgroundColorBlue": 1.0,
        "gridOffsetX": 0.0,
        "gridOffsetY": 0.0,
        "gridSpacing": 100.0,
        "iconSize": float(ICON_SIZE),
        "showIconPreview": False,
        "showItemInfo": False,
        "labelOnBottom": True,
        "textSize": 13.0,
        # 不自动排列，否则 Finder 会忽略下面的坐标重新摆一遍
        "arrangeBy": "none",
    }

    with DSStore.open(str(out), "w+") as store:
        store["."]["bwsp"] = bwsp
        store["."]["icvp"] = icvp
        for name, (x, y) in ICON_POSITIONS.items():
            store[name]["Iloc"] = (x, y)

    # 读回来确认真的写进去了——写坏的 .DS_Store 只会让 Finder 忽略布局，
    # 不会报错，所以这里必须自己验一遍。
    with DSStore.open(str(out), "r") as store:
        assert store["."]["bwsp"]["WindowBounds"] == bwsp["WindowBounds"]
        for name, pos in ICON_POSITIONS.items():
            assert tuple(store[name]["Iloc"]) == pos, name

    print(f"已生成 {out}（{out.stat().st_size} 字节）")
    print(f"  窗口 {WINDOW_SIZE[0]}×{WINDOW_SIZE[1]}，图标 {ICON_SIZE}px")
    for name, pos in ICON_POSITIONS.items():
        print(f"  {name} → {pos}")


if __name__ == "__main__":
    main()
