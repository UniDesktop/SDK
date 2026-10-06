"""UniDesktop API (UDA) 的 Python SDK。

一行 ``from uda import Uda`` 即可使用全部桌面能力::

    from uda import Uda, Theme

    with Uda() as uda:
        print(uda.theme)                    # "dark" / "light" / "unknown"
        uda.wallpaper = "~/Pictures/a.png"  # 设置壁纸
        uda.notify("标题", "正文内容")        # 发一条系统通知

**零依赖**：只用 Python 标准库（``ctypes`` + ``zlib``）。

**不泄漏底层细节**：调用方看不到 ``ctypes.byref``、``c_void_p``、裸指针或十六进制
状态码。所有指针出参、字符串内存释放、函数指针保活都封装在本模块内，失败时抛出带
诊断消息的 :class:`UdaError`。

**图标可以只给一个路径**：托盘图标可以直接传 ``.png`` 文件路径，SDK 内部会用
:mod:`_png` 读成 RGBA 再提交 —— Linux 的 ``StatusNotifierItem`` 把 ``Path`` 当作
freedesktop 图标主题名而不是文件路径，直接传 PNG 路径在 Linux 上不会显示任何东西。

动态库定位顺序：``UDA_LIBRARY`` 环境变量 > ``cargo metadata`` 报告的 target 目录 >
仓库内常见构建目录 > 系统动态库搜索路径。
"""

from __future__ import annotations

import ctypes
import json
import os
import shutil
import subprocess
import sys
from functools import lru_cache
from pathlib import Path
from typing import Any, Callable, Final

# Windows 控制台默认代码页（cp1252 等）编码不了中文，因此这里尽早把标准输出
# 切到 UTF-8；`_encoding` 与 `uda` 同目录，直接按模块导入。
sys.path.insert(0, str(Path(__file__).resolve().parent))

from _encoding import force_utf8_output  # noqa: E402

force_utf8_output()

from _png import PngError, load_icon_rgba  # noqa: E402

__all__ = [
    "Uda",
    "UdaError",
    "Theme",
    "FillMode",
    "WakeLockType",
    "TrayIcon",
    "TrayMenu",
]

# --------------------------------------------------------------------------
# 常量（与 include/uda.h 保持一致）
# --------------------------------------------------------------------------

#: 调用成功。
OK: Final[int] = 0
#: 空指针、非法 UTF-8 或未知枚举值。
ERR_INVALID_ARGUMENT: Final[int] = -1
#: 当前平台或会话不支持该特性。
ERR_NOT_SUPPORTED: Final[int] = -2
#: 环境检测失败。
ERR_DETECTION_FAILED: Final[int] = -3
#: I/O 错误。
ERR_IO: Final[int] = -4
#: 内部错误。
ERR_INTERNAL: Final[int] = -5
#: panic 在 FFI 边界被捕获（正常不应出现）。
ERR_PANIC: Final[int] = -6

#: 各平台的动态库文件名（与 ``crates/uda-ffi`` 的 ``cdylib`` 产物一致）。
_LIBRARY_FILENAME_BY_PLATFORM: Final[dict[str, str]] = {
    "linux": "libuda_ffi.so",
    "darwin": "libuda_ffi.dylib",
    "win32": "uda_ffi.dll",
}

#: 仓库内常见的构建目录（相对于本文件的 ``examples/python/``）。
_LIBRARY_CANDIDATES: Final[tuple[str, ...]] = (
    "../../target/debug/libuda_ffi.so",
    "../../target/release/libuda_ffi.so",
    "../../target/debug/uda_ffi.dylib",
    "../../target/release/uda_ffi.dylib",
    "../../target/debug/uda_ffi.dll",
    "../../target/release/uda_ffi.dll",
)

#: 调用 ``cargo metadata`` 查询真实 target 目录时的超时（秒）。
_CARGO_METADATA_TIMEOUT: Final[float] = 10.0

#: 提交给 shell 的托盘图标最长边像素数（``docs/internals/tray_specs.md`` §2.6）。
TRAY_ICON_MAX_EXTENT: Final[int] = 32


class UdaError(RuntimeError):
    """UDA 调用失败时抛出，携带状态码与诊断消息。"""

    def __init__(self, status: int, message: str) -> None:
        self.status = status
        self.message = message
        super().__init__(f"UDA error {status}: {message}")


class Theme:
    """主题名称常量。"""

    UNKNOWN: Final[str] = "unknown"
    DARK: Final[str] = "dark"
    LIGHT: Final[str] = "light"


class FillMode:
    """壁纸填充模式常量。"""

    CROP: Final[str] = "crop"
    FILL: Final[str] = "fill"
    FIT: Final[str] = "fit"
    STRETCH: Final[str] = "stretch"


class WakeLockType:
    """常亮锁类型常量。"""

    DISPLAY: Final[str] = "display"
    SYSTEM: Final[str] = "system"


class MediaCommand:
    """媒体播控指令常量。"""

    PLAY: Final[str] = "play"
    PAUSE: Final[str] = "pause"
    TOGGLE: Final[str] = "toggle"
    NEXT: Final[str] = "next"
    PREVIOUS: Final[str] = "previous"
    STOP: Final[str] = "stop"


class PlaybackStatus:
    """播放状态常量。

    ``UNKNOWN`` 同时表示"没有播放器在运行"和"状态无法判定"，两者都不是错
    误；切勿把它渲染成"已暂停"。
    """

    PLAYING: Final[str] = "playing"
    PAUSED: Final[str] = "paused"
    STOPPED: Final[str] = "stopped"
    UNKNOWN: Final[str] = "unknown"


class SessionAction:
    """会话与电源动作常量（``uda.session`` 命名空间的动作名）。

    仅 ``LOCK`` 可以安全地自动化；其余五个会结束用户会话或停止机器，必须由宿
    主应用先取得用户**显式确认**再调用。
    """

    LOCK: Final[str] = "lock"
    LOGOUT: Final[str] = "logout"
    SUSPEND: Final[str] = "suspend"
    HIBERNATE: Final[str] = "hibernate"
    REBOOT: Final[str] = "reboot"
    SHUTDOWN: Final[str] = "shutdown"


class SessionCapability:
    """会话能力位常量，与 ``include/uda.h`` 的 UDA_SESSION_CAP_* 一致。

    能力位表达"代码路径存在"，**不是**"当前账户被允许"：关掉休眠的机器仍置位
    ``HIBERNATE``，真正拒绝发生在调用时（抛 :class:`UdaError` 状态码 -2）。
    """

    MANAGEMENT: Final[int] = 0x00010000
    LOCK: Final[int] = 0x00020000
    LOGOUT: Final[int] = 0x00040000
    SUSPEND: Final[int] = 0x00080000
    HIBERNATE: Final[int] = 0x00100000
    REBOOT: Final[int] = 0x00200000
    SHUTDOWN: Final[int] = 0x00400000


_FILL_CODES: Final[dict[str, int]] = {
    FillMode.CROP: 0,
    FillMode.FILL: 1,
    FillMode.FIT: 2,
    FillMode.STRETCH: 3,
}

_WAKELOCK_CODES: Final[dict[str, int]] = {
    WakeLockType.DISPLAY: 0,
    WakeLockType.SYSTEM: 1,
}

#: 指令名 -> C-ABI 码，与 ``include/uda.h`` 的 UDA_MEDIA_CMD_* 一致。
_MEDIA_COMMAND_CODES: Final[dict[str, int]] = {
    MediaCommand.PLAY: 0,
    MediaCommand.PAUSE: 1,
    MediaCommand.TOGGLE: 2,
    MediaCommand.NEXT: 3,
    MediaCommand.PREVIOUS: 4,
    MediaCommand.STOP: 5,
}

#: C-ABI 状态码 -> 状态名，与 ``include/uda.h`` 的 UDA_MEDIA_* 一致。
_MEDIA_STATUS_NAMES: Final[dict[int, str]] = {
    0: PlaybackStatus.PLAYING,
    1: PlaybackStatus.PAUSED,
    2: PlaybackStatus.STOPPED,
    3: PlaybackStatus.UNKNOWN,
}

