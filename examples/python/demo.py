#!/usr/bin/env python3
"""UDA C-ABI 层 Python 演示。

演示三项核心能力：

1. 检测系统深浅色模式（Dark / Light）；
2. 读取当前壁纸路径；
3. 申请防休眠常亮锁，保持 2 秒后释放。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/demo.py

也可通过 ``UDA_LIBRARY`` 显式指定动态库路径。
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

# 使 `import uda` 在本脚本位于 examples/python/ 时仍然可用。
sys.path.insert(0, str(Path(__file__).resolve().parent))

from uda import Uda, UdaError  # noqa: E402

#: 演示中常亮锁的持有时长（秒）。
WAKELOCK_HOLD_SECONDS = 2.0


def print_theme(uda: Uda) -> None:
    """打印检测到的深浅色模式。"""
    theme = uda.detect_theme()
    label = {
        "dark": "深色 (Dark)",
        "light": "浅色 (Light)",
        "unknown": "未知 (Unknown)",
    }.get(theme, theme)
    print(f"[1/3] 系统主题模式: {label}")


def print_wallpaper(uda: Uda) -> None:
    """打印当前壁纸路径；未设置时给出说明。"""
    path = uda.get_wallpaper()
    if path is None:
        print("[2/3] 当前壁纸路径: 未设置或平台不支持读取")
    else:
        print(f"[2/3] 当前壁纸路径: {path}")


def demo_wakelock(uda: Uda) -> None:
    """申请常亮锁，保持 2 秒后释放。"""
    print(f"[3/3] 申请防休眠常亮锁 (display)，保持 {WAKELOCK_HOLD_SECONDS:.0f} 秒 ...")
    handle = uda.wakelock_acquire("display", "UDA Python demo")
    print(f"      已获取，句柄 = {handle}")

    # 分段睡眠，让"锁正在生效"的过程在输出中可见。
    deadline = time.monotonic() + WAKELOCK_HOLD_SECONDS
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(remaining, 0.5))

    uda.wakelock_release(handle)
    print("      已释放常亮锁")


def main() -> int:
    """运行演示，返回进程退出码。"""
    try:
        with Uda() as uda:
            print("=== UDA Python 演示 ===")
            print_theme(uda)
            print_wallpaper(uda)
            demo_wakelock(uda)
            print("=== 演示完成 ===")
    except UdaError as error:
        # 平台能力缺失（例如无 D-Bus 会话）是可预期的失败，明确报告而非崩溃。
        print(f"UDA 调用失败: {error}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
