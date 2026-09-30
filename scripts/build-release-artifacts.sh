#!/usr/bin/env bash
set -euo pipefail

out_dir="${1:-dist}"
mkdir -p "$out_dir"

nix_cmd=(nix --extra-experimental-features "nix-command flakes")

echo "Building/caching Linux dependencies..."
"${nix_cmd[@]}" build .#cross-linux-x86_64-deps --no-link

echo "Building/caching Windows dependencies..."
"${nix_cmd[@]}" build .#cross-windows-x86_64-deps --no-link

echo "Building portable Linux distribution artifact..."
linux_out="$("${nix_cmd[@]}" build .#dist-linux-x86_64 --no-link --print-out-paths)"

echo "Building Windows distribution artifact..."
windows_out="$("${nix_cmd[@]}" build .#dist-windows-x86_64 --no-link --print-out-paths)"

cp "$linux_out"/dotlink-linux-x86_64* "$out_dir/"
cp "$windows_out"/dotlink-windows-x86_64.exe* "$out_dir/"

printf '\nRelease artifacts:\n'
ls -lh "$out_dir"