#: 会话动作名 -> C-ABI 函数名后缀，用于 ``uda_session_*``。
_SESSION_ACTIONS: Final[dict[str, str]] = {
    SessionAction.LOCK: "lock",
    SessionAction.LOGOUT: "logout",
    SessionAction.SUSPEND: "suspend",
    SessionAction.HIBERNATE: "hibernate",
    SessionAction.REBOOT: "reboot",
    SessionAction.SHUTDOWN: "shutdown",
}

#: 会话动作名 -> 能力位，便于调用方在不查询掩码的情况下做单点判断。
_SESSION_ACTION_CAPABILITY: Final[dict[str, int]] = {
    SessionAction.LOCK: SessionCapability.LOCK,
    SessionAction.LOGOUT: SessionCapability.LOGOUT,
    SessionAction.SUSPEND: SessionCapability.SUSPEND,
    SessionAction.HIBERNATE: SessionCapability.HIBERNATE,
    SessionAction.REBOOT: SessionCapability.REBOOT,
    SessionAction.SHUTDOWN: SessionCapability.SHUTDOWN,
}

_THEME_NAMES: Final[dict[int, str]] = {
    0: Theme.UNKNOWN,
    1: Theme.DARK,
    2: Theme.LIGHT,
}

#: 托盘菜单文本项回调签名：``(item_id: int, user_data) -> None``。
TrayTextCallback = Callable[[int, Any], None]
#: 托盘菜单复选框项回调签名：``(item_id: int, checked: bool, user_data) -> None``。
TrayCheckboxCallback = Callable[[int, bool, Any], None]


