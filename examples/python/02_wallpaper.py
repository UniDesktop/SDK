#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""读取当前壁纸，并把壁纸切换为指定图片。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/02_wallpaper.py [图片路径] [填充模式]

``图片路径`` 默认为 ``icons/UniDesktop.png``；``填充模式`` 为
``crop`` / ``fill`` / ``fit`` / ``stretch``，默认 ``fill``。
"""

from __future__ import annotations

import sys

import _bootstrap
from _bootstrap import ICONS

from uda import FillMode, Uda


def main() -> int:
    image = sys.argv[1] if len(sys.argv) > 1 else str(ICONS / "UniDesktop.png")
    fill_mode = sys.argv[2] if len(sys.argv) > 2 else FillMode.FILL

    with Uda() as uda:
        print(f"切换前壁纸: {uda.wallpaper or '（未设置）'}")

        uda.set_wallpaper(image, fill_mode)
        print(f"已设置壁纸: {image}（填充模式 {fill_mode}）")

        print(f"切换后壁纸: {uda.wallpaper or '（未设置）'}")

    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap.run(main))
