#!/usr/bin/env bash
# Builds target/release/Sound-Babound.app: a minimal app bundle, so macOS
# shows the microphone prompt and remembers the answer (TCC binds the
# permission to the bundle's Info.plist and its signature).
#
# The signature is ad hoc (`codesign --sign -`), not Developer ID: the bundle
# runs on the machine that built it, not as a notarized download.
#
# Builds only; it opens no audio device.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "bundle-macos.sh: macOS only" >&2
    exit 1
fi

cargo build --release -p sound-babound

# `cargo pkgid` prints e.g. `path+file:///…/crates/app#sound-babound@0.1.0`.
pkgid="$(cargo pkgid -p sound-babound)"
version="${pkgid##*[#@]}"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]; then
    echo "bundle-macos.sh: cannot read the version from '$pkgid'" >&2
    exit 1
fi

target_dir="${CARGO_TARGET_DIR:-$repo/target}"
app="$target_dir/release/Sound-Babound.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$target_dir/release/sound-babound" "$app/Contents/MacOS/sound-babound"
sed "s/@VERSION@/$version/g" crates/app/macos/Info.plist >"$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"

plutil -lint "$app/Contents/Info.plist"
codesign --force --sign - "$app"
codesign --verify --strict "$app"

echo "built $app ($version)"