@lru_cache(maxsize=1)
def _cargo_target_directory() -> Path | None:
    """通过 ``cargo metadata`` 查询真实的 target 目录。

    开发者可以通过 ``.cargo/config.toml``、``CARGO_TARGET_DIR`` 或 workspace
    外置缓存把产物放到 ``./target`` 之外；此时硬编码的相对路径会失效。直接询问
    Cargo 是唯一可靠的定位方式。查询失败（未安装 cargo、超时、非 JSON 输出）时
    返回 ``None``，由调用方回落到其它候选路径。
    """
    cargo = shutil.which("cargo")
    if cargo is None:
        return None

    here = Path(__file__).resolve().parent
    for root in (here.parent.parent, *here.parents):
        if (root / "Cargo.toml").is_file():
            break
    else:
        return None

    try:
        completed = subprocess.run(  # noqa: S603 - 参数固定，无 shell 注入面
            [cargo, "metadata", "--no-deps", "--format-version", "1"],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=_CARGO_METADATA_TIMEOUT,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None

    if completed.returncode != 0:
        return None

    try:
        metadata = json.loads(completed.stdout)
    except json.JSONDecodeError:
        return None

    target = metadata.get("target_directory")
    return Path(target).expanduser() if isinstance(target, str) else None


def _candidate_paths() -> list[Path]:
    """按优先级返回动态库候选路径。"""
    candidates: list[Path] = []

    override = os.environ.get("UDA_LIBRARY")
    if override:
        candidates.append(Path(override).expanduser())

    filename = _LIBRARY_FILENAME_BY_PLATFORM.get(sys.platform, "libuda_ffi.so")
    target_directory = _cargo_target_directory()
    if target_directory is not None:
        for profile in ("debug", "release"):
            candidates.append(target_directory / profile / filename)

    here = Path(__file__).resolve().parent
    for relative in _LIBRARY_CANDIDATES:
        candidates.append((here / relative).resolve())

    return candidates


def _load_library() -> ctypes.CDLL:
    """加载 UDA 动态库，失败时抛出带排查建议的 :class:`UdaError`。

    找不到库是最常见的使用问题，因此这里给出明确的构建提示，而不是让
    ``OSError: cannot open shared object file`` 单独暴露给调用方。
    """
    errors: list[str] = []

    for candidate in _candidate_paths():
        if candidate.is_file():
            try:
                return ctypes.CDLL(str(candidate))
            except OSError as exc:  # 库存在但加载失败（依赖缺失等）
                errors.append(f"{candidate}: {exc}")

    # 回落到系统搜索路径，便于库已安装到系统的场景。
    try:
        return ctypes.CDLL(_LIBRARY_FILENAME_BY_PLATFORM.get(sys.platform, "libuda_ffi.so"))
    except OSError as exc:
        errors.append(f"(system path): {exc}")

    raise UdaError(
        ERR_NOT_SUPPORTED,
        "无法加载 libuda_ffi；请先在仓库根目录执行 `cargo build -p uda-ffi`，"
        f"或设置 UDA_LIBRARY 指向动态库。已尝试：{'; '.join(errors) or '无候选路径'}",
    )


class _Int32Slot:
    """一个可写的 ``int32_t`` 出参槽位。

    ctypes 的 ``byref`` 必须被调用方显式写出，这会把"指针"这个概念泄漏到业务
    代码里。把槽位包成对象后，SDK 内部调用，业务侧只见 :meth:`value`。
    """

    __slots__ = ("_slot",)

    def __init__(self) -> None:
        self._slot = ctypes.c_int32(0)

    @property
    def value(self) -> int:
        """读回写入的整数值。"""
        return int(self._slot.value)


class _UInt64Slot:
    """一个可写的 ``uint64_t`` 出参槽位（句柄、菜单行 id）。"""

    __slots__ = ("_slot",)

    def __init__(self) -> None:
        self._slot = ctypes.c_uint64(0)

    @property
    def value(self) -> int:
        """读回写入的句柄值。"""
        return int(self._slot.value)


class _UInt32Slot:
    """一个可写的 ``uint32_t`` 出参槽位（通知 id）。"""

    __slots__ = ("_slot",)

    def __init__(self) -> None:
        self._slot = ctypes.c_uint32(0)

    @property
    def value(self) -> int:
        """读回写入的通知 id。"""
        return int(self._slot.value)


class _RgbaSlot:
    """一段可写的四字节出参（强调色的 R, G, B, A）。"""

    __slots__ = ("_buffer",)

    def __init__(self) -> None:
        self._buffer = (ctypes.c_uint8 * 4)(0, 0, 0, 0)

    @property
    def value(self) -> tuple[int, int, int, int]:
        """读回 ``(r, g, b, a)``；未写入时是 ``(0, 0, 0, 0)``。"""
        return tuple(self._buffer)  # type: ignore[return-value]


class Uda:
    """UniDesktop API 入口。

    使用 ``with`` 语句可确保退出前释放本对象持有的全部常亮锁与托盘资源::

        with Uda() as uda:
            print(uda.theme)

    Args:
        library_path: 显式指定动态库路径；为 ``None`` 时按默认顺序查找。
    """

    def __init__(self, library_path: str | os.PathLike[str] | None = None) -> None:
        if library_path is not None:
            try:
                self._lib = ctypes.CDLL(str(library_path))
            except OSError as exc:
                raise UdaError(
                    ERR_NOT_SUPPORTED, f"无法加载 {library_path}: {exc}"
                ) from exc
        else:
            self._lib = _load_library()

        self._declare_prototypes()
        #: 本对象持有、尚未释放的常亮锁句柄。
        self._handles: list[int] = []
        #: 本对象创建、尚未销毁的托盘图标。
        self._tray_icons: list["TrayIcon"] = []
        #: 本对象创建、尚未销毁的托盘菜单。
        self._tray_menus: list["TrayMenu"] = []
        #: 保活所有已注册的 C 回调蹦床。
        #:
        #: ctypes 的 ``CFUNCTYPE`` 实例必须被强引用：一旦被 GC，底层函数指针
        #: 悬空，而库端仍持有该地址，托盘线程回调时就会跳进已释放的内存。
        self._trampolines: list[Any] = []

    # ------------------------------------------------------------------
    # ctypes 原型声明（内部实现，业务代码不应触碰）
    # ------------------------------------------------------------------
    def _declare_prototypes(self) -> None:
        """声明每个导出函数的参数与返回类型。

        显式声明 ``argtypes`` 是内存安全的关键：否则 ctypes 会把 Python 整数
        按 C ``int`` 传入，64 位指针在 Windows 上会被截断。
        """
        c_int32_p = ctypes.POINTER(ctypes.c_int32)
        c_char_p_p = ctypes.POINTER(ctypes.c_char_p)
        c_uint64_p = ctypes.POINTER(ctypes.c_uint64)
        c_uint32_p = ctypes.POINTER(ctypes.c_uint32)

        self._lib.uda_detect_theme.argtypes = [c_int32_p]
        self._lib.uda_detect_theme.restype = ctypes.c_int32

        self._lib.uda_set_wallpaper.argtypes = [ctypes.c_char_p, ctypes.c_int32]
        self._lib.uda_set_wallpaper.restype = ctypes.c_int32

        self._lib.uda_get_wallpaper.argtypes = [c_char_p_p]
        self._lib.uda_get_wallpaper.restype = ctypes.c_int32

        self._lib.uda_free_string.argtypes = [ctypes.c_char_p]
        self._lib.uda_free_string.restype = None

        self._lib.uda_wakelock_acquire.argtypes = [
            ctypes.c_int32,
            ctypes.c_char_p,
            c_uint64_p,
        ]
        self._lib.uda_wakelock_acquire.restype = ctypes.c_int32

        self._lib.uda_wakelock_release.argtypes = [ctypes.c_uint64]
        self._lib.uda_wakelock_release.restype = ctypes.c_int32

        self._lib.uda_last_error_message.argtypes = []
        self._lib.uda_last_error_message.restype = ctypes.c_char_p

        self._lib.uda_status_message.argtypes = [ctypes.c_int32]
        self._lib.uda_status_message.restype = ctypes.c_char_p

        # ---- 通知 ----
        # `uda_notify(app_name, title, body, icon, actions, out_id)`：
        # app_name 是 Windows 的 toast 身份（AppUserModelID），未打包进程靠它
        # 才能弹 toast。
        self._lib.uda_notify.argtypes = [
            ctypes.c_char_p,
            ctypes.c_char_p,
            ctypes.c_char_p,
            ctypes.c_char_p,
            ctypes.c_char_p,
            c_uint32_p,
        ]
        self._lib.uda_notify.restype = ctypes.c_int32

        # ---- 强调色 ----
        self._lib.uda_get_accent_color.argtypes = [ctypes.POINTER(ctypes.c_uint8)]
        self._lib.uda_get_accent_color.restype = ctypes.c_int32

        # ---- 媒体播控（Media） ----
        # 三个字符串出参各自独立分配，业务侧统一由 :class:`_MediaTrack` 释放。
        self._lib.uda_media_get_metadata.argtypes = [
            c_char_p_p,
            c_char_p_p,
            c_char_p_p,
            c_uint64_p,
            c_uint64_p,
        ]
        self._lib.uda_media_get_metadata.restype = ctypes.c_int32

        self._lib.uda_media_get_status.argtypes = [c_int32_p]
        self._lib.uda_media_get_status.restype = ctypes.c_int32

        self._lib.uda_media_send_command.argtypes = [ctypes.c_int32]
        self._lib.uda_media_send_command.restype = ctypes.c_int32

        # ---- 会话与电源（Session） ----
        # 能力查询写一个 uint32_t 掩码；六个动作无参无出参，只回状态码。
        self._lib.uda_session_capabilities.argtypes = [ctypes.POINTER(ctypes.c_uint32)]
        self._lib.uda_session_capabilities.restype = ctypes.c_int32

        for _suffix in _SESSION_ACTIONS.values():
            _function = getattr(self._lib, f"uda_session_{_suffix}")
            _function.argtypes = []
            _function.restype = ctypes.c_int32
        del _suffix, _function

        # ---- 托盘（Tray） ----
        self._lib.uda_tray_create.argtypes = [
            ctypes.c_char_p,
            ctypes.c_char_p,
            c_uint64_p,
        ]
        self._lib.uda_tray_create.restype = ctypes.c_int32

        self._lib.uda_tray_set_tooltip.argtypes = [ctypes.c_uint64, ctypes.c_char_p]
        self._lib.uda_tray_set_tooltip.restype = ctypes.c_int32

        self._lib.uda_tray_set_icon_path.argtypes = [ctypes.c_uint64, ctypes.c_char_p]
        self._lib.uda_tray_set_icon_path.restype = ctypes.c_int32

        self._lib.uda_tray_set_icon_rgba.argtypes = [
            ctypes.c_uint64,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.POINTER(ctypes.c_uint8),
            ctypes.c_size_t,
        ]
        self._lib.uda_tray_set_icon_rgba.restype = ctypes.c_int32

        self._lib.uda_tray_set_visible.argtypes = [ctypes.c_uint64, ctypes.c_int32]
        self._lib.uda_tray_set_visible.restype = ctypes.c_int32

        self._lib.uda_tray_destroy.argtypes = [ctypes.c_uint64]
        self._lib.uda_tray_destroy.restype = ctypes.c_int32

        self._lib.uda_tray_menu_create.argtypes = [c_uint64_p]
        self._lib.uda_tray_menu_create.restype = ctypes.c_int32

        # 回调签名与 include/uda.h 的 typedef 一致；`None` 表示不注册回调，
        # ctypes 会按空指针传入，库端即得到"静默行"。
        self._TextCallback = ctypes.CFUNCTYPE(
            None, ctypes.c_uint64, ctypes.c_void_p
        )
        self._CheckboxCallback = ctypes.CFUNCTYPE(
            None, ctypes.c_uint64, ctypes.c_int32, ctypes.c_void_p
        )

        # 注意：argtype 必须直接写 ``CFUNCTYPE`` 本体，**不能**写
        # ``ctypes.POINTER(self._TextCallback)``。Rust 侧签名是
        # ``Option<extern "C" fn(..)>``，即一个裸函数指针；而
        # ``LP_CFunctionType`` 要求的是"指向函数指针的指针"，ctypes 在转换时
        # 会交出 CFUNCTYPE 实例对象在 Python 堆上的地址而非函数入口本身，
        # Rust 端把它当函数指针调用就会跳到 Python 堆上并直接段错误。
        self._lib.uda_tray_menu_add_text.argtypes = [
            ctypes.c_uint64,
            ctypes.c_char_p,
            self._TextCallback,
            ctypes.c_void_p,
            c_uint64_p,
        ]
        self._lib.uda_tray_menu_add_text.restype = ctypes.c_int32

        self._lib.uda_tray_menu_add_separator.argtypes = [ctypes.c_uint64]
        self._lib.uda_tray_menu_add_separator.restype = ctypes.c_int32

        # 同上：直接传 CFUNCTYPE 本体，理由见 uda_tray_menu_add_text 处的说明。
        self._lib.uda_tray_menu_add_checkbox.argtypes = [
            ctypes.c_uint64,
            ctypes.c_char_p,
            ctypes.c_int32,
            self._CheckboxCallback,
            ctypes.c_void_p,
            c_uint64_p,
        ]
        self._lib.uda_tray_menu_add_checkbox.restype = ctypes.c_int32

        self._lib.uda_tray_set_menu.argtypes = [ctypes.c_uint64, ctypes.c_uint64]
        self._lib.uda_tray_set_menu.restype = ctypes.c_int32

        self._lib.uda_tray_menu_destroy.argtypes = [ctypes.c_uint64]
        self._lib.uda_tray_menu_destroy.restype = ctypes.c_int32

    # ------------------------------------------------------------------
    # 内部辅助
    # ------------------------------------------------------------------
    def _check(self, status: int, action: str) -> None:
        """状态码非 0 时读取诊断消息并抛出 :class:`UdaError`。"""
        if status == OK:
            return
        raise UdaError(status, self._last_error_message(action))

    def _last_error_message(self, action: str) -> str:
        """读取库记录的失败原因，读取失败时退回状态码描述。"""
        raw = self._lib.uda_last_error_message()
        if raw:
            return raw.decode("utf-8", errors="replace")
        # 库未记录消息（或记录失败）时，用静态描述兜底。
        raw = self._lib.uda_status_message(0)
        if raw:
            return f"{action} 失败（{raw.decode('utf-8', errors='replace')}）"
        return f"{action} 失败"

    # ------------------------------------------------------------------
    # 外观
    # ------------------------------------------------------------------
    @property
    def theme(self) -> str:
        """系统深浅色：``"dark"`` / ``"light"`` / ``"unknown"``。

        示例::

            print(uda.theme)
        """
        slot = _Int32Slot()
        self._check(
            self._lib.uda_detect_theme(ctypes.byref(slot._slot)), "detect_theme"
        )
        return _THEME_NAMES.get(slot.value, Theme.UNKNOWN)

    @property
    def accent_color(self) -> tuple[int, int, int, int] | None:
        """系统强调色，返回 ``(r, g, b, a)``；平台不支持时为 ``None``。

        示例::

            color = uda.accent_color
            if color:
                print(f"#{color[0]:02x}{color[1]:02x}{color[2]:02x}")
        """
        slot = _RgbaSlot()
        self._check(
            self._lib.uda_get_accent_color(ctypes.cast(slot._buffer, ctypes.POINTER(ctypes.c_uint8))),
            "get_accent_color",
        )
        # 平台不暴露强调色时（多数 Linux 桌面）库不会写入，四字节保持 0。
        return slot.value if any(slot.value) else None

    # ------------------------------------------------------------------
    # 媒体播控（Media）
    # ------------------------------------------------------------------
    @property
    def media(self) -> "_MediaController":
        """媒体播控入口。

        返回一个绑定到本 :class:`Uda` 实例的控制器，业务侧通过它查询正在播放
        的曲目并发送播控指令::

            track = uda.media.now_playing
            if track:
                print(f"{track.title} - {track.artist}")
            uda.media.play_pause()
        """
        return _MediaController(self)

    # ------------------------------------------------------------------
    # 会话与电源（Session）
    # ------------------------------------------------------------------
    @property
    def session(self) -> "_SessionController":
        """会话与电源生命周期入口。

        提供六个动作（:attr:`_SessionController.lock`、
        :attr:`_SessionController.logout`、:attr:`_SessionController.suspend`、
        :attr:`_SessionController.hibernate`、:attr:`_SessionController.reboot`、
        :attr:`_SessionController.shutdown`）与一个能力查询
        :attr:`_SessionController.capabilities`。

        **除锁屏外，其余五个动作会结束用户会话或停止机器**，必须由宿主应用先
        取得用户显式确认后再调用::

            caps = uda.session.capabilities
            if caps["shutdown"]:
                uda.session.shutdown()   # 仅在用户确认之后！
        """
        return _SessionController(self)

    # ------------------------------------------------------------------
    # 壁纸
    # ------------------------------------------------------------------
    @property
    def wallpaper(self) -> str | None:
        """当前壁纸路径；未设置或平台不支持时返回 ``None``。"""
        out = ctypes.c_char_p()
        self._check(
            self._lib.uda_get_wallpaper(ctypes.byref(out)), "get_wallpaper"
        )
        if not out.value:
            return None
        try:
            return out.value.decode("utf-8", errors="replace")
        finally:
            # 无论解码是否成功都必须释放，避免泄漏。
            self._lib.uda_free_string(out)

    @wallpaper.setter
    def wallpaper(self, path: str | os.PathLike[str]) -> None:
        """设置桌面壁纸。

        Args:
            path: 图片文件路径。
        """
        self.set_wallpaper(path)

    def set_wallpaper(
        self,
        path: str | os.PathLike[str],
        fill_mode: str = FillMode.FILL,
    ) -> None:
        """设置桌面壁纸。

        Args:
            path: 图片文件路径。
            fill_mode: ``"crop"`` / ``"fill"`` / ``"fit"`` / ``"stretch"``。

        示例::

            uda.set_wallpaper("~/Pictures/a.png", FillMode.FIT)
        """
        try:
            code = _FILL_CODES[fill_mode]
        except KeyError:
            raise UdaError(
                ERR_INVALID_ARGUMENT,
                f"未知填充模式 {fill_mode!r}；可选：{sorted(_FILL_CODES)}",
            ) from None

        encoded = os.fspath(path).encode("utf-8")
        status = self._lib.uda_set_wallpaper(encoded, code)
        self._check(status, f"set_wallpaper({os.fspath(path)!r})")

    # ------------------------------------------------------------------
    # 通知
    # ------------------------------------------------------------------
    def notify(
        self,
        title: str,
        body: str = "",
        icon: str = "",
        actions: dict[str, str] | None = None,
        app_name: str = "",
    ) -> int:
        """发送一条系统通知，返回服务器分配的 id。

        Args:
            title: 单行标题。
            body: 多行正文，可为空。
            icon: 图标路径或 URI，可为空。
            actions: 按钮表，键为动作标识、值为按钮文字，如
                ``{"open": "查看详情"}``。Windows 上 toast *按钮*需要 MSIX 打包
                身份，因此该参数在 Windows 上不呈现按钮（toast 本身正常显示）。
            app_name: 发送方应用名。在 Windows 上它就是 toast 的
                AppUserModelID，而未打包进程没有该身份；UDA 会在第一次弹 toast
                前用它注册进程的显式 AUMID。留空则使用通用身份
                ``"UniDesktop.Notification"``。

        示例::

            uda.notify("下载完成", "report.pdf 已保存到 ~/Downloads")
            uda.notify("更新可用", "v0.2.1 已发布", actions={"open": "查看详情"},
                       app_name="我的应用")
        """
        flat = ""
        if actions:
            flat = "\n".join(f"{key}\n{label}" for key, label in actions.items())

        slot = _UInt32Slot()
        status = self._lib.uda_notify(
            app_name.encode("utf-8"),
            title.encode("utf-8"),
            body.encode("utf-8"),
            icon.encode("utf-8"),
            flat.encode("utf-8"),
            ctypes.byref(slot._slot),
        )
        self._check(status, f"notify({title!r})")
        return slot.value

    # ------------------------------------------------------------------
    # 常亮锁
    # ------------------------------------------------------------------
    def wakelock(
        self,
        lock_type: str = WakeLockType.DISPLAY,
        reason: str = "UDA",
    ) -> "WakeLock":
        """申请防休眠常亮锁，返回可作 with 语句使用的 :class:`WakeLock`。

        退出 ``with`` 块（或调用 :meth:`WakeLock.release`）时自动释放，因此
        不必担心忘记释放导致 ``systemd-inhibit`` 子进程残留::

            with uda.wakelock() as lock:
                ...          # 这三秒屏幕不会休眠

        Args:
            lock_type: ``"display"`` 或 ``"system"``。
            reason: 诊断用描述文本。
        """
        return WakeLock(self, lock_type, reason)

    def _acquire_wakelock(self, lock_type: str, reason: str) -> int:
        """底层申请；业务代码请用 :meth:`wakelock`。"""
        try:
            code = _WAKELOCK_CODES[lock_type]
        except KeyError:
            raise UdaError(
                ERR_INVALID_ARGUMENT,
                f"未知常亮锁类型 {lock_type!r}；可选：{sorted(_WAKELOCK_CODES)}",
            ) from None

        slot = _UInt64Slot()
        status = self._lib.uda_wakelock_acquire(
            code, reason.encode("utf-8"), ctypes.byref(slot._slot)
        )
        self._check(status, f"wakelock_acquire({lock_type!r})")

        handle = slot.value
        if handle == 0:
            # 库保证成功时返回非零句柄；为 0 说明契约被破坏。
            raise UdaError(ERR_INTERNAL, "wakelock_acquire 返回了空句柄")
        self._handles.append(handle)
        return handle

    def _release_wakelock(self, handle: int) -> None:
        """底层释放；业务代码请用 :meth:`WakeLock.release`。"""
        status = self._lib.uda_wakelock_release(handle)
        self._check(status, f"wakelock_release({handle})")
        if handle in self._handles:
            self._handles.remove(handle)

    # ------------------------------------------------------------------
    # 托盘（Tray）
    # ------------------------------------------------------------------
    def create_tray_icon(
        self, name: str = "UDA", tooltip: str = "", icon: str | os.PathLike[str] = ""
    ) -> "TrayIcon":
        """创建托盘图标。

        Args:
            name: 应用名（用于 D-Bus 总线名 / 窗口类注册）。
            tooltip: 悬停提示文本，可为空。
            icon: 图标文件路径（``.png`` 等），SDK 会自动解码成 RGBA 后提交；
                传空串则创建时无图标，可稍后设置。

        Returns:
            :class:`TrayIcon` 实例，由调用方持有；销毁它才会从系统托盘注销。

        示例::

            icon = uda.create_tray_icon("我的应用", "提示文本", "icons/logo.png")
        """
        tray_icon = TrayIcon(self, name, tooltip)
        self._tray_icons.append(tray_icon)
        if icon:
            tray_icon.icon = icon
        return tray_icon

    def create_tray_menu(self) -> "TrayMenu":
        """创建空的托盘右键菜单。

        Returns:
            :class:`TrayMenu` 实例。
        """
        menu = TrayMenu(self)
        self._tray_menus.append(menu)
        return menu

    def _register_tray_icon(self, icon: "TrayIcon") -> None:
        """记录托盘图标，便于 ``release_all`` 统一清理。"""
        if icon not in self._tray_icons:
            self._tray_icons.append(icon)

    def _register_tray_menu(self, menu: "TrayMenu") -> None:
        """记录菜单，便于 ``release_all`` 统一清理。"""
        if menu not in self._tray_menus:
            self._tray_menus.append(menu)

    def _unregister_tray_icon(self, icon: "TrayIcon") -> None:
        if icon in self._tray_icons:
            self._tray_icons.remove(icon)

    def _unregister_tray_menu(self, menu: "TrayMenu") -> None:
        if menu in self._tray_menus:
            self._tray_menus.remove(menu)

    def release_all(self) -> None:
        """释放本对象持有的全部常亮锁与托盘资源。

        单个资源销毁失败不会阻断其余资源的释放；托盘图标销毁时会自动从
        系统托盘注销。重复调用是安全的。
        """
        for handle in list(self._handles):
            try:
                self._release_wakelock(handle)
            except UdaError:
                # 句柄已失效时无需再报错，调用方可按需查询。
                pass

        for icon in list(self._tray_icons):
            try:
                icon.destroy()
            except Exception:  # noqa: BLE001 - 逐一清理，互不阻断
                pass

        for menu in list(self._tray_menus):
            try:
                menu.destroy()
            except Exception:  # noqa: BLE001 - 逐一清理，互不阻断
                pass

        self._trampolines.clear()

    def __enter__(self) -> "Uda":
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        self.release_all()

    def __del__(self) -> None:  # pragma: no cover - 仅在忘记释放时兜底
        try:
            self.release_all()
        except Exception:
            # 解释器关闭时属性可能已被回收，静默忽略。
            pass


def _decode_or_empty(pointer: ctypes.c_char_p) -> str:
    """把库返回的 ``char *`` 解成 Python 字符串；``NULL`` 视为空串。

    媒体元数据的每个字段都是独立分配的可空指针：播放器没发布该字段（例如电台
    流没有专辑）时是 ``NULL``，而不是空字符串。业务侧判断 ``if track.album``
    即可，不必关心指针是否为空。
    """
    if not pointer.value:
        return ""
    return pointer.value.decode("utf-8", errors="replace")


class MediaTrack:
    """一条"正在播放"快照。

    所有字段都是纯 Python 值，不持有任何 C 端资源；播放器的 C 字符串已由
    :class:`_MediaController` 解码并释放。
    """

    __slots__ = ("title", "artist", "album", "duration_ms", "position_ms")

    def __init__(
        self,
        title: str,
        artist: str,
        album: str,
        duration_ms: int,
        position_ms: int,
    ) -> None:
        #: 曲名；播放器未发布时为空串。
        self.title = title
        #: 艺人；播放器发布多位时已用 ``", "`` 连接。
        self.artist = artist
        #: 专辑名。
        self.album = album
        #: 曲目时长（毫秒）；直播流等未知时长为 0。
        self.duration_ms = duration_ms
        #: 当前播放位置（毫秒）；后端无法上报时为 0。
        self.position_ms = position_ms

    def __repr__(self) -> str:
        return (
            f"MediaTrack(title={self.title!r}, artist={self.artist!r}, "
            f"album={self.album!r}, duration_ms={self.duration_ms})"
        )


class _MediaController:
    """媒体播控命名空间（``uda.media``）。

    把三个 C 导出函数与"字符串出参由库分配、需释放"的细节收在一处：调用方
    只见到 :class:`MediaTrack`、状态名常量与 :class:`MediaCommand` 指令名。
    """

    __slots__ = ("_uda",)

    def __init__(self, uda: "Uda") -> None:
        self._uda = uda

    @property
    def now_playing(self) -> MediaTrack | None:
        """当前播放的曲目快照；没有播放器运行时返回 ``None``。

        示例::

            track = uda.media.now_playing
            if track is None:
                print("当前没有播放器")
            else:
                print(f"{track.title} - {track.artist}")
        """
        title = ctypes.c_char_p()
        artist = ctypes.c_char_p()
        album = ctypes.c_char_p()
        duration = _UInt64Slot()
        position = _UInt64Slot()

        status = self._uda._lib.uda_media_get_metadata(
            ctypes.byref(title),
            ctypes.byref(artist),
            ctypes.byref(album),
            ctypes.byref(duration._slot),
            ctypes.byref(position._slot),
        )
        self._uda._check(status, "media_get_metadata")

        try:
            title_text = _decode_or_empty(title)
            artist_text = _decode_or_empty(artist)
            album_text = _decode_or_empty(album)

            # 库端已把"元数据全空"归一成与"无播放器"完全相同的返回值（三个
            # NULL + 时长 0），SDK 必须同样归一成 None：否则调用方拿到一个空壳
            # 对象，无法与"没有播放器"区分，示例里就会打印出一堆"(未发布)"。
            if (
                not title_text
                and not artist_text
                and not album_text
                and duration.value == 0
            ):
                return None

            return MediaTrack(
                title=title_text,
                artist=artist_text,
                album=album_text,
                duration_ms=duration.value,
                position_ms=position.value,
            )
        finally:
            # 无论解码是否成功都要释放；uda_free_string(NULL) 是空操作。
            self._uda._lib.uda_free_string(title)
            self._uda._lib.uda_free_string(artist)
            self._uda._lib.uda_free_string(album)

    @property
    def status(self) -> str:
        """当前播放状态，返回 :class:`PlaybackStatus` 常量之一。

        ``unknown`` 同时覆盖"没有播放器"与"状态无法判定"，均非错误。
        """
        slot = _Int32Slot()
        self._uda._check(
            self._uda._lib.uda_media_get_status(ctypes.byref(slot._slot)),
            "media_get_status",
        )
        return _MEDIA_STATUS_NAMES.get(slot.value, PlaybackStatus.UNKNOWN)

    def send(self, command: str) -> None:
        """发送一条播控指令。

        Args:
            command: :class:`MediaCommand` 常量之一。

        Raises:
            UdaError: 指令名无法识别（状态码 -1），或没有播放器可接收、播放器
                拒绝执行（状态码 -2）。
        """
        try:
            code = _MEDIA_COMMAND_CODES[command]
        except KeyError:
            known = "、".join(sorted(_MEDIA_COMMAND_CODES))
            raise UdaError(
                ERR_INVALID_ARGUMENT,
                f"未知的播控指令 {command!r}；可用指令：{known}",
            ) from None

        self._uda._check(
            self._uda._lib.uda_media_send_command(code),
            f"media_send_command({command})",
        )

    def play(self) -> None:
        """开始播放。"""
        self.send(MediaCommand.PLAY)

    def pause(self) -> None:
        """暂停播放。"""
        self.send(MediaCommand.PAUSE)

    def play_pause(self) -> None:
        """在播放与暂停之间切换。"""
        self.send(MediaCommand.TOGGLE)

    def next(self) -> None:
        """切到下一曲。"""
        self.send(MediaCommand.NEXT)

    def previous(self) -> None:
        """切到上一曲。"""
        self.send(MediaCommand.PREVIOUS)

    def stop(self) -> None:
        """停止播放。"""
        self.send(MediaCommand.STOP)


class _SessionController:
    """会话与电源生命周期命名空间（``uda.session``）。

    六个动作方法各自对应一个 C 导出 ``uda_session_*``，互不掩饰自己触发的是
    哪个系统动作；调用点从源码就能看出来，而不是一个泛泛的
    ``perform(action_code)``。

    **安全约定**：除 :meth:`lock` 外的五个方法会结束用户会话或停止机器，返回
    成功时已不可撤销。请先用 :attr:`capabilities` 确认平台支持，并在调用前取得
    用户显式确认。
    """

    __slots__ = ("_uda",)

    def __init__(self, uda: "Uda") -> None:
        self._uda = uda

    # ------------------------------------------------------------------
    # 能力查询（无副作用，可随时调用）
    # ------------------------------------------------------------------
    @property
    def capabilities(self) -> dict[str, bool]:
        """当前平台的会话动作能力矩阵。

        返回一个以动作名为键的字典，例如::

            {"lock": True, "logout": True, "suspend": True,
             "hibernate": False, "reboot": True, "shutdown": True}

        该查询是**静态且无副作用**的：不会触碰机器的电源状态，因此可以随意调
        用来决定界面上画哪些按钮——也必须在画出"关机"这类按钮之前调用。

        能力位表达"代码路径存在"，**不是**"当前账户被允许"：关掉休眠的机器依
        然 ``hibernate: True``，真正拒绝发生在调用时。Windows 的 reboot /
        shutdown 还需要 `SeShutdownPrivilege`，同样是运行时答案。
        """
        slot = _UInt32Slot()

        self._uda._check(
            self._uda._lib.uda_session_capabilities(ctypes.byref(slot._slot)),
            "session_capabilities",
        )

        return {
            action: bool(slot.value & capability)
            for action, capability in _SESSION_ACTION_CAPABILITY.items()
        }

    def supports(self, action: str) -> bool:
        """单个动作是否被当前平台支持。

        Args:
            action: :class:`SessionAction` 常量之一。

        Returns:
            ``True`` 表示后端存在该动作的代码路径。

        Raises:
            UdaError: 动作名无法识别（状态码 -1）。
        """
        capability = self._capability_of(action)
        slot = _UInt32Slot()

        self._uda._check(
            self._uda._lib.uda_session_capabilities(ctypes.byref(slot._slot)),
            "session_capabilities",
        )

        return bool(slot.value & capability)

    def _capability_of(self, action: str) -> int:
        """把动作名翻译成能力位；未知名直接拒绝而不是猜一个。"""
        try:
            return _SESSION_ACTION_CAPABILITY[action]
        except KeyError:
            known = "、".join(sorted(_SESSION_ACTION_CAPABILITY))
            raise UdaError(
                ERR_INVALID_ARGUMENT,
                f"未知的会话动作 {action!r}；可用动作：{known}",
            ) from None

    def _perform(self, action: str) -> None:
        """调用 ``uda_session_<action>`` 并把状态码翻译成异常或成功。"""
        try:
            suffix = _SESSION_ACTIONS[action]
        except KeyError:
            known = "、".join(sorted(_SESSION_ACTIONS))
            raise UdaError(
                ERR_INVALID_ARGUMENT,
                f"未知的会话动作 {action!r}；可用动作：{known}",
            ) from None

        self._uda._check(
            getattr(self._uda._lib, f"uda_session_{suffix}")(),
            f"session_{suffix}",
        )

    # ------------------------------------------------------------------
    # 六个动作
    # ------------------------------------------------------------------
    def lock(self) -> None:
        """锁定会话；**这是唯一可以安全自动化的动作**。

        Linux：session 总线上的 ``org.freedesktop.ScreenSaver.Lock()``，失败时
        回退 ``loginctl lock-session``。Windows：``LockWorkStation()``。

        该动作可逆（用户输密码解锁）且不销毁任何数据，正在运行的程序继续运行。
        """
        self._perform(SessionAction.LOCK)

    def logout(self) -> None:
        """结束当前用户的会话。

        Linux：system 总线的
        ``org.freedesktop.login1.Manager.TerminateSession("")``，失败时回退桌
        面自己的会话管理器。Windows：``ExitWindowsEx(EWX_LOGOFF, 0)``。

        Warning:
            该动作会注销用户，未保存的工作可能丢失。**必须**先取得用户显式确
            认。
        """
        self._perform(SessionAction.LOGOUT)

    def suspend(self) -> None:
        """挂起机器到内存。

        Linux：``org.freedesktop.login1.Manager.Suspend(false)``。Windows：
        ``SetSuspendState(false, ...)``。

        Warning:
            该动作会改变机器的电源状态。**必须**先取得用户显式确认。
        """
        self._perform(SessionAction.SUSPEND)

    def hibernate(self) -> None:
        """休眠机器到磁盘。

        Linux：``org.freedesktop.login1.Manager.Hibernate(false)``。Windows：
        ``SetSuspendState(true, ...)``，系统未启用休眠时以状态码 -2 拒绝。

        Warning:
            该动作会改变机器的电源状态。**必须**先取得用户显式确认。
        """
        self._perform(SessionAction.HIBERNATE)

    def reboot(self) -> None:
        """重启机器。

        Linux：``org.freedesktop.login1.Manager.Reboot(false)``。Windows：
        ``ExitWindowsEx(EWX_REBOOT | EWX_FORCEIFHUNG, 0)``，需先启用
        `SeShutdownPrivilege`；权限不足时以状态码 -2 拒绝而不会执行到一半。

        Warning:
            该动作会重启机器，未保存的工作一定丢失。**必须**先取得用户显式确
            认。
        """
        self._perform(SessionAction.REBOOT)

    def shutdown(self) -> None:
        """关闭机器电源。

        Linux：``org.freedesktop.login1.Manager.PowerOff(false)``。Windows：
        ``ExitWindowsEx(EWX_POWEROFF | EWX_FORCEIFHUNG, 0)``，同样需要
        `SeShutdownPrivilege`。

        Warning:
            该动作会关机，未保存的工作一定丢失。**必须**先取得用户显式确认。
        """
        self._perform(SessionAction.SHUTDOWN)


class WakeLock:
    """防休眠常亮锁。

    建议用 ``with`` 语句持有，退出时自动释放::

        with uda.wakelock() as lock:
            ...  # 屏幕不会休眠
    """

    def __init__(self, uda: Uda, lock_type: str, reason: str) -> None:
        self._uda = uda
        self._type = lock_type
        self._handle = uda._acquire_wakelock(lock_type, reason)

    @property
    def handle(self) -> int:
        """库分配的句柄。"""
        return self._handle

    def release(self) -> None:
        """释放常亮锁。重复调用是安全的。"""
        if self._handle == 0:
            return
        handle = self._handle
        self._handle = 0
        self._uda._release_wakelock(handle)

    def __enter__(self) -> "WakeLock":
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        self.release()

    def __del__(self) -> None:  # pragma: no cover - 仅在忘记释放时兜底
        try:
            self.release()
        except Exception:
            pass


# --------------------------------------------------------------------------
# 托盘（Tray）
# --------------------------------------------------------------------------


class TrayMenu:
    """托盘右键菜单。

    菜单是"先构造、后挂载"的数据对象：先 ``add_text`` / ``add_checkbox`` /
    ``add_separator`` 逐行添加，再交给 :meth:`TrayIcon.set_menu` 挂到图标上。

    示例::

        menu = uda.create_tray_menu()
        menu.add_text("打招呼", lambda item_id, data: print("你好"))
        menu.add_checkbox("深色模式", checked=False, callback=on_toggle)
        menu.add_separator()
        menu.add_text("退出", lambda item_id, data: icon.stop())
        icon.menu = menu
    """

    def __init__(self, uda: "Uda") -> None:
        self._uda = uda
        self._handle = 0
        #: 已添加的行（按添加顺序），用于 :meth:`item_for` 反查。
        self._items: list["TrayItem"] = []
        self._create()

    def _create(self) -> None:
        slot = _UInt64Slot()
        self._uda._check(
            self._uda._lib.uda_tray_menu_create(ctypes.byref(slot._slot)),
            "uda_tray_menu_create",
        )
        self._handle = slot.value
        if self._handle == 0:
            raise UdaError(ERR_INTERNAL, "uda_tray_menu_create 返回了空句柄")
        self._uda._register_tray_menu(self)

    # ------------------------------------------------------------------
    # 构造行
    # ------------------------------------------------------------------
    def add_text(
        self,
        label: str,
        callback: TrayTextCallback | None = None,
        user_data: Any = None,
    ) -> "TrayItem":
        """添加一行普通文本项。

        Args:
            label: 菜单项文本；空白文本会被库拒绝（``UDA_ERR_NOT_SUPPORTED``）。
            callback: 点击回调，签名为 ``(item_id, user_data) -> None``。
                回调运行在**托盘工作线程**上，必须尽快返回，不能直接操作 UI。
            user_data: 原样透传给回调的 Python 对象（由本封装保活）。

        Returns:
            代表该行的 :class:`TrayItem`。
        """
        slot = _UInt64Slot()
        raw_callback, keepalive = self._make_text_trampoline(callback, user_data)

        status = self._uda._lib.uda_tray_menu_add_text(
            self._handle,
            label.encode("utf-8"),
            raw_callback,
            None,
            ctypes.byref(slot._slot),
        )
        self._uda._check(status, f"uda_tray_menu_add_text({label!r})")
        item_id = slot.value
        item = TrayItem(self, item_id, "text", label, callback)
        self._items.append(item)
        # 保活蹦床与用户回调对象，防止被 GC 后函数指针悬空。
        if keepalive is not None:
            self._uda._trampolines.append(keepalive)
        return item

    def add_separator(self) -> "TrayMenu":
        """添加一条分隔线。

        Returns:
            本菜单，便于链式调用。
        """
        status = self._uda._lib.uda_tray_menu_add_separator(self._handle)
        self._uda._check(status, "uda_tray_menu_add_separator")
        self._items.append(TrayItem(self, 0, "separator", "", None))
        return self

    def add_checkbox(
        self,
        label: str,
        checked: bool = False,
        callback: TrayCheckboxCallback | None = None,
        user_data: Any = None,
    ) -> "TrayItem":
        """添加一个复选框项。

        库会在调用回调**之前**翻转内部状态，因此回调收到的是勾选后的新值，
        与托盘实际渲染状态一致。

        Args:
            label: 菜单项文本。
            checked: 初始是否勾选。
            callback: 勾选状态变化回调，签名为
                ``(item_id, checked, user_data) -> None``。
            user_data: 原样透传给回调的 Python 对象。

        Returns:
            代表该行的 :class:`TrayItem`。
        """
        slot = _UInt64Slot()
        raw_callback, keepalive = self._make_checkbox_trampoline(callback, user_data)

        status = self._uda._lib.uda_tray_menu_add_checkbox(
            self._handle,
            label.encode("utf-8"),
            1 if checked else 0,
            raw_callback,
            None,
            ctypes.byref(slot._slot),
        )
        self._uda._check(status, f"uda_tray_menu_add_checkbox({label!r})")
        item_id = slot.value
        item = TrayItem(self, item_id, "checkbox", label, callback)
        self._items.append(item)
        if keepalive is not None:
            self._uda._trampolines.append(keepalive)
        return item

    # ------------------------------------------------------------------
    # 回调蹦床（内部）
    # ------------------------------------------------------------------
    def _make_text_trampoline(
        self,
        callback: TrayTextCallback | None,
        user_data: Any,
    ) -> tuple[Any, Any]:
        """把 Python 回调包装成 C 函数指针。

        Returns:
            ``(c_function_pointer, keepalive)``。``c_function_pointer`` 直接
            传给 ``uda_tray_menu_add_text``，``keepalive`` 需被强引用，
            否则 ctypes 实例被回收后指针悬空。
        """
        if callback is None:
            # 传 NULL 表示"静默行"，库端仍会渲染与响应，只是无回调。
            return None, None

        uda = self._uda
        # user_data 闭包捕获：Python 对象随蹦床一起被保活，生命周期与行一致。
        payload = user_data

        def invoke(item_id: int, raw_user_data: Any = None) -> None:
            """C 蹦床入口。

            ``raw_user_data`` 始终是库侧传来的指针（本封装统一传 NULL），
            因此用户看到的 ``user_data`` 用的是闭包捕获的 Python 对象
            ``payload``；两者刻意分离，避免把裸地址暴露给调用方。
            """
            try:
                callback(item_id, payload)
            except Exception as exc:  # noqa: BLE001 - 不能异常穿透 C 边界
                print(f"[UDA tray] 文本菜单回调异常: {exc!r}", file=sys.stderr)

        factory = uda._TextCallback
        # 注意：必须持有返回的 CFUNCTYPE 实例，见 Uda._trampolines 说明。
        c_function = factory(invoke)
        keepalive = (c_function, callback, payload)
        return c_function, keepalive

    def _make_checkbox_trampoline(
        self,
        callback: TrayCheckboxCallback | None,
        user_data: Any,
    ) -> tuple[Any, Any]:
        """把 Python 复选框回调包装成 C 函数指针。"""
        if callback is None:
            return None, None

        uda = self._uda
        payload = user_data

        def invoke(item_id: int, checked: int, raw_user_data: Any = None) -> None:
            """C 蹦床入口。``raw_user_data`` 见 :meth:`_make_text_trampoline`。"""
            try:
                callback(item_id, bool(checked), payload)
            except Exception as exc:  # noqa: BLE001 - 不能异常穿透 C 边界
                print(f"[UDA tray] 复选框回调异常: {exc!r}", file=sys.stderr)

        c_function = uda._CheckboxCallback(invoke)
        keepalive = (c_function, callback, payload)
        return c_function, keepalive

    # ------------------------------------------------------------------
    # 生命周期
    # ------------------------------------------------------------------
    @property
    def handle(self) -> int:
        """库分配的菜单句柄；销毁后为 0。"""
        return self._handle

    @property
    def items(self) -> list["TrayItem"]:
        """已添加的行（含分隔线），按顺序。"""
        return list(self._items)

    def item_for(self, item_id: int) -> "TrayItem | None":
        """按 id 查找已注册的行。"""
        for item in self._items:
            if item.item_id == item_id:
                return item
        return None

    def destroy(self) -> None:
        """销毁菜单句柄。

        在 :meth:`TrayIcon.set_menu` 之后调用是安全的：图标持有自己的引用，
        托盘仍可正常工作。销毁后句柄不可再用。
        """
        if self._handle == 0:
            return
        handle = self._handle
        self._handle = 0
        try:
            status = self._uda._lib.uda_tray_menu_destroy(handle)
            self._uda._check(status, f"uda_tray_menu_destroy({handle})")
        finally:
            self._uda._unregister_tray_menu(self)

    def __enter__(self) -> "TrayMenu":
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        self.destroy()

    def __del__(self) -> None:  # pragma: no cover - 仅在忘记销毁时兜底
        try:
            self.destroy()
        except Exception:
            pass


class TrayItem:
    """菜单中的一行（文本项、复选框或分隔线）。

    该对象只是 Python 侧的登记记录，用于把库分配的 ``item_id`` 与
    调用方原始回调关联起来；库侧的菜单数据以 ``item_id`` 为准。

    Args:
        menu: 所属的 :class:`TrayMenu`。
        item_id: 库分配的行 id；分隔线为 0（无 id）。
        kind: ``"text"`` / ``"checkbox"`` / ``"separator"``。
        label: 行文本。
        callback: 调用方注册的原始回调（可为 ``None``）。
    """

    def __init__(
        self,
        menu: "TrayMenu",
        item_id: int,
        kind: str,
        label: str,
        callback: Any,
    ) -> None:
        self.menu = menu
        self.item_id = item_id
        self.kind = kind
        self.label = label
        self.callback = callback

    def __repr__(self) -> str:
        return (
            f"TrayItem(kind={self.kind!r}, label={self.label!r}, "
            f"item_id={self.item_id})"
        )


class TrayIcon:
    """系统托盘图标。

    示例::

        icon = uda.create_tray_icon("我的应用", "提示", "icons/logo.png")
        menu = uda.create_tray_menu()
        menu.add_text("退出", lambda item_id, data: icon.destroy())
        icon.menu = menu
        icon.wait()          # 阻塞，直到 destroy() 被调用
    """

    def __init__(self, uda: "Uda", name: str, tooltip: str = "") -> None:
        self._uda = uda
        self._handle = 0
        self._menu: TrayMenu | None = None
        self._stopped = False
        # 本封装记录的最近一次外观设置值。库侧不提供 getter，因此这些字段表示
        # "调用方最后一次设置成什么"，而不是"shell 当前渲染成什么"。
        self._tooltip = tooltip
        self._icon_source = ""
        self._visible = True
        slot = _UInt64Slot()
        status = self._uda._lib.uda_tray_create(
            name.encode("utf-8"),
            tooltip.encode("utf-8"),
            ctypes.byref(slot._slot),
        )
        self._uda._check(status, "uda_tray_create")
        self._handle = slot.value
        if self._handle == 0:
            raise UdaError(ERR_INTERNAL, "uda_tray_create 返回了空句柄")
        self._uda._register_tray_icon(self)

    @property
    def handle(self) -> int:
        """库分配的图标句柄；销毁后为 0。"""
        return self._handle

    # ------------------------------------------------------------------
    # 外观
    # ------------------------------------------------------------------
    @property
    def tooltip(self) -> str:
        """悬停提示文本（本封装记录的最近一次设置值）。"""
        return self._tooltip

    @tooltip.setter
    def tooltip(self, text: str) -> None:
        """修改悬停提示文本。超过 127 字符会被库截断。"""
        self._require_handle()
        status = self._uda._lib.uda_tray_set_tooltip(
            self._handle, text.encode("utf-8")
        )
        self._uda._check(status, f"uda_tray_set_tooltip({text!r})")
        self._tooltip = text

    @property
    def icon(self) -> str:
        """当前图标来源（本封装记录的路径；未设置时为空串）。"""
        return self._icon_source

    @icon.setter
    def icon(self, source: str | os.PathLike[str]) -> None:
        """从图片文件设置托盘图标。

        传入 ``.png`` 等路径即可，SDK 内部会读文件、解码成 RGBA、降采样后提交。

        之所以不直接把路径交给 ``uda_tray_set_icon_path``：Linux 后端把该参数
        当作 **freedesktop 图标主题名**（见
        ``crates/uda-platform-linux/src/tray.rs`` 的 ``IconPayload::Name``），
        Windows 后端才按文件路径交给 ``LoadImageW``。仓库内的 PNG 路径在 Linux
        上只会解析成一个不存在的主题名，托盘依旧是空的；RGBA 通道两端语义一致。

        解码失败只记录一条日志并保持原图标：托盘的事件与菜单都不依赖图标，
        不该让整个应用因此退出。
        """
        self._require_handle()
        path = Path(source).expanduser()
        try:
            width, height, rgba = load_icon_rgba(path, TRAY_ICON_MAX_EXTENT)
        except PngError as error:
            print(f"[UDA tray] 无法加载图标 {path}: {error}", file=sys.stderr)
            return

        self._set_icon_rgba(width, height, rgba)
        self._icon_source = str(source)

    def _set_icon_rgba(self, width: int, height: int, data: bytes) -> None:
        """提交 RGBA 像素（内部路径，业务代码请用 :attr:`icon`）。"""
        stride = width * 4
        if len(data) < stride * height:
            raise UdaError(
                ERR_INVALID_ARGUMENT,
                f"RGBA 数据长度 {len(data)} 小于 stride*height={stride * height}",
            )
        buffer = (ctypes.c_uint8 * len(data)).from_buffer_copy(data)
        status = self._uda._lib.uda_tray_set_icon_rgba(
            self._handle,
            width,
            height,
            stride,
            buffer,
            len(data),
        )
        self._uda._check(status, "uda_tray_set_icon_rgba")

    @property
    def visible(self) -> bool:
        """图标当前是否可见（本封装记录的最近一次设置值）。"""
        return self._visible

    @visible.setter
    def visible(self, value: bool) -> None:
        """显示 / 隐藏图标（不注销，可随时恢复）。"""
        self._require_handle()
        status = self._uda._lib.uda_tray_set_visible(
            self._handle, 1 if value else 0
        )
        self._uda._check(status, f"uda_tray_set_visible({value})")
        self._visible = bool(value)

    # ------------------------------------------------------------------
    # 菜单
    # ------------------------------------------------------------------
    @property
    def menu(self) -> TrayMenu | None:
        """当前挂载的菜单。"""
        return self._menu

    @menu.setter
    def menu(self, menu: TrayMenu) -> None:
        """把菜单挂到图标上（替换已有菜单）。

        挂载后 ``menu`` 仍可独立销毁；图标持有自己的引用。
        """
        self._require_handle()
        if menu.handle == 0:
            raise UdaError(ERR_INVALID_ARGUMENT, "菜单已被销毁")
        status = self._uda._lib.uda_tray_set_menu(self._handle, menu.handle)
        self._uda._check(status, "uda_tray_set_menu")
        self._menu = menu

    # ------------------------------------------------------------------
    # 生命周期
    # ------------------------------------------------------------------
    def stop(self) -> None:
        """请求 :meth:`wait` 返回，但**不**注销图标。"""
        self._stopped = True

    def wait(self) -> None:
        """阻塞当前线程，直到 :meth:`stop` 或 :meth:`destroy` 被调用。

        菜单回调运行在托盘工作线程上，因此这里只需等待标志位，绝不能在回调
        线程里做阻塞操作。
        """
        import time

        while not self._stopped and self._handle:
            time.sleep(0.2)

    def _require_handle(self) -> None:
        if self._handle == 0:
            raise UdaError(ERR_INVALID_ARGUMENT, "托盘图标已被销毁")

    def destroy(self) -> None:
        """销毁图标并从系统托盘注销。句柄不可再用。"""
        if self._handle == 0:
            return
        handle = self._handle
        self._handle = 0
        try:
            status = self._uda._lib.uda_tray_destroy(handle)
            self._uda._check(status, f"uda_tray_destroy({handle})")
        finally:
            self._uda._unregister_tray_icon(self)
            self._menu = None
            self._stopped = True

    def __enter__(self) -> "TrayIcon":
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        self.destroy()

    def __del__(self) -> None:  # pragma: no cover - 仅在忘记销毁时兜底
        try:
            self.destroy()
        except Exception:
            pass
