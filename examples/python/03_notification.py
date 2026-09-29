#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""发送一条系统通知：标题、正文、图标与按钮。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/03_notification.py
"""

from __future__ import annotations

import sys

import _bootstrap
from _bootstrap import ICONS

from uda import Uda


#: 发送方应用名；在 Windows 上它就是 toast 的 AppUserModelID。
APP_NAME = "UDA Notification Demo"

#: 通知卡片上显示的图标。
#:
#: 选 ``UniDesktop_3D_transparent_mini.png`` 而不是 ``UniDesktop.png``：前者
#: 500×333 带透明通道，Toast 直接 `<image src>` 引用原图即可，无需缩放；后者
#: 1254×1254 不透明且体积大 10 倍，卡片里既显得粗糙又拖慢渲染。
APP_ICON = ICONS / "UniDesktop_3D_transparent_mini.png"


def main() -> int:
    if not APP_ICON.is_file():
        print(f"未找到通知图标 {APP_ICON}", file=sys.stderr)
        return 1

    with Uda() as uda:
        # app_name 必须显式给出：未打包进程没有 toast 身份，UDA 会用它注册一个。
        uda.notify(
            title="来自 UDA 的问候",
            body="这是一条通过 UniDesktop API 发出的系统通知。",
            icon=str(APP_ICON),
            actions={"open": "查看详情", "later": "稍后提醒"},
            app_name=APP_NAME,
        )
        print("通知已发送。")
        print(f"  发送方: {APP_NAME}")
        print(f"  图标:   {APP_ICON}")

    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap.run(main))
