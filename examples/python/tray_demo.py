#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""UDA C-ABI 层 Python 托盘演示。

在系统托盘创建一个标题为 **UDA Tray Demo** 的图标，并挂载一个右键菜单：

- **欢迎使用 UniDesktop** —— 普通文本项，点击后向控制终端打印一行状态；
- **开启深色模式同步** —— 复选框项，勾选状态变化时打印新旧值；
- 一条分隔线；
- **退出程序** —— 文本项，点击后结束事件循环并注销托盘图标。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/tray_demo.py

进程会一直存活并派发菜单事件，直到选择"退出程序"或按下 ``Ctrl+C``。
也可通过 ``UDA_LIBRARY`` 显式指定动态库路径。

实现说明：菜单回调由托盘**工作线程**回调（见 ``include/uda.h`` 的线程模型
说明），因此这里用 :class:`threading.Event` 做跨线程信号，主循环只负责休眠与
检测，绝不在回调线程里做阻塞操作。
"""

from __future__ import annotations

import sys
import threading
from pathlib import Path

# 使 `import uda` / `import _encoding` 在本脚本位于 examples/python/ 时仍可用。
sys.path.insert(0, str(Path(__file__).resolve().parent))

# 必须先建立编码护栏：否则 Windows cp1252 控制台打印中文会直接抛异常。
from _encoding import (  # noqa: E402
    console_supports_unicode,
    detect_language,
    force_utf8_output,
)
from _png import PngError, load_icon_rgba  # noqa: E402

force_utf8_output()

import time  # noqa: E402

from uda import TrayIcon, TrayMenu, Uda, UdaError  # noqa: E402

#: 仓库根目录。本脚本位于 ``examples/python/``，向上两级即仓库根；图标按仓库
#: 定位而不是按进程 CWD，这样从任何目录运行都能显示同一张图标。
_REPO_ROOT = Path(__file__).resolve().parent.parent.parent

#: 托盘图标标题。
TRAY_NAME = "UDA Tray Demo"
#: 托盘悬停提示。
TRAY_TOOLTIP = "UDA Tray Demo"
#: 图标源文件。
ICON_PATH = _REPO_ROOT / "icons" / "UniDesktop.png"
#: 提交给 shell 的图标最长边像素数。
#: ``docs/internals/tray_specs.md`` §2.6 规定托盘图标为 ``SM_CXSMICON``（16px
#: @96dpi），HiDPI 下常见 32px。原图是 1254x1254，直接提交会让
#: ``IconPixmap`` 往会话总线广播 6 MB 数据、Windows 端还得为整张位图建 DIB，
#: 因此先在这里降到两端都无需再缩放的大小；shell 仍会自行缩放到系统尺寸。
TRAY_ICON_MAX_EXTENT = 32
#: 主循环轮询间隔（秒）。无需更短：菜单事件由工作线程直接回调推送。
POLL_INTERVAL_SECONDS = 0.2

#: 控制台能否原样呈现中文；为 False 时全部输出降级为英文。
_UNICODE_OK = console_supports_unicode()

#: 界面语言；``en`` 时输出英文文案，其余输出中文文案。
_LANGUAGE = detect_language()


def _text(zh: str, en: str) -> str:
    """按语言探测结果返回中文或英文文案。"""
    return en if _LANGUAGE == "en" else zh


class TrayDemo:
    """托盘演示应用：一个图标、一份菜单、一个运行标志。

    Attributes:
        quit_event: 由"退出程序"菜单项置位，主循环据此退出。
        dark_sync_enabled: 复选框当前状态，由复选框回调维护。
    """

    def __init__(self, uda: Uda) -> None:
        self.uda = uda
        self.quit_event = threading.Event()
        self.dark_sync_enabled = False
        self.icon: TrayIcon | None = None

    # ------------------------------------------------------------------
    # 菜单回调（运行在托盘工作线程）
    # ------------------------------------------------------------------
    def on_greeting(self, item_id: int, user_data: object) -> None:
        """"欢迎使用 UniDesktop" 被点击。

        只做一次打印与标志更新：回调必须尽快返回，否则会阻塞后续菜单交互。
        """
        print(
            _text(
                f">>点击了菜单: 欢迎使用 UniDesktop！所有菜单项都能正常工作。 "
                f"(item_id={item_id})",
                f">>clicked menu row: Welcome to UniDesktop! Every row works. "
                f"(item_id={item_id})",
            ),
            flush=True,
        )

    def on_dark_sync(self, item_id: int, checked: bool, user_data: object) -> None:
        """"开启深色模式同步" 勾选状态变化。

        Args:
            item_id: 库分配的菜单行 id。
            checked: 翻转后的新状态，与托盘实际渲染一致。
            user_data: 注册回调时透传的 Python 对象。
        """
        previous = self.dark_sync_enabled
        self.dark_sync_enabled = checked
        print(
            _text(
                f">>点击了菜单: 深色模式同步已{'开启' if checked else '关闭'} "
                f"(item_id={item_id}, {previous} -> {checked})",
                f">>clicked menu row: dark-mode sync {'on' if checked else 'off'} "
                f"(item_id={item_id}, {previous} -> {checked})",
            ),
            flush=True,
        )

    def on_quit(self, item_id: int, user_data: object) -> None:
        """"退出程序" 被点击：通知主循环退出。"""
        print(
            _text(
                f">>点击了菜单: 退出程序 (item_id={item_id})，正在注销托盘 ...",
                f">>clicked menu row: quit (item_id={item_id}), unregistering ...",
            ),
            flush=True,
        )
        self.quit_event.set()

    # ------------------------------------------------------------------
    # 构建与运行
    # ------------------------------------------------------------------
    def build(self) -> TrayIcon:
        """创建图标与菜单并完成挂载。"""
        icon = self.uda.create_tray_icon(TRAY_NAME, TRAY_TOOLTIP)
        self.icon = icon

        menu = self.uda.create_tray_menu()
        # 用 lambda 把 item_id / user_data 暴露给调用方，同时保留本类方法作
        # 为实际处理逻辑。Python 可调用对象（lambda / 模块级函数 / 绑定方法）
        # 由 uda.py 的蹦床机制保活，无需在此处额外持有引用。
        menu.add_text(
            "欢迎使用 UniDesktop",
            callback=lambda item_id, user_data: self.on_greeting(item_id, user_data),
        )
        menu.add_checkbox(
            "开启深色模式同步",
            checked=False,
            callback=lambda item_id, checked, user_data: self.on_dark_sync(
                item_id, checked, user_data
            ),
        )
        menu.add_separator()
        menu.add_text(
            "退出程序",
            callback=lambda item_id, user_data: self.on_quit(item_id, user_data),
        )
        icon.set_menu(menu)
        self.apply_icon(icon)

        return icon

    def apply_icon(self, icon: TrayIcon) -> None:
        """把 :data:`ICON_PATH` 解码并铺到托盘图标上。

        解码成 RGBA 再走 :meth:`~uda.TrayIcon.set_icon_rgba`，而不是把文件路径
        直接交给 :meth:`~uda.TrayIcon.set_icon_path`，原因有二：

        1. Linux 后端把 ``TrayIconSource::Path`` 原样当作 **freedesktop 图标
           主题名**（``IconPayload::Name``，见
           ``crates/uda-platform-linux/src/tray.rs``），Windows 后端才按文件
           路径交给 ``LoadImageW``。仓库内的 PNG 路径在 Linux 上只会解析成一个
           不存在的主题名，托盘依旧是空的；RGBA 通道两端语义一致。
        2. ``IconPixmap`` 原样广播像素尺寸，1254x1254 的原图必须先降到
           :data:`TRAY_ICON_MAX_EXTENT` 以内。

        图标缺失或损坏只降级为一句警告：托盘的事件与菜单都不依赖图标，不该让
        整个演示因此退出。由 ``_png`` 按纯标准库解码，不引入第三方依赖。
        """
        try:
            width, height, rgba = load_icon_rgba(ICON_PATH, TRAY_ICON_MAX_EXTENT)
        except PngError as error:
            print(
                _text(
                    f"警告: 未能加载托盘图标 {ICON_PATH}: {error}",
                    f"warning: could not load the tray icon {ICON_PATH}: {error}",
                ),
                file=sys.stderr,
                flush=True,
            )
            return

        icon.set_icon_rgba(width, height, rgba)
        print(
            _text(
                f"托盘图标已设置: {ICON_PATH} ({width}x{height})",
                f"tray icon set: {ICON_PATH} ({width}x{height})",
            ),
            flush=True,
        )

    def run(self) -> None:
        """派发菜单事件，直到收到退出指令或键盘中断。"""
        print(
            _text(
                f"托盘图标已创建: {TRAY_NAME}",
                f"tray icon created: {TRAY_NAME}",
            ),
            flush=True,
        )
        print(
            _text(
                "右键点击托盘图标查看菜单；选择\"退出程序\"结束本程序。",
                'right-click the tray icon; choose "退出程序" to exit.',
            ),
            flush=True,
        )

        while not self.quit_event.is_set():
            # 刻意用「睡眠 + 轮询」而不是 `Event.wait(timeout)`：后者在等待中被
            # 信号（SIGTERM 转 KeyboardInterrupt）打断时，CPython 会在
            # `Condition.__exit__` 里释放未持有的锁，抛出
            # `RuntimeError: release unlocked lock`，把干净的 Ctrl+C 变成一张
            # 与业务无关的栈。纯睡眠没有锁状态可损坏，用户按 Ctrl+C 时只会看到
            # main() 里那条友好的提示。
            time.sleep(POLL_INTERVAL_SECONDS)


def main() -> int:
    """运行托盘演示，返回进程退出码。"""
    try:
        with Uda() as uda:
            demo = TrayDemo(uda)
            # 图标解码是纯 Python 的逐像素循环，原图较大时会占用一秒左右；
            # 先给一句进度提示，免得控制台静默让人误以为卡死。
            print(
                _text(
                    "正在创建托盘图标 ...",
                    "creating the tray icon ...",
                ),
                flush=True,
            )
            demo.build()
            try:
                demo.run()
            finally:
                # 无论因何种原因退出，都显式注销图标；`with Uda()` 的
                # release_all() 会兜底，但提前一步可让托盘立即消失。
                if demo.icon is not None:
                    demo.icon.destroy()
    except UdaError as error:
        # 平台能力缺失（例如无 D-Bus 会话、无 StatusNotifierItem 主机）属于
        # 可预期的失败：明确报告而不留下半个崩溃栈。
        print(
            _text(f"UDA 调用失败: {error}", f"UDA call failed: {error}"),
            file=sys.stderr,
        )
        return 1
    except KeyboardInterrupt:
        print(
            _text("\n收到中断信号，正在退出 ...", "\ninterrupted, exiting ..."),
        )
        return 0

    print(_text("=== 演示结束 ===", "=== demo finished ==="))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
