#!/usr/bin/env bash
set -euo pipefail

out_dir="${1:-dist}"
mkdir -p "$out_dir"
rm -f "$out_dir"/dotlink-* "$out_dir"/VERSION "$out_dir"/PLATFORMS.txt

nix_cmd=(nix --extra-experimental-features "nix-command flakes")

echo "Building all release binaries from the Nix release graph..."
release_out="$("${nix_cmd[@]}" build .#release-all --no-link --print-out-paths)"

cp "$release_out"/dotlink-* "$out_dir/"
cp "$release_out"/VERSION "$release_out"/PLATFORMS.txt "$out_dir/"

printf '\nRelease artifacts:\n'
ls -lh "$out_dir"
