#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""申请防休眠常亮锁，保持 3 秒后优雅释放。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/04_wakelock.py
"""

from __future__ import annotations

import time

import _bootstrap  # noqa: F401  # 建立编码护栏与 sys.path

from uda import Uda

#: 常亮锁持有时长（秒）。
HOLD_SECONDS = 3.0


def main() -> int:
    with Uda() as uda:
        print(f"申请常亮锁，保持 {HOLD_SECONDS:.0f} 秒 ...")

        # 退出 with 块时自动释放，无需手动调用 release()。
        with uda.wakelock():
            time.sleep(HOLD_SECONDS)

        print("常亮锁已释放。")

    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap.run(main))
