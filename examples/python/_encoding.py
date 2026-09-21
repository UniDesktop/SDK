# -*- coding: utf-8 -*-
"""跨平台输出编码护栏。

Windows 控制台默认使用 OEM/ANSI 代码页（北美/英国镜像为 ``cp1252``），该代码页
无法编码中日韩等非拉丁字符，于是任何包含中文的 ``print()`` 都会抛出
``UnicodeEncodeError: 'charmap' codec can't encode characters``。GitHub Actions
的 Windows runner 正是如此，而 Linux/macOS 默认 UTF-8，所以问题只在 Windows
复现，本地 Linux 验证很难提前发现。

本模块把标准输出/标准错误重定向到 UTF-8，并在 ``errors="replace"`` 下兜底，
确保任何字符都能落盘；同时探测控制台是否真的受限于代码页，供上层决定是否
回落到英文诊断。

用法（放在模块最顶部，先于其它 import）::

    from _encoding import force_utf8_output, console_supports_unicode

    force_utf8_output()
    if console_supports_unicode():
        ...
"""

from __future__ import annotations

import sys

__all__ = ["force_utf8_output", "console_supports_unicode", "detect_language"]


def force_utf8_output(errors: str = "replace") -> None:
    """把 ``sys.stdout`` / ``sys.stderr`` 重配置为 UTF-8。

    Args:
        errors: 编码失败时的降级策略，默认 ``"replace"``（替换为 ``?``）
            而不是 ``"strict"``，保证极端情况下输出仍能继续而不是抛异常。

    兼容性说明：

    - Python 3.7+ 的 :meth:`io.TextIOWrapper.reconfigure` 可用；
    - 若流已被替换为自定义对象（某些测试框架、IDE 集成终端），优雅降级为
      重新包装一个 UTF-8 文本层；
    - 若连重新包装都失败（例如流已关闭），保持原样，绝不让诊断代码把程序搞崩。
    """
    for stream_name in ("stdout", "stderr"):
        stream = getattr(sys, stream_name, None)
        if stream is None:
            continue

        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is not None:
            try:
                reconfigure(encoding="utf-8", errors=errors)
            except (ValueError, OSError, LookupError):
                # LookupError: 引擎不认识 "utf-8"（理论不存在，兜底）
                # ValueError / OSError: 流不支持重配置（已关闭、非 TextIOWrapper）
                pass
            continue

        # 老式或被替换的流：尝试用 UTF-8 缓冲层重新包装。
        buffer = getattr(stream, "buffer", None)
        if buffer is None:
            continue
        try:
            import io

            setattr(
                sys,
                stream_name,
                io.TextIOWrapper(
                    buffer,
                    encoding="utf-8",
                    errors=errors,
                    line_buffering=getattr(stream, "line_buffering", False),
                ),
            )
        except (ValueError, OSError):  # pragma: no cover - hostile console
            pass


def console_supports_unicode() -> bool:
    """探测控制台能否原样呈现非 ASCII 字符。

    判定依据（按优先级）：

    1. ``PYTHONIOENCODING`` 已显式指定编码时，以其为准；
    2. ``sys.stdout.encoding`` 报告的是 ``utf-8`` / ``utf8`` / ``U8`` 时返回
       ``True``（此时无需回退）；
    3. 其它情况默认返回 ``False``，即假设可能受限于代码页，交由上层选择
       最保守的英文输出。
    """
    import os

    forced = os.environ.get("PYTHONIOENCODING", "")
    if forced:
        return forced.lower().replace("-", "") in {"utf8", "u8", "utf"}

    encoding = getattr(sys.stdout, "encoding", None) or ""
    return encoding.lower().replace("-", "") in {"utf8", "u8", "utf"}


def detect_language(default: str = "zh-CN") -> str:
    """根据环境变量推断界面语言，用于中文/英文输出的选择。

    识别 ``LANG`` / ``LC_ALL`` / ``LANGUAGE``；未设置或无法识别时返回
    ``default``（默认中文，与本仓库文档语言一致）。
    """
    import os

    for name in ("UDA_LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"):
        value = os.environ.get(name, "")
        if not value:
            continue
        tag = value.split(".")[0].split("@")[0].lower()
        if tag.startswith("en"):
            return "en"
        if tag.startswith("zh"):
            return "zh-CN"
    return default
