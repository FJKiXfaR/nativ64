#!/bin/sh
# Format the whole workspace with the toolchain CI uses (the latest stable).
# With rustup, "+stable" pins it even if the directory overrides the toolchain.
# Pass --check to verify only.
if command -v rustup >/dev/null 2>&1; then
    exec cargo +stable fmt --all "$@"
fi
exec cargo fmt --all "$@"
