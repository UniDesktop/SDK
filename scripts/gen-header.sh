#!/usr/bin/env bash
# Regenerate include/uda.h from the Rust C-ABI surface via cbindgen.
#
# include/uda.h is a GENERATED file: never edit it by hand. Edit the doc
# comments on the exported functions in crates/uda-ffi, or the constants and
# callback types in crates/uda-ffi/src/abi.rs, then re-run this script and
# commit the header together with the Rust change.
#
# CI re-runs this script in --check mode (see .github/workflows/ci.yml), so a
# stale header fails the build instead of drifting silently.
#
# Usage:
#   scripts/gen-header.sh           # rewrite include/uda.h in place
#   scripts/gen-header.sh --check   # diff only; exit 1 when the header is stale
set -euo pipefail

usage() {
    echo "Usage: scripts/gen-header.sh [--check]" >&2
    echo "  --check   compare include/uda.h with fresh cbindgen output; exit 1 if stale" >&2
    echo "  (default) rewrite include/uda.h in place" >&2
}

if [ "$#" -gt 1 ]; then
    echo "error: expected at most one argument, got $#" >&2
    usage
    exit 2
fi

check=false
case "${1:-}" in
    "") ;;
    --check) check=true ;;
    *)
        echo "error: unknown argument: $1" >&2
        usage
        exit 2
        ;;
esac

# Keep in sync with .github/workflows/ci.yml and crates/uda-ffi/cbindgen.toml:
# output can differ between cbindgen releases, so every consumer uses the same
# pinned version.
CBINDGEN_VERSION="0.29.4"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HEADER="$ROOT/include/uda.h"
CONFIG="$ROOT/crates/uda-ffi/cbindgen.toml"

if ! command -v cbindgen >/dev/null 2>&1; then
    echo "cbindgen not found; installing pinned v${CBINDGEN_VERSION} ..." >&2
    cargo install cbindgen --locked --version "$CBINDGEN_VERSION"
fi

# `cargo install` can succeed while its bin directory (CARGO_HOME/bin) is not
# on PATH; fail with an actionable message instead of a bare "command not
# found" from the version probe below.
if ! command -v cbindgen >/dev/null 2>&1; then
    echo "error: cbindgen is still not on PATH after installing it." >&2
    echo "       Add the cargo bin directory (usually ~/.cargo/bin) to PATH." >&2
    exit 1
fi

installed="$(cbindgen --version | awk '{print $2}')"
if [ "$installed" != "$CBINDGEN_VERSION" ]; then
    echo "warning: cbindgen v${installed} installed, but the header is generated" >&2
    echo "         with v${CBINDGEN_VERSION}; output can differ between versions." >&2
    echo "         Consider: cargo install cbindgen --locked --version ${CBINDGEN_VERSION}" >&2
fi

# Keep the scratch file inside the workspace (`.tmp/` is gitignored): a
# `$TMPDIR` write can trip strict sandboxing setups, and every other throwaway
# artifact in this repo lands in `.tmp/` too.
mkdir -p "$ROOT/.tmp"
output="$(mktemp "$ROOT/.tmp/uda-header.XXXXXX")"
trap 'rm -f "$output"' EXIT

# Resolve the crate through the workspace so the call works from any directory.
(cd "$ROOT" && cbindgen --config "$CONFIG" --crate uda-ffi --output "$output")

# cbindgen renders negative constants bare (e.g. "#define UDA_ERR_IO -4"),
# which would turn a caller's "-UDA_ERR_IO" into the token "--4". Wrap
# negative literals in parentheses the way the header always has.
perl -pi -e 's/^(#define UDA_[A-Z0-9_]+) (-\d+)$/$1 ($2)/' "$output"

if [ "$check" = true ]; then
    if ! diff -u "$HEADER" "$output"; then
        echo
        echo "error: include/uda.h is stale. Re-run scripts/gen-header.sh and" >&2
        echo "       commit the regenerated header with your Rust change." >&2
        exit 1
    fi
    echo "include/uda.h is up to date."
else
    cp "$output" "$HEADER"
    echo "regenerated $HEADER"
fi
