#!/usr/bin/env python3
"""Direct regression checks for generated C semantics and the version gate.

Run with: python3 -B scripts/test-header.py
Requires C11 and C++11 compilers (CC/CXX, defaulting to cc/c++).
"""

import os
import re
import shlex
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
HEADER = ROOT / "include" / "uda.h"

#: The canonical session-capability bits, part of the C ABI (see
#: `crates/uda-ffi/src/abi.rs`). The header is parsed and compared against this
#: map: a renumbered, renamed or dropped bit fails here, so the test proves the
#: header against an independent expectation instead of against itself. Adding
#: a capability therefore means updating both `abi.rs` and this map - which is
#: the point, since the numbers are a promise to C callers.
EXPECTED_CAPABILITIES = {
    "MANAGEMENT": 16,
    "LOCK": 17,
    "LOGOUT": 18,
    "SUSPEND": 19,
    "HIBERNATE": 20,
    "REBOOT": 21,
    "SHUTDOWN": 22,
}

#: A capability define exactly as cbindgen emits it: a name, then a 32-bit
#: unsigned shift expression. Leading whitespace and whitespace after the `#`
#: are allowed so a reformatted define (an `#if` block, a future formatter
#: style) is still parsed rather than silently skipped.
CAPABILITY_DEFINE = re.compile(
    r"^\s*#\s*define\s+(UDA_SESSION_CAP_[A-Z0-9_]+)\s+\(1u\s*<<\s*(\d+)\)\s*$",
    re.MULTILINE,
)
#: Any capability define, whatever its value shape, so a differently spelled
#: value cannot silently evade the strict parser above.
CAPABILITY_NAME = re.compile(
    r"^\s*#\s*define\s+(UDA_SESSION_CAP_[A-Z0-9_]+)\b", re.MULTILINE
)


def parse_capabilities(text):
    """Return ``{macro: bit}`` for every ``UDA_SESSION_CAP_*`` define in ``text``.

    Raises ``AssertionError`` when a define does not have the expected
    ``#define NAME (1u << BIT)`` shape or when a name is defined twice, so a
    malformed capability can never silently vanish from the parsed map.
    """
    parsed = CAPABILITY_DEFINE.findall(text)
    declared = CAPABILITY_NAME.findall(text)
    if not declared:
        raise AssertionError("no UDA_SESSION_CAP_* defines found")
    if len(parsed) != len(declared):
        raise AssertionError(
            "a UDA_SESSION_CAP_* define does not have the expected "
            f"'#define NAME (1u << BIT)' shape: {declared!r}"
        )
    macros = {name: int(bit) for name, bit in parsed}
    if len(macros) != len(parsed):
        raise AssertionError(f"duplicate UDA_SESSION_CAP_* defines: {declared!r}")
    return macros


def capability_macros():
    """Return sorted ``(macro, bit)`` pairs, checked against the ABI map."""
    macros = parse_capabilities(HEADER.read_text(encoding="utf-8"))
    expected = {
        f"UDA_SESSION_CAP_{name}": bit
        for name, bit in EXPECTED_CAPABILITIES.items()
    }
    if macros != expected:
        raise AssertionError(
            "the header's capability defines disagree with the canonical ABI "
            f"map (a bit was renumbered, renamed or dropped): "
            f"header={macros!r} expected={expected!r}"
        )
    return sorted(macros.items(), key=lambda item: item[1])


