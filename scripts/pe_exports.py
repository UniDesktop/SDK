#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Parse the export directory of a PE shared library and list ``uda_*`` symbols.

A cross-compiled ``uda_ffi.dll`` cannot be inspected with the Linux ``nm``
tool (the mingw build of ``nm`` reports "no symbols" for DLLs produced by the
Rust toolchain). This script therefore reads the PE export table directly, so
the ``#[no_mangle]`` export list can be verified on machines that cannot run
Windows binaries at all.

Usage::

    python scripts/pe_exports.py path/to/uda_ffi.dll

Every string this script prints is plain ASCII. GitHub's Windows runners
default to a ``cp1252`` console code page, which cannot encode CJK text and
would raise ``UnicodeEncodeError`` on any non-ASCII output, so the diagnostics
below stay ASCII-only and stdout is re-configured to UTF-8 as a defensive
measure for the rare case a non-ASCII path is echoed back.
"""

from __future__ import annotations

import pathlib
import struct
import sys

#: Force UTF-8 output with a ``replace`` fallback so that a non-ASCII path echoed
#: by the caller cannot trip the console code page (for example ``cp1252`` on the
#: GitHub Windows runner). ``errors="replace"`` guarantees the encode step never
#: raises, so a diagnostic can never abort a CI step.
for _stream_name in ("stdout", "stderr"):
    _stream = getattr(sys, _stream_name, None)
    if _stream is None:
        continue
    _reconfigure = getattr(_stream, "reconfigure", None)
    if _reconfigure is not None:
        try:
            _reconfigure(encoding="utf-8", errors="replace")
        except (ValueError, OSError, LookupError):  # pragma: no cover - hostile console
            pass
        continue
    # Replaced / legacy stream (some IDEs and harnesses swap stdout). Re-wrap the
    # underlying buffer so UTF-8 text still reaches the terminal.
    _buffer = getattr(_stream, "buffer", None)
    if _buffer is None:
        continue
    try:
        import io

        setattr(
            sys,
            _stream_name,
            io.TextIOWrapper(
                _buffer,
                encoding="utf-8",
                errors="replace",
                line_buffering=getattr(_stream, "line_buffering", False),
            ),
        )
    except (ValueError, OSError):  # pragma: no cover - hostile console
        pass

#: Offset of the data-directory table inside a PE32+ optional header (magic 0x20B).
_PE32_PLUS_DATA_DIRECTORY_OFFSET = 112
#: Index of the export table inside the data-directory table.
_EXPORT_DIRECTORY_INDEX = 0


def _read_sections(data: bytes, optional_header: int, optional_size: int) -> list[tuple[int, int, int]]:
    """Return the ``(virtual address, virtual size, file offset)`` of each section."""
    sections: list[tuple[int, int, int]] = []
    for index in range(struct.unpack_from("<H", data, optional_header - 4)[0]):
        offset = optional_header + optional_size + index * 40
        virtual_size, virtual_address, _raw_size, raw_pointer = struct.unpack_from(
            "<IIII", data, offset + 8
        )
        sections.append((virtual_address, virtual_size, raw_pointer))
    return sections


def _exported_names(data: bytes) -> list[str]:
    """Parse the export directory and return every exported function name."""
    if data[:2] != b"MZ":
        raise ValueError("not a PE file (missing MZ signature)")

    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe_offset : pe_offset + 4] != b"PE\x00\x00":
        raise ValueError("not a PE file (missing PE signature)")

    # COFF header: +2 is the section count and +16 the optional-header size.
    # The section count is not needed here because the section-table walk stops
    # on its own, so only the optional-header size is read.
    coff = pe_offset + 4
    optional_size = struct.unpack_from("<H", data, coff + 16)[0]
    optional_header = coff + 20

    magic = struct.unpack_from("<H", data, optional_header)[0]
    if magic != 0x20B:
        raise ValueError(f"only PE32+ is supported, found magic 0x{magic:x}")

    data_directory = optional_header + _PE32_PLUS_DATA_DIRECTORY_OFFSET
    export_rva, _export_size = struct.unpack_from(
        "<II", data, data_directory + _EXPORT_DIRECTORY_INDEX * 8
    )
    if export_rva == 0:
        raise ValueError("this PE file has no export directory")

    sections = _read_sections(data, optional_header, optional_size)

    def rva_to_offset(rva: int) -> int | None:
        for virtual_address, virtual_size, raw_pointer in sections:
            if virtual_address <= rva < virtual_address + max(virtual_size, 1):
                return raw_pointer + (rva - virtual_address)
        return None

    export_offset = rva_to_offset(export_rva)
    if export_offset is None:
        raise ValueError("export directory RVA does not map to a file offset")

    name_count = struct.unpack_from("<I", data, export_offset + 24)[0]
    names_rva = struct.unpack_from("<I", data, export_offset + 32)[0]
    names_offset = rva_to_offset(names_rva)
    if names_offset is None:
        raise ValueError("export name table RVA does not map to a file offset")

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
        print(f"usage: {argv[0]} <path-to-dll>", file=sys.stderr)
        return 2

    path = pathlib.Path(argv[1])
    if not path.is_file():
        print(f"file not found: {path}", file=sys.stderr)
        return 2

    try:
        names = _exported_names(path.read_bytes())
    except (OSError, ValueError) as error:
        print(f"parse failed: {error}", file=sys.stderr)
        return 1

    uda_names = sorted(name for name in names if name.startswith("uda_"))
    print(f"total exported functions: {len(names)}")
    print(f"uda_* symbols ({len(uda_names)}):")
    for name in uda_names:
        print(f"   {name}")
    return 0 if uda_names else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
