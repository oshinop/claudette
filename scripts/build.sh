#!/bin/sh
# Builds claudette's release binaries into dist/claudette-<platform>.
#   scripts/build.sh                    every platform this machine can build: all four on macOS,
#                                       the two Linux ones elsewhere
#   scripts/build.sh linux-x64 ...      only these (darwin-arm64, darwin-x64, linux-arm64, linux-x64)
# macOS builds use the Apple toolchain, so they need a Mac. Linux builds are static (musl) and linked
# by zig through cargo-zigbuild: `brew install zig && cargo install --locked cargo-zigbuild`.
set -eu
cd "$(dirname "$0")/.."

target() {
  case "$1" in
    darwin-arm64) echo aarch64-apple-darwin ;;
    darwin-x64) echo x86_64-apple-darwin ;;
    linux-arm64) echo aarch64-unknown-linux-musl ;;
    linux-x64) echo x86_64-unknown-linux-musl ;;
    *) echo "unknown platform $1: use darwin-arm64, darwin-x64, linux-arm64 or linux-x64" >&2; return 2 ;;
  esac
}

if [ "$#" -eq 0 ]; then
  if [ "$(uname -s)" = Darwin ]; then
    set -- darwin-arm64 darwin-x64 linux-arm64 linux-x64
  else
    set -- linux-arm64 linux-x64
  fi
fi

for platform in "$@"; do
  target "$platform" >/dev/null
  case "$platform" in
    darwin-*) [ "$(uname -s)" = Darwin ] || { echo "$platform needs a Mac: macOS builds use the Apple toolchain" >&2; exit 1; } ;;
    *)
      if ! command -v zig >/dev/null || ! command -v cargo-zigbuild >/dev/null; then
        echo "Linux builds need zig and cargo-zigbuild: brew install zig && cargo install --locked cargo-zigbuild" >&2
        exit 1
      fi ;;
  esac
done

mkdir -p dist
for platform in "$@"; do
  t=$(target "$platform")
  rustup target add "$t" >/dev/null 2>&1
  case "$platform" in
    darwin-*) cargo build --release --locked --target "$t" ;;
    *) cargo zigbuild --release --locked --target "$t" ;;
  esac
  install -m 755 "target/$t/release/claudette" "dist/claudette-$platform"
done
ls -l dist
