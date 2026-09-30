#!/usr/bin/env bash
set -euo pipefail

out_dir="${1:-dist}"
mkdir -p "$out_dir"

echo "Building/caching Linux dependencies..."
nix build .#cross-linux-x86_64-deps --no-link

echo "Building/caching Windows dependencies..."
nix build .#cross-windows-x86_64-deps --no-link

echo "Building portable Linux distribution artifact..."
linux_out="$(nix build .#dist-linux-x86_64 --no-link --print-out-paths)"

echo "Building Windows distribution artifact..."
windows_out="$(nix build .#dist-windows-x86_64 --no-link --print-out-paths)"

cp "$linux_out"/abird-link-linux-x86_64* "$out_dir/"
cp "$windows_out"/abird-link-windows-x86_64.exe* "$out_dir/"

printf '\nRelease artifacts:\n'
ls -lh "$out_dir"
