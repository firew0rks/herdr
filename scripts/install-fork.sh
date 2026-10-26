#!/usr/bin/env bash
# Build this fork and install it as ~/.local/bin/herdr, ahead of any packaged herdr.
# Needs the Rust toolchain and Zig 0.16 (brew install zig@0.16, or set ZIG).
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ -z ${ZIG:-} ]]; then
  for candidate in "$(brew --prefix zig@0.16 2>/dev/null)/bin/zig" "$(command -v zig || true)"; do
    if [[ -x $candidate && $("$candidate" version) == 0.16.* ]]; then ZIG=$candidate; break; fi
  done
fi
[[ -n ${ZIG:-} ]] || { echo "Zig 0.16 not found; install it or set ZIG." >&2; exit 1; }
export ZIG

cargo build --release --locked
mkdir -p ~/.local/bin
# Copy then rename, so a running server keeps its binary intact.
cp target/release/herdr ~/.local/bin/herdr.new
mv -f ~/.local/bin/herdr.new ~/.local/bin/herdr
echo "installed $(~/.local/bin/herdr --version) ($(git rev-parse --short HEAD))"

resolved=$(command -v herdr || true)
if [[ $resolved != "$HOME/.local/bin/herdr" ]]; then
  echo "warning: 'herdr' resolves to ${resolved:-nothing}; put ~/.local/bin first in PATH or remove the packaged herdr." >&2
fi
