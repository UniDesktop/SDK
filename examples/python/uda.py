"""ctypes 封装：UniDesktop API (UDA) C-ABI 层。

纯 Python 标准库实现（`ctypes`），无任何第三方依赖。加载平台对应的动态库：

- Linux   -> ``libuda_ffi.so``
- macOS   -> ``libuda_ffi.dylib``
- Windows -> ``uda_ffi.dll``

动态库按以下顺序定位：``UDA_LIBRARY`` 环境变量 > ``cargo metadata`` 报告的
target 目录 > 仓库内常见构建目录 > 系统动态库搜索路径。

约定见 ``include/uda.h``：所有函数返回 ``int32_t`` 状态码（0 成功，负数失败），
库返回的字符串必须用 :meth:`Uda.free_string` 释放。

用法::

    from uda import Uda

    with Uda() as uda:
        theme = uda.detect_theme()          # "dark" / "light" / "unknown"
        path = uda.get_wallpaper()          # str 或 None
        handle = uda.wakelock_acquire("display", "示例")
        uda.wakelock_release(handle)
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

__all__ = [
    "Uda",
    "UdaError",
    "Theme",
    "FillMode",
    "WakeLockType",
    "TrayIcon",
    "TrayMenu",
    "TrayItem",
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

#: 主题对应的 C 状态码。
THEME_UNKNOWN: Final[int] = 0
THEME_DARK: Final[int] = 1
THEME_LIGHT: Final[int] = 2

_THEME_NAMES: Final[dict[int, str]] = {
    THEME_UNKNOWN: "unknown",
    THEME_DARK: "dark",
    THEME_LIGHT: "light",
}

#: 填充模式对应的 C 状态码。
FILL_CROP: Final[int] = 0
FILL_FILL: Final[int] = 1
FILL_FIT: Final[int] = 2
FILL_STRETCH: Final[int] = 3

#: 常亮锁类型对应的 C 状态码。
WAKELOCK_DISPLAY: Final[int] = 0
WAKELOCK_SYSTEM: Final[int] = 1

#: 托盘菜单文本项回调签名：``(item_id: int, user_data) -> None``。
TrayTextCallback = Callable[[int, Any], None]
#: 托盘菜单复选框项回调签名：``(item_id: int, checked: bool, user_data) -> None``。
TrayCheckboxCallback = Callable[[int, bool, Any], None]

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
    "../../target/debug/uda_ffi.dll",
    "../../target/release/uda_ffi.dll",
)

#: 调用 ``cargo metadata`` 查询真实 target 目录时的超时（秒）。
_CARGO_METADATA_TIMEOUT: Final[float] = 10.0


class UdaError(RuntimeError):
    """UDA 调用失败时抛出，携带状态码与诊断消息。"""

    def __init__(self, status: int, message: str) -> None:
        self.status = status
        self.message = message
        super().__init__(f"UDA error {status}: {message}")


class Theme:
    """主题名称常量，便于 ``from uda import Theme`` 后使用具名字面量。"""

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


_FILL_CODES: Final[dict[str, int]] = {
    FillMode.CROP: FILL_CROP,
    FillMode.FILL: FILL_FILL,
    FillMode.FIT: FILL_FIT,
    FillMode.STRETCH: FILL_STRETCH,
}

_WAKELOCK_CODES: Final[dict[str, int]] = {
    WakeLockType.DISPLAY: WAKELOCK_DISPLAY,
    WakeLockType.SYSTEM: WAKELOCK_SYSTEM,
}


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
    """按优先级返回动态库候选路径。

    优先使用环境变量 ``UDA_LIBRARY``，其次询问 Cargo 得到真实 target 目录，
    再尝试仓库常见构建目录，最后交给系统的动态库搜索路径（由
    :func:`ctypes.CDLL` 解析裸库名）。
    """
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
        return ctypes.CDLL(filename)
    except OSError as exc:
        errors.append(f"{filename} (system path): {exc}")

    raise UdaError(
        ERR_NOT_SUPPORTED,
        "无法加载 libuda_ffi；请先在仓库根目录执行 `cargo build -p uda-ffi`，"
        f"或设置 UDA_LIBRARY 指向动态库。已尝试：{'; '.join(errors) or '无候选路径'}",
    )


class Uda:
    """UDA C-ABI 的 Python 封装。

    使用 ``with`` 语句可确保退出前释放本对象持有的全部常亮锁，避免
    ``systemd-inhibit`` 子进程残留::

        with Uda() as uda:
            ...
    """

    def __init__(self, library_path: str | os.PathLike[str] | None = None) -> None:
        """加载动态库并声明全部导出函数的原型。

        Args:
            library_path: 显式指定动态库路径；为 ``None`` 时按默认顺序查找。
        """
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
        #: 这里集中持有，直到 ``TrayIcon`` / ``TrayMenu`` 被销毁。
        self._trampolines: list[Any] = []

    # ------------------------------------------------------------------
    # ctypes 原型声明
    # ------------------------------------------------------------------
    def _declare_prototypes(self) -> None:
        """声明每个导出函数的参数与返回类型。

        显式声明 ``argtypes`` 是内存安全的关键：否则 ctypes 会把 Python 整数
        按 C ``int`` 传入，64 位指针在 Windows 上会被截断。
        """
        c_int32_p = ctypes.POINTER(ctypes.c_int32)
        c_char_p_p = ctypes.POINTER(ctypes.c_char_p)
        c_uint64_p = ctypes.POINTER(ctypes.c_uint64)

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
        # Rust 端把它当函数指针调用就会跳到 Python 堆上并直接段错误
        # （实测复现：注册成功、item_id 正常返回，直到工作线程回调进来才崩）。
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
    # 公共 API
    # ------------------------------------------------------------------
    def detect_theme(self) -> str:
        """检测系统深浅色。

        Returns:
            ``"dark"``、``"light"`` 或 ``"unknown"``。
        """
        out = ctypes.c_int32(-1)
        status = self._lib.uda_detect_theme(ctypes.byref(out))
        self._check(status, "detect_theme")
        return _THEME_NAMES.get(out.value, "unknown")

    def set_wallpaper(self, path: str, fill_mode: str = FillMode.FILL) -> None:
        """设置桌面壁纸。

        Args:
            path: 图片文件路径（UTF-8）。
            fill_mode: ``"crop"`` / ``"fill"`` / ``"fit"`` / ``"stretch"``。
        """
        try:
            code = _FILL_CODES[fill_mode]
        except KeyError:
            raise UdaError(
                ERR_INVALID_ARGUMENT,
                f"未知填充模式 {fill_mode!r}；可选：{sorted(_FILL_CODES)}",
            ) from None

        encoded = path.encode("utf-8")
        status = self._lib.uda_set_wallpaper(encoded, code)
        self._check(status, f"set_wallpaper({path!r})")

    def get_wallpaper(self) -> str | None:
        """读取当前壁纸路径。

        Returns:
            壁纸路径；未设置壁纸或平台不支持时返回 ``None``。
        """
        out = ctypes.c_char_p()
        status = self._lib.uda_get_wallpaper(ctypes.byref(out))
        self._check(status, "get_wallpaper")

        if not out.value:
            return None

        try:
            return out.value.decode("utf-8", errors="replace")
        finally:
            # 无论解码是否成功都必须释放，避免泄漏。
            self._lib.uda_free_string(out)

    def free_string(self, pointer: ctypes.c_char_p | None) -> None:
        """释放由库分配的字符串。

        ``uda_free_string`` 接受 NULL，因此空指针安全跳过；ctypes 的
        ``c_char_p`` 取值为 ``None`` 时即对应 C 的 NULL。

        Args:
            pointer: 由本库（例如 :meth:`get_wallpaper`）返回的指针。
        """
        if pointer:
            self._lib.uda_free_string(pointer)

    # ------------------------------------------------------------------
    # 常亮锁
    # ------------------------------------------------------------------
    def wakelock_acquire(
        self, lock_type: str = WakeLockType.DISPLAY, reason: str = "UDA"
    ) -> int:
        """申请防休眠常亮锁。

        Args:
            lock_type: ``"display"`` 或 ``"system"``。
            reason: 诊断用描述文本。

        Returns:
            非零句柄，需传给 :meth:`wakelock_release`。
        """
        try:
            code = _WAKELOCK_CODES[lock_type]
        except KeyError:
            raise UdaError(
                ERR_INVALID_ARGUMENT,
                f"未知常亮锁类型 {lock_type!r}；可选：{sorted(_WAKELOCK_CODES)}",
            ) from None

        out = ctypes.c_uint64(0)
        status = self._lib.uda_wakelock_acquire(
            code, reason.encode("utf-8"), ctypes.byref(out)
        )
        self._check(status, f"wakelock_acquire({lock_type!r})")

        handle = int(out.value)
        if handle == 0:
            # 库保证成功时返回非零句柄；为 0 说明契约被破坏。
            raise UdaError(ERR_INTERNAL, "wakelock_acquire 返回了空句柄")
        self._handles.append(handle)
        return handle

    def wakelock_release(self, handle: int) -> None:
        """释放常亮锁。

        Args:
            handle: :meth:`wakelock_acquire` 返回的句柄。
        """
        status = self._lib.uda_wakelock_release(handle)
        self._check(status, f"wakelock_release({handle})")
        if handle in self._handles:
            self._handles.remove(handle)

    # ------------------------------------------------------------------
    # 托盘（Tray）
    # ------------------------------------------------------------------
    def create_tray_icon(
        self, name: str = "UDA", tooltip: str = ""
    ) -> "TrayIcon":
        """创建托盘图标。

        返回的 :class:`TrayIcon` 由调用方持有；销毁它才会真正从系统托盘注销。

        Args:
            name: 应用名（用于 D-Bus 总线名 / 窗口类注册）。
            tooltip: 悬停提示文本，可为空；超过 127 字符会被截断。

        Returns:
            :class:`TrayIcon` 实例。
        """
        icon = TrayIcon(self, name, tooltip)
        self._tray_icons.append(icon)
        return icon

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
                self.wakelock_release(handle)
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


# --------------------------------------------------------------------------
# 托盘（Tray）
# --------------------------------------------------------------------------


class TrayMenu:
    """托盘右键菜单。

    菜单是"先构造、后挂载"的数据对象：先 ``add_text`` / ``add_checkbox`` /
    ``add_separator`` 逐行添加，再交给 :meth:`TrayIcon.set_menu` 挂到图标上。

    Args:
        uda: 拥有该菜单的 :class:`Uda` 实例。
    """

    def __init__(self, uda: "Uda") -> None:
        self._uda = uda
        self._handle = 0
        #: 已添加的行（按添加顺序），用于 :meth:`item_for` 反查。
        self._items: list["TrayItem"] = []
        self._create()

    def _create(self) -> None:
        out = ctypes.c_uint64(0)
        status = self._uda._lib.uda_tray_menu_create(ctypes.byref(out))
        self._uda._check(status, "uda_tray_menu_create")
        self._handle = int(out.value)
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
        return self._add_text(label, callback, user_data)

    def _add_text(
        self,
        label: str,
        callback: TrayTextCallback | None,
        user_data: Any,
    ) -> "TrayItem":
        out_item = ctypes.c_uint64(0)
        raw_callback, keepalive = self._make_text_trampoline(callback, user_data)

        status = self._uda._lib.uda_tray_menu_add_text(
            self._handle,
            label.encode("utf-8"),
            raw_callback,
            None,
            ctypes.byref(out_item),
        )
        self._uda._check(status, f"uda_tray_menu_add_text({label!r})")
        item_id = int(out_item.value)
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
        out_item = ctypes.c_uint64(0)
        raw_callback, keepalive = self._make_checkbox_trampoline(callback, user_data)

        status = self._uda._lib.uda_tray_menu_add_checkbox(
            self._handle,
            label.encode("utf-8"),
            1 if checked else 0,
            raw_callback,
            None,
            ctypes.byref(out_item),
        )
        self._uda._check(status, f"uda_tray_menu_add_checkbox({label!r})")
        item_id = int(out_item.value)
        item = TrayItem(self, item_id, "checkbox", label, callback)
        self._items.append(item)
        if keepalive is not None:
            self._uda._trampolines.append(keepalive)
        return item

    # ------------------------------------------------------------------
    # 回调蹦床
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

    Args:
        uda: 拥有该图标的 :class:`Uda` 实例。
        name: 应用名（注册用）。
        tooltip: 悬停提示文本，可为空。

    用法::

        with uda.create_tray_icon("UDA", "提示") as icon:
            menu = uda.create_tray_menu()
            menu.add_text("欢迎使用 UniDesktop", lambda item, data: print("你好"))
            menu.add_checkbox("开启深色模式同步", checked=False,
                              callback=lambda item, checked, data: print(checked))
            menu.add_separator()
            menu.add_text("退出程序", lambda item, data: icon.stop_event.set())
            icon.set_menu(menu)
            icon.run_until_stopped()
    """

    def __init__(self, uda: "Uda", name: str, tooltip: str = "") -> None:
        self._uda = uda
        self._handle = 0
        self._menu: TrayMenu | None = None
        out = ctypes.c_uint64(0)
        status = self._uda._lib.uda_tray_create(
            name.encode("utf-8"),
            tooltip.encode("utf-8"),
            ctypes.byref(out),
        )
        self._uda._check(status, "uda_tray_create")
        self._handle = int(out.value)
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
    def set_tooltip(self, tooltip: str) -> None:
        """修改悬停提示文本。超过 127 字符会被库截断。"""
        self._require_handle()
        status = self._uda._lib.uda_tray_set_tooltip(
            self._handle, tooltip.encode("utf-8")
        )
        self._uda._check(status, f"uda_tray_set_tooltip({tooltip!r})")

    def set_icon_path(self, path: str) -> None:
        """从文件路径或图标主题名设置图标。

        Linux 接受 freedesktop 图标主题名；Windows 需要文件路径。
        """
        self._require_handle()
        status = self._uda._lib.uda_tray_set_icon_path(
            self._handle, path.encode("utf-8")
        )
        self._uda._check(status, f"uda_tray_set_icon_path({path!r})")

    def set_icon_rgba(
        self, width: int, height: int, data: bytes, stride: int | None = None
    ) -> None:
        """从 RGBA 像素缓冲区设置图标。

        Args:
            width: 像素宽，非零。
            height: 像素高，非零。
            data: 自上而下、每像素 4 字节（R, G, B, A）的原始字节。
            stride: 每行字节数，默认 ``width * 4``。若缓冲区行间有填充，
                需显式给出。
        """
        self._require_handle()
        if stride is None:
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

    def set_visible(self, visible: bool) -> None:
        """显示 / 隐藏图标（不注销，可随时恢复）。"""
        self._require_handle()
        status = self._uda._lib.uda_tray_set_visible(
            self._handle, 1 if visible else 0
        )
        self._uda._check(status, f"uda_tray_set_visible({visible})")

    # ------------------------------------------------------------------
    # 菜单
    # ------------------------------------------------------------------
    def set_menu(self, menu: TrayMenu) -> None:
        """把菜单挂到图标上（替换已有菜单）。

        挂载后 ``menu`` 仍可独立销毁；图标持有自己的引用。
        """
        self._require_handle()
        if menu.handle == 0:
            raise UdaError(ERR_INVALID_ARGUMENT, "菜单已被销毁")
        status = self._uda._lib.uda_tray_set_menu(self._handle, menu.handle)
        self._uda._check(status, "uda_tray_set_menu")
        self._menu = menu

    @property
    def menu(self) -> TrayMenu | None:
        """当前挂载的菜单。"""
        return self._menu

    # ------------------------------------------------------------------
    # 生命周期
    # ------------------------------------------------------------------
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

    def __enter__(self) -> "TrayIcon":
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        self.destroy()

    def __del__(self) -> None:  # pragma: no cover - 仅在忘记销毁时兜底
        try:
            self.destroy()
        except Exception:
            pass
