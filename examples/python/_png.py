# -*- coding: utf-8 -*-
"""极简 PNG 解码器：把归档的图标解码成 RGBA 像素。

托盘图标为什么要在这里解码
--------------------------
UDA 的 C-ABI 提供两条设置图标的路径：

* ``uda_tray_set_icon_path`` —— Linux 后端把它当作 **freedesktop 图标主题名**
  （``IconPayload::Name``，见 ``crates/uda-platform-linux/src/tray.rs``），
  Windows 后端才按文件路径用 ``LoadImageW`` 加载；
* ``uda_tray_set_icon_rgba`` —— 两端通用的原始像素通道。

因此把一个仓库内的 PNG 文件路径直接传给 ``set_icon_path``，在 Linux 上只会得到
一个不存在的主题名，托盘依然是空的。要两端都显示，就必须在本模块解码成 RGBA
再走 ``set_icon_rgba``。

为什么不用 Pillow
-----------------
``uda.py`` 的对外契约是"纯 Python 标准库实现、无第三方依赖"，示例不能为了加载
一张图标就引入 Pillow。PNG 的解码本身只需要 :mod:`zlib` 加上每行一个字节的
过滤器，代价是约 100 行代码；托盘图标是一次性启动成本，慢一点可以接受。

支持范围（仅覆盖仓库自带图标）
------------------------------
* 位深 8 位；色彩类型 0（灰度）、2（真彩）、3（调色板）、4（灰+alpha）、
  6（RGBA）。
* 非隔行扫描（Adam7 隔行不处理：托盘图标不需要，且实现代价不成比例）。
* 缺席的 ``tRNS`` 块按"完全不透明"处理。
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path
from typing import Final

__all__ = ["PngError", "decode_png_rgba", "downsample_rgba", "load_icon_rgba"]


class PngError(RuntimeError):
    """PNG 解析失败：文件损坏、格式不在支持范围内，或不是 PNG。"""


#: PNG 文件签名。
_SIGNATURE: Final[bytes] = b"\x89PNG\r\n\x1a\n"

#: 色彩类型 -> 每像素通道数。
_CHANNELS: Final[dict[int, int]] = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}


def _unfilter(row: bytes, previous: bytes, filter_type: int, channels: int) -> bytearray:
    """反演一行的 PNG 过滤器，还原出原始字节。

    每种过滤器都是"由左邻、上邻、上左邻推算"，因此逐字节逆推即可，
    循环内的分支顺序按出现频率排（0 与 1 最常见）。
    """
    line = bytearray(row)
    stride = len(line)
    for i in range(stride):
        a = line[i - channels] if i >= channels else 0
        b = previous[i]
        if filter_type == 0:
            continue
        if filter_type == 1:
            line[i] = (line[i] + a) & 0xFF
        elif filter_type == 2:
            line[i] = (line[i] + b) & 0xFF
        elif filter_type == 3:
            line[i] = (line[i] + ((a + b) >> 1)) & 0xFF
        elif filter_type == 4:
            c = previous[i - channels] if i >= channels else 0
            predictor = a + b - c
            pa = abs(predictor - a)
            pb = abs(predictor - b)
            pc = abs(predictor - c)
            nearest = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
            line[i] = (line[i] + nearest) & 0xFF
        else:
            raise PngError(f"未知 PNG 行过滤器 {filter_type}")
    return line


def _expand(width: int, height: int, color: int, palette: bytes, raw: bytes) -> bytes:
    """把解码后的像素展开成 RGBA，每像素恒定 4 字节，自上而下。"""
    channels = _CHANNELS[color]
    stride = width * channels
    rgba = bytearray(width * height * 4)

    for index in range(width * height):
        source = index * channels
        target = index * 4
        if color == 2:
            rgba[target : target + 3] = raw[source : source + 3]
            rgba[target + 3] = 0xFF
        elif color == 6:
            rgba[target : target + 4] = raw[source : source + 4]
        elif color == 0:
            gray = raw[source]
            rgba[target] = gray
            rgba[target + 1] = gray
            rgba[target + 2] = gray
            rgba[target + 3] = 0xFF
        elif color == 4:
            gray = raw[source]
            rgba[target] = gray
            rgba[target + 1] = gray
            rgba[target + 2] = gray
            rgba[target + 3] = raw[source + 1]
        else:  # color == 3, 调色板
            base = raw[source] * 3
            rgba[target : target + 3] = palette[base : base + 3]
            rgba[target + 3] = 0xFF

    del stride  # 仅为可读性保留声明处
    return bytes(rgba)


def decode_png_rgba(path: str | Path) -> tuple[int, int, bytes]:
    """把一个 PNG 文件解码成 RGBA 像素。

    Args:
        path: PNG 文件路径。

    Returns:
        ``(width, height, rgba)``：``rgba`` 长 ``width * height * 4``，自上而下、
        每像素 R, G, B, A 四字节，与 ``uda_tray_set_icon_rgba`` 的契约一致。

    Raises:
        PngError: 文件不存在、不是 PNG、使用了本解析器不支持的形式，
            或内容损坏。
    """
    file = Path(path)
    try:
        data = file.read_bytes()
    except OSError as exc:
        raise PngError(f"无法读取 {file}: {exc}") from exc

    if data[: len(_SIGNATURE)] != _SIGNATURE:
        raise PngError(f"{file} 不是 PNG 文件")

    # 只解析到 IEND；PNG 允许尾部有其他块（如 APNG 动画帧），对本用途无意义。
    offset = len(_SIGNATURE)
    width = height = depth = color = 0
    interlace = 0
    palette = b""
    compressed = bytearray()

    while offset + 8 <= len(data):
        (length,) = struct.unpack(">I", data[offset : offset + 4])
        kind = data[offset + 4 : offset + 8]
        body = data[offset + 8 : offset + 8 + length]
        # 每块末尾是 4 字节 CRC。这里逐块解压、不做校验：一个不匹配的 CRC
        # 应当由 `zlib.decompress` 或后续的尺寸校验暴露，而不是提前误报。
        offset += 12 + length

        if kind == b"IHDR":
            if len(body) < 13:
                raise PngError(f"{file} 的 IHDR 块残缺")
            width, height, depth, color, _compression, _filter, interlace = struct.unpack(
                ">IIBBBBB", body[:13]
            )
        elif kind == b"PLTE":
            palette = bytes(body)
        elif kind == b"IDAT":
            compressed += body
        elif kind == b"IEND":
            break

    if width <= 0 or height <= 0:
        raise PngError(f"{file} 的 IHDR 报告了非法尺寸 {width}x{height}")
    if depth != 8:
        # 位深 1/2/4/16 都要额外的按位拆包或降位处理，托盘图标无需支持。
        raise PngError(f"{file} 的位深 {depth} 不受支持（仅支持 8 位）")
    if color not in _CHANNELS:
        raise PngError(f"{file} 的色彩类型 {color} 不受支持")
    if interlace != 0:
        raise PngError(f"{file} 使用了隔行扫描，本解码器不支持")
    if color == 3 and not palette:
        raise PngError(f"{file} 声明使用调色板但没有 PLTE 块")

    try:
        stream = zlib.decompress(bytes(compressed))
    except zlib.error as exc:
        raise PngError(f"{file} 的 IDAT 解压失败: {exc}") from exc

    channels = _CHANNELS[color]
    stride = width * channels
    expected = (stride + 1) * height
    if len(stream) < expected:
        raise PngError(
            f"{file} 的像素数据不足：需要 {expected} 字节，实际 {len(stream)}"
        )

    # 逐行反演过滤器，前一行的**还原结果**是后一行的参照，因此必须顺序推进。
    pixels = bytearray(stride * height)
    previous = bytearray(stride)
    cursor = 0
    for row in range(height):
        filter_type = stream[cursor]
        cursor += 1
        line = _unfilter(stream[cursor : cursor + stride], previous, filter_type, channels)
        cursor += stride
        start = row * stride
        pixels[start : start + stride] = line
        previous = line

    return width, height, _expand(width, height, color, palette, bytes(pixels))


def downsample_rgba(
    width: int,
    height: int,
    rgba: bytes,
    max_extent: int,
) -> tuple[int, int, bytes]:
    """把 RGBA 图像等比缩小到最长边不超过 ``max_extent`` 像素。

    托盘图标只有 16~32 px（``docs/internals/tray_specs.md`` §2.6），而
    ``IconPixmap`` 会原样广播 ``(width, height, bytes)``。把一张 1254x1254 的
    原图直接提交上去，等于往会话总线灌 6 MB 数据，Windows 端还得为整张位图建
    DIB 再交给 shell 缩放，两头都白费，因此必须先降采样。

    过滤器是 **alpha 预乘的区域平均**（premultiplied box filter）。半透明像素
    若按普通平均混合，边缘颜色会被"透明即 0"拉暗，出现一圈黑边；先乘 alpha
    再累加、最后除以 alpha 之和即可避免。

    Args:
        width: 源宽，必须为正。
        height: 源高，必须为正。
        rgba: 长 ``width * height * 4`` 的像素数据，自上而下。
        max_extent: 目标最长边；``<= 0`` 表示不缩放。

    Returns:
        ``(width, height, rgba)``。源图已不超限时原样返回同一对象。

    Raises:
        PngError: 尺寸非法，或 ``rgba`` 与所报尺寸不符。
    """
    if width <= 0 or height <= 0:
        raise PngError(f"源图尺寸非法: {width}x{height}")
    if len(rgba) < width * height * 4:
        raise PngError(f"RGBA 数据长度 {len(rgba)} 与 {width}x{height} 不符")
    if max_extent <= 0 or max(width, height) <= max_extent:
        return width, height, rgba

    # 等比缩放：最长边正好落在 max_extent 上。另一边四舍五入后也不会超过它
    # （``edge * max_extent / max(w, h) <= max_extent`` 恒成立），只有下限 1
    # 需要兜住，否则极端长宽比会算出 0 像素的一条边。
    scale = max_extent / max(width, height)
    target_width = max(1, round(width * scale))
    target_height = max(1, round(height * scale))

    # 每个目标像素覆盖的源区域是左闭右开矩形。`max(..., +1)` 保证缩放比小于 1
    # （放大方向）时也不会出现空区域。
    ratio_x = width / target_width
    ratio_y = height / target_height

    # alpha 恒为 255 时预乘项退化成"通道和 * 255"，可以整段切片后交给内置
    # `sum()` 在 C 层累加；只有带 alpha 的图才必须逐像素相乘。
    fully_opaque = min(rgba[3::4]) == 0xFF

    out = bytearray(target_width * target_height * 4)
    for ty in range(target_height):
        y0 = int(ty * ratio_y)
        y1 = max(y0 + 1, int((ty + 1) * ratio_y))
        for tx in range(target_width):
            x0 = int(tx * ratio_x)
            x1 = max(x0 + 1, int((tx + 1) * ratio_x))
            columns = x1 - x0

            red = green = blue = alpha = 0
            for sy in range(y0, y1):
                start = sy * width * 4 + x0 * 4
                span = columns * 4
                chunk = rgba[start : start + span]
                if fully_opaque:
                    red += sum(chunk[0::4]) * 0xFF
                    green += sum(chunk[1::4]) * 0xFF
                    blue += sum(chunk[2::4]) * 0xFF
                    alpha += 0xFF * columns
                else:
                    for column in range(columns):
                        at = column * 4
                        weight = chunk[at + 3]
                        red += chunk[at] * weight
                        green += chunk[at + 1] * weight
                        blue += chunk[at + 2] * weight
                        alpha += weight

            samples = (y1 - y0) * columns
            target = (ty * target_width + tx) * 4
            # 每个通道值都不超过 0xFF，故 ``red <= 0xFF * alpha`` 恒成立，
            # 整数除不会越界；alpha 为 0 时三个颜色通道保持 0。
            if alpha:
                out[target] = red // alpha
                out[target + 1] = green // alpha
                out[target + 2] = blue // alpha
            # ``alpha <= 0xFF * samples``，故四舍五入后仍在 0..255 内。
            out[target + 3] = (alpha + samples // 2) // samples

    return target_width, target_height, bytes(out)


def load_icon_rgba(
    path: str | Path,
    max_extent: int | None = None,
) -> tuple[int, int, bytes]:
    """读取一个 PNG 并（可选）缩放到托盘可用的尺寸。

    Args:
        path: PNG 文件路径。
        max_extent: 给定正数时，把最长边等比缩到该值以内；``None`` 或 ``<= 0``
            表示按原尺寸返回。

    Returns:
        ``(width, height, rgba)``，含义同 :func:`decode_png_rgba`。

    Raises:
        PngError: 与 :func:`decode_png_rgba` 相同，外加 :func:`downsample_rgba`
            的校验错误。
    """
    width, height, rgba = decode_png_rgba(path)
    if max_extent is None:
        return width, height, rgba
    return downsample_rgba(width, height, rgba, max_extent)


if __name__ == "__main__":  # pragma: no cover - 手工核对用
    import sys

    for argument in sys.argv[1:]:
        width, height, rgba = decode_png_rgba(argument)
        print(f"{argument}: {width}x{height}, {len(rgba)} bytes")
