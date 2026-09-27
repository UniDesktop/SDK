#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""创建系统托盘图标，并挂载带回调的右键菜单。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/05_tray.py

右键点击托盘图标即可看到菜单；选择"退出程序"或按 Ctrl+C 结束。
"""

from __future__ import annotations

import _bootstrap
from pathlib import Path

from uda import Uda


def main() -> int:
    with Uda() as uda:
        # 直接给一个 .png 路径即可：SDK 内部会解码成 RGBA 再提交，
        # 因此 Windows 与 Linux 都能正常显示。
        icon = uda.create_tray_icon("UDA Tray Demo", "UDA Tray Demo", Path(__file__).resolve().parent.parent.parent / "icons/UniDesktop_3D_transparent.png")

        menu = uda.create_tray_menu()
        menu.add_text("欢迎使用 UniDesktop", lambda item_id, data: print(f"点击了菜单项 {item_id}"))
        menu.add_checkbox(
            "开启深色模式同步",
            checked=False,
            callback=lambda item_id, checked, data: print(f"复选框 {item_id} -> {checked}"),
        )
        menu.add_separator()
        menu.add_text("退出程序", lambda item_id, data: icon.stop())
        icon.menu = menu

        print("托盘图标已创建，右键查看菜单；选择\"退出程序\"结束。")
        icon.wait()  # 阻塞，直到"退出程序"把 stop() 置位

    print("已退出。")
    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap.run(main))