class HeaderRegressionTests(unittest.TestCase):
    def test_capability_parser_accepts_reformatted_defines(self):
        # The strict shape check must not turn a reformatted define into a
        # silent skip: indentation and a space after `#` are both legal C.
        text = (
            "#define UDA_SESSION_CAP_LOCK (1u << 17)\n"
            "  #  define UDA_SESSION_CAP_REBOOT (1u << 21)\n"
        )
        self.assertEqual(
            parse_capabilities(text),
            {"UDA_SESSION_CAP_LOCK": 17, "UDA_SESSION_CAP_REBOOT": 21},
        )

    def test_capability_parser_rejects_a_define_with_a_different_shape(self):
        # A capability re-spelled with a different value must fail loudly rather
        # than disappear from the parsed map: the value-agnostic name pattern
        # still sees it, so the count mismatch trips the assertion.
        with self.assertRaises(AssertionError):
            parse_capabilities("#define UDA_SESSION_CAP_LOCK (2u << 17)\n")

    def test_capability_macros_preserve_unsigned_c_and_cpp_semantics(self):
        for language, variable, default, standard in (
            ("c", "CC", "cc", "c11"),
            ("c++", "CXX", "c++", "c++11"),
        ):
            with self.subTest(language=language):
                lines = ['#include "uda.h"']
                if language == "c":
                    assertion = "_Static_assert"
                    unsigned_type = "_Generic(({macro}), unsigned int: 1, default: 0)"
                else:
                    lines.append("#include <type_traits>")
                    assertion = "static_assert"
                    unsigned_type = "std::is_same<decltype({macro}), unsigned int>::value"
                for macro, bit in capability_macros():
                    lines.extend(
                        (
                            f'{assertion}({unsigned_type.format(macro=macro)}, "{macro} type");',
                            f'{assertion}({macro} == {1 << bit}u, "{macro} value");',
                            # Type-width-derived: if the macro stops being a
                            # 32-bit unsigned value this complement comparison
                            # fails loudly instead of silently testing a
                            # hard-coded 0xFFFFFFFF.
                            f'{assertion}((uint64_t)(~{macro}) == (uint64_t)(~0u ^ (1u << {bit})), "{macro} complement");',
                        )
                    )
                compiler = shlex.split(os.environ.get(variable, default))
                result = subprocess.run(
                    compiler
                    + [f"-std={standard}", "-Werror", "-fsyntax-only", "-x", language,
                       "-I", str(HEADER.parent), "-"],
                    input="\n".join(lines) + "\n",
                    text=True,
                    capture_output=True,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_callback_typedefs_are_usable_from_cpp(self):
        # cbindgen emits the two callback typedefs before the extern "C" block.
        # That does not change the exported function symbols, but a C++ consumer
        # must still be able to assign an `extern "C"` function to each typedef
        # exactly as the header documents.
        source = "\n".join(
            (
                '#include "uda.h"',
                'extern "C" void uya_text(uint64_t, void *);',
                'extern "C" void uya_checkbox(uint64_t, int32_t, void *);',
                "UdaTrayTextCallback text_callback = uya_text;",
                "UdaTrayCheckboxCallback checkbox_callback = uya_checkbox;",
                "int main() { return text_callback == nullptr || checkbox_callback == nullptr; }",
            )
        ) + "\n"
        compiler = shlex.split(os.environ.get("CXX", "c++"))
        result = subprocess.run(
            compiler
            + ["-std=c++11", "-Werror", "-pedantic", "-fsyntax-only", "-x", "c++",
               "-I", str(HEADER.parent), "-"],
            input=source,
            text=True,
            capture_output=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_status_alias_stays_rust_only(self):
        self.assertNotRegex(HEADER.read_text(), r"\bUdaStatus\b")

    def test_wrong_generator_version_exits_before_generation(self):
        scratch = ROOT / ".tmp"
        scratch.mkdir(exist_ok=True)
        header_before = HEADER.read_bytes()
        with tempfile.TemporaryDirectory(prefix="header-test-", dir=scratch) as directory:
            stub_dir = Path(directory)
            calls = stub_dir / "calls"
            stub = stub_dir / "cbindgen"
            stub.write_text(
                '#!/bin/sh\n'
                'printf "%s\\n" "$*" >> "$CBINDGEN_TEST_CALLS"\n'
                'if [ "$#" -eq 1 ] && [ "$1" = "--version" ]; then\n'
                '    printf "cbindgen 0.29.3\\n"\n'
                '    exit 0\n'
                'fi\n'
                'printf "generation reached\\n" >&2\n'
                'exit 99\n'
            )
            stub.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = str(stub_dir) + os.pathsep + env.get("PATH", "")
            env["CBINDGEN_TEST_CALLS"] = str(calls)
            for arguments in ([], ["--check"]):
                with self.subTest(arguments=arguments):
                    calls.write_text("")
                    result = subprocess.run(
                        ["bash", str(ROOT / "scripts" / "gen-header.sh"), *arguments],
                        env=env,
                        text=True,
                        capture_output=True,
                    )
                    self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                    self.assertIn("error: cbindgen v0.29.3 installed", result.stderr)
                    self.assertIn("requires v0.29.4", result.stderr)
                    self.assertEqual(calls.read_text(), "--version\n")
                    self.assertEqual(HEADER.read_bytes(), header_before)


if __name__ == "__main__":
    unittest.main()
