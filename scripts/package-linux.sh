#!/usr/bin/env bash
# Backward-compatible entry point for origin/linux's documented local command.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    printf 'This wrapper requires Linux x86_64.\n' >&2
    exit 1
fi
cd "$root"
cargo build --locked --release --workspace --bins
exec python3 scripts/package.py package --target x86_64-unknown-linux-gnu \
    --bin-dir "${CARGO_TARGET_DIR:-$root/target}/release" "$@"
