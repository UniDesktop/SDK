#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""List the ``uda_*`` function names declared in a C header.

The public header (``include/uda.h``) is the single source of truth for the
exported ABI surface: every ``#[no_mangle]`` symbol in ``crates/uda-ffi`` must be
declared there, and every declaration must be a real export of the built shared
library. Deriving the expected symbol set from the header means a newly added
export can never silently fail a check that hard-codes a stale count, which is
what broke the release pipeline.

Usage::

    python scripts/header_symbols.py include/uda.h

One sorted symbol per line, so the output is easy to consume in a shell loop::

    mapfile -t expected < <(python3 scripts/header_symbols.py include/uda.h)

Only *declarations* are matched. A declaration starts with a return type and
continues to ``uda_name(``; the closing ``);`` may be on a later line for the
multi-argument exports. Prose in the doc comments above each declaration, the
``#define`` constant blocks and the callback ``typedef`` lines never contribute
a name.
"""

from __future__ import annotations

import pathlib
import re
import sys

#: A declaration's signature: a return type (possibly `const`-qualified and
#: pointer-starred), then the symbol name and its opening parameter list. The
#: return type may be `void`, `int32_t`, `const char *`, `uint64_t`, ... so the
#: pattern anchors on the *name* and only requires that something type-like
#: precede it on the same line.
_DECLARATION = re.compile(
    r"^[A-Za-z_][A-Za-z0-9_]*"  # first token of the type, e.g. `int32_t`, `const`, `void`
    r"(?:\s+[A-Za-z_][A-Za-z0-9_]*)*"  # further type words, e.g. `char`
    r"\s*\**\s*"  # pointer stars, e.g. the `*` in `const char *`
    r"\b(uda_[a-z0-9_]+)\s*\("
)


def header_symbols(header: pathlib.Path) -> list[str]:
    """Return the sorted ``uda_*`` function names declared in ``header``."""
    names: set[str] = set()
    for line in header.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        # Skip block-comment continuation, preprocessor directives and line
        # comments outright: none of them can hold a declaration.
        if stripped.startswith(("*", "/", "#")):
            continue
        match = _DECLARATION.match(line)
        if match:
            names.add(match.group(1))
    return sorted(names)


def _write_lines(lines: list[str]) -> None:
    """Print ``lines`` with LF endings, whatever platform this runs on.

    On Windows the text stream translates ``\n`` to ``\r\n``. A trailing ``\r``
    is invisible in a terminal but breaks every shell consumer that anchors with
    ``$`` (for example ``grep -E " uda_x$"``), so the byte stream is used here and
    every ``\r`` is stripped explicitly.
    """
    for line in lines:
        sys.stdout.buffer.write(f"{line}\n".encode("utf-8"))
    sys.stdout.buffer.flush()


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(f"usage: {argv[0]} <path-to-header>", file=sys.stderr)
        return 2

    header = pathlib.Path(argv[1])
    if not header.is_file():
        print(f"header not found: {header}", file=sys.stderr)
        return 2

    names = header_symbols(header)
    if not names:
        print(f"error: no uda_* declarations found in {header}", file=sys.stderr)
        return 1

    _write_lines(names)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
