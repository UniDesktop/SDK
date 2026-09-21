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
from typing import Final

__all__ = ["Uda", "UdaError", "Theme", "FillMode", "WakeLockType"]

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
    # 资源管理
    # ------------------------------------------------------------------
    def release_all(self) -> None:
        """释放本对象持有的全部常亮锁（单个失败不阻断其余释放）。"""
        for handle in list(self._handles):
            try:
                self.wakelock_release(handle)
            except UdaError:
                # 句柄已失效时无需再报错，调用方可按需查询。
                pass

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
