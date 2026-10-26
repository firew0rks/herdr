#!/usr/bin/env bash
# Install this fork as ~/.local/bin/herdr, ahead of any packaged herdr.
# Default: the latest fork release (Linux x86_64, macOS arm64) from GitHub.
# --build: build this checkout instead; needs Rust and Zig 0.16 (brew install zig@0.16, or set ZIG).
# Cut a release with: git tag fork-v<version>.<n> && git push origin fork-v<version>.<n>
set -euo pipefail
cd "$(dirname "$0")/.."

dest=~/.local/bin/herdr
mkdir -p ~/.local/bin

if [[ ${1:-} == --build ]]; then
  if [[ -z ${ZIG:-} ]]; then
    for candidate in "$(brew --prefix zig@0.16 2>/dev/null)/bin/zig" "$(command -v zig || true)"; do
      if [[ -x $candidate && $("$candidate" version) == 0.16.* ]]; then ZIG=$candidate; break; fi
    done
  fi
  [[ -n ${ZIG:-} ]] || { echo "Zig 0.16 not found; install it or set ZIG." >&2; exit 1; }
  export ZIG
  cargo build --release --locked
  cp target/release/herdr "$dest.new"
  from="checkout $(git rev-parse --short HEAD)"
else
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) asset=herdr-linux-x86_64 ;;
    Darwin-arm64) asset=herdr-macos-aarch64 ;;
    *) echo "No fork release for $(uname -s)-$(uname -m); use --build." >&2; exit 1 ;;
  esac
  url=https://github.com/firew0rks/herdr/releases/latest/download/$asset
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL "$url" -o "$tmp/$asset"
  curl -fsSL "$url.sha256" -o "$tmp/$asset.sha256"
  sha256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }
  (cd "$tmp" && sha256 -c "$asset.sha256" >/dev/null)
  chmod +x "$tmp/$asset"
  # macOS quarantines downloads; this binary is our own build.
  [[ $(uname -s) == Darwin ]] && xattr -c "$tmp/$asset" 2>/dev/null || true
  cp "$tmp/$asset" "$dest.new"
  from="release $(curl -fsSLI -o /dev/null -w '%{url_effective}' https://github.com/firew0rks/herdr/releases/latest | sed 's|.*/||')"
fi

# Copy then rename, so a running server keeps its binary intact.
mv -f "$dest.new" "$dest"
echo "installed $("$dest" --version) ($from)"

resolved=$(command -v herdr || true)
if [[ $resolved != "$HOME/.local/bin/herdr" ]]; then
  echo "warning: 'herdr' resolves to ${resolved:-nothing}; put ~/.local/bin first in PATH or remove the packaged herdr." >&2
fi
