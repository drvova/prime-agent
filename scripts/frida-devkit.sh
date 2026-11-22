#!/usr/bin/env bash
# Get the frida-core devkit (static library + header) a `--features frida`
# build links, for one Rust target, verify it against the pinned checksum,
# and print the directory to pass as FRIDA_DEVKIT_DIR.
#
#   scripts/frida-devkit.sh [target-triple] [out-dir]
#
# Run by `make frida-devkit` and `make frida-test`. The version must be the
# one the pinned `frida-sys` crate binds (its FRIDA_VERSION file); a devkit of
# another version has a different header and ABI. Upstream publishes no devkit
# for other targets (for example *-windows-gnu and *-freebsd), and its
# linux-musl devkits are not pinned here because no musl target ships.
set -euo pipefail

VERSION=17.9.5
target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
case "$target" in
  x86_64-unknown-linux-gnu)
    name=linux-x86_64
    sum=e98803f6cad21c41a1b67eaa495963955afab6ed7ae95a18dc485ab2188bb021 ;;
  aarch64-unknown-linux-gnu)
    name=linux-arm64
    sum=87854f75f8b3f9ad7799521a2f86184f1842b1525987fbb5b7d7e1193f797bb3 ;;
  aarch64-apple-darwin)
    name=macos-arm64
    sum=9b2e4e420604c7c9929cf7e4dd4b4cdb4c1cd10e619c0b8290ea21a118ce82ee ;;
  x86_64-apple-darwin)
    name=macos-x86_64
    sum=cbf2181fea84b608858317466753bd274d446033931ae7b63b75fb53977c7bc1 ;;
  x86_64-pc-windows-msvc)
    name=windows-x86_64
    sum=21316e63575ca8af3d9baacebc849f8127929645f4522395c947bd52951ca903 ;;
  *)
    echo "frida-devkit: no pinned frida-core devkit for target $target" >&2
    exit 1 ;;
esac

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="${2:-$root/target/frida-devkit/$VERSION/$target}"
if [ ! -f "$out/frida-core.h" ]; then
  mkdir -p "$out"
  archive="$out/devkit.tar.xz"
  curl --fail --silent --show-error --location --output "$archive" \
    "https://github.com/frida/frida/releases/download/$VERSION/frida-core-devkit-$VERSION-$name.tar.xz"
  if command -v sha256sum >/dev/null; then
    actual="$(sha256sum "$archive" | cut -d' ' -f1)"
  else
    actual="$(shasum -a 256 "$archive" | cut -d' ' -f1)"
  fi
  if [ "$actual" != "$sum" ]; then
    rm -f "$archive"
    echo "frida-devkit: checksum mismatch for $name: got $actual, pinned $sum" >&2
    exit 1
  fi
  tar -xJf "$archive" -C "$out"
  rm -f "$archive"
fi
echo "$out"
