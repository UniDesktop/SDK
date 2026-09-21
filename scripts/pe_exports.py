#!/usr/bin/env python3
"""解析 PE 动态库的导出目录，列出导出的 ``uda_*`` 符号。

交叉编译出的 ``uda_ffi.dll`` 无法用 Linux 侧 ``nm`` 解析（mingw 的 ``nm`` 对
Rust 产出的 DLL 报「无符号」），因此这里直接读取 PE 结构中的导出表，用于在
无法运行 Windows 的环境里验证 ``#[no_mangle]`` 符号确实被导出。

用法::

    python3 scripts/pe_exports.py path/to/uda_ffi.dll
"""

from __future__ import annotations

import pathlib
import struct
import sys

#: PE32+ 可选头中数据目录表的起始偏移（魔法 0x20B）。
_PE32_PLUS_DATA_DIRECTORY_OFFSET = 112
#: 数据目录中导出表的索引。
_EXPORT_DIRECTORY_INDEX = 0


def _read_sections(data: bytes, optional_header: int, optional_size: int) -> list[tuple[int, int, int]]:
    """返回 ``(虚拟地址, 虚拟大小, 文件偏移)`` 三元组列表。"""
    sections: list[tuple[int, int, int]] = []
    for index in range(struct.unpack_from("<H", data, optional_header - 4)[0]):
        offset = optional_header + optional_size + index * 40
        virtual_size, virtual_address, _raw_size, raw_pointer = struct.unpack_from(
            "<IIII", data, offset + 8
        )
        sections.append((virtual_address, virtual_size, raw_pointer))
    return sections


def _exported_names(data: bytes) -> list[str]:
    """解析导出目录，返回全部导出的函数名。"""
    if data[:2] != b"MZ":
        raise ValueError("不是 PE 文件（缺少 MZ 签名）")

    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe_offset : pe_offset + 4] != b"PE\x00\x00":
        raise ValueError("不是 PE 文件（缺少 PE 签名）")

    coff = pe_offset + 4
    section_count = struct.unpack_from("<H", data, coff + 2)[0]
    optional_size = struct.unpack_from("<H", data, coff + 16)[0]
    optional_header = coff + 20

    magic = struct.unpack_from("<H", data, optional_header)[0]
    if magic != 0x20B:
        raise ValueError(f"仅支持 PE32+，实际魔法为 0x{magic:x}")

    data_directory = optional_header + _PE32_PLUS_DATA_DIRECTORY_OFFSET
    export_rva, _export_size = struct.unpack_from(
        "<II", data, data_directory + _EXPORT_DIRECTORY_INDEX * 8
    )
    if export_rva == 0:
        raise ValueError("该 PE 文件没有导出目录")

    sections = _read_sections(data, optional_header, optional_size)

    def rva_to_offset(rva: int) -> int | None:
        for virtual_address, virtual_size, raw_pointer in sections:
            if virtual_address <= rva < virtual_address + max(virtual_size, 1):
                return raw_pointer + (rva - virtual_address)
        return None

    export_offset = rva_to_offset(export_rva)
    if export_offset is None:
        raise ValueError("导出目录 RVA 无法映射到文件偏移")

    name_count = struct.unpack_from("<I", data, export_offset + 24)[0]
    names_rva = struct.unpack_from("<I", data, export_offset + 32)[0]
    names_offset = rva_to_offset(names_rva)
    if names_offset is None:
        raise ValueError("导出名称表 RVA 无法映射到文件偏移")

    names: list[str] = []
    for index in range(name_count):
        name_rva = struct.unpack_from("<I", data, names_offset + index * 4)[0]
        name_offset = rva_to_offset(name_rva)
        if name_offset is None:
            continue
        terminator = data.index(b"\x00", name_offset)
        names.append(data[name_offset:terminator].decode("ascii"))
    return names


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(f"用法: {argv[0]} <path-to-dll>", file=sys.stderr)
        return 2

    path = pathlib.Path(argv[1])
    if not path.is_file():
        print(f"文件不存在: {path}", file=sys.stderr)
        return 2

    try:
        names = _exported_names(path.read_bytes())
    except (OSError, ValueError) as error:
        print(f"解析失败: {error}", file=sys.stderr)
        return 1

    uda_names = sorted(name for name in names if name.startswith("uda_"))
    print(f"导出函数总数: {len(names)}")
    print(f"uda_* 符号 ({len(uda_names)}):")
    for name in uda_names:
        print(f"   {name}")
    return 0 if uda_names else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
