#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""UDA C-ABI 层 Python 演示。

演示三项核心能力：

1. 检测系统深浅色模式（Dark / Light）；
2. 读取当前壁纸路径；
3. 申请防休眠常亮锁，保持 2 秒后释放。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/demo.py

也可通过 ``UDA_LIBRARY`` 显式指定动态库路径。

跨平台输出说明：Windows 控制台默认代码页（北美镜像为 ``cp1252``）无法编码
中文，因此本脚本在启动时把标准输出重配置为 UTF-8，并在探测到控制台可能受限于
代码页时自动回落到英文文案（``UDA_LANG=en`` 可强制英文，``UDA_LANG=zh-CN``
可强制中文）。
"""

from __future__ import annotations

import sys
from pathlib import Path

# 使 `import uda` / `import _encoding` 在本脚本位于 examples/python/ 时仍可用。
sys.path.insert(0, str(Path(__file__).resolve().parent))

# 必须先建立编码护栏：任何后续 import 期间的 print() 都不会再触发编码错误。
from _encoding import (  # noqa: E402
    console_supports_unicode,
    detect_language,
    force_utf8_output,
)

force_utf8_output()

import time  # noqa: E402

from uda import Uda, UdaError  # noqa: E402

#: 演示中常亮锁的持有时长（秒）。
WAKELOCK_HOLD_SECONDS = 2.0

#: 控制台能否原样呈现中文；为 False 时全部输出降级为英文。
_UNICODE_OK = console_supports_unicode()

#: 界面语言；``en`` 时输出英文文案，其余输出中文文案。
_LANGUAGE = detect_language()


def _text(zh: str, en: str) -> str:
    """按语言探测结果返回中文或英文文案。"""
    return en if _LANGUAGE == "en" else zh


def print_theme(uda: Uda) -> None:
    """打印检测到的深浅色模式。"""
    theme = uda.detect_theme()
    labels = {
        "dark": _text("深色 (Dark)", "dark"),
        "light": _text("浅色 (Light)", "light"),
        "unknown": _text("未知 (Unknown)", "unknown"),
    }
    label = labels.get(theme, theme)
    print(_text(f"[1/3] 系统主题模式: {label}", f"[1/3] system theme: {label}"))


def print_wallpaper(uda: Uda) -> None:
    """打印当前壁纸路径；未设置时给出说明。"""
    path = uda.get_wallpaper()
    if path is None:
        print(
            _text(
                "[2/3] 当前壁纸路径: 未设置或平台不支持读取",
                "[2/3] current wallpaper: unset or unsupported on this platform",
            )
        )
    else:
        print(
            _text(
                f"[2/3] 当前壁纸路径: {path}",
                f"[2/3] current wallpaper: {path}",
            )
        )


def demo_wakelock(uda: Uda) -> None:
    """申请常亮锁，保持 2 秒后释放。"""
    print(
        _text(
            f"[3/3] 申请防休眠常亮锁 (display)，保持 {WAKELOCK_HOLD_SECONDS:.0f} 秒 ...",
            f"[3/3] acquiring display wake lock, holding it for "
            f"{WAKELOCK_HOLD_SECONDS:.0f}s ...",
        )
    )
    handle = uda.wakelock_acquire("display", "UDA Python demo")
    print(_text(f"      已获取，句柄 = {handle}", f"      acquired, handle = {handle}"))

    # 分段睡眠，让"锁正在生效"的过程在输出中可见。
    deadline = time.monotonic() + WAKELOCK_HOLD_SECONDS
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(remaining, 0.5))

    uda.wakelock_release(handle)
    print(_text("      已释放常亮锁", "      wake lock released"))


def main() -> int:
    """运行演示，返回进程退出码。"""
    try:
        with Uda() as uda:
            print(_text("=== UDA Python 演示 ===", "=== UDA Python demo ==="))
            print_theme(uda)
            print_wallpaper(uda)
            demo_wakelock(uda)
            print(_text("=== 演示完成 ===", "=== demo finished ==="))
    except UdaError as error:
        # 平台能力缺失（例如无 D-Bus 会话）是可预期的失败，明确报告而非崩溃。
        print(
            _text(f"UDA 调用失败: {error}", f"UDA call failed: {error}"),
            file=sys.stderr,
        )
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
