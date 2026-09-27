#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""检测系统的深浅色模式与强调色。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/01_appearance.py
"""

from __future__ import annotations

import _bootstrap  # noqa: F401  # 建立编码护栏与 sys.path

from uda import Uda


def main() -> int:
    with Uda() as uda:
        print(f"系统主题: {uda.theme}")

        color = uda.accent_color
        if color is None:
            print("强调色: 当前平台不提供")
        else:
            r, g, b, _a = color
            print(f"强调色: #{r:02x}{g:02x}{b:02x}")

    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap.run(main))
