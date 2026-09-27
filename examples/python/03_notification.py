#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""发送一条系统通知：标题、正文、图标与按钮。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/03_notification.py
"""

from __future__ import annotations

import _bootstrap
from _bootstrap import ICONS

from uda import Uda


#: 发送方应用名；在 Windows 上它就是 toast 的 AppUserModelID。
APP_NAME = "UDA Notification Demo"


def main() -> int:
    with Uda() as uda:
        # app_name 必须显式给出：未打包进程没有 toast 身份，UDA 会用它注册一个。
        uda.notify(
            title="来自 UDA 的问候",
            body="这是一条通过 UniDesktop API 发出的系统通知。",
            icon=str(ICONS / "UniDesktop.png"),
            actions={"open": "查看详情", "later": "稍后提醒"},
            app_name=APP_NAME,
        )
        print("通知已发送。")

    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap.run(main))
