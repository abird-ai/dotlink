#!/bin/sh
set -eu

say() {
  printf '%s\n' "$*"
}

die() {
  printf 'dotlink installer: %s\n' "$*" >&2
  exit 1
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "sha256sum or shasum is required for checksum verification"
  fi
}

dotlink_version() {
  output="$("$1" --version 2>/dev/null || true)"
  case "$output" in
    "dotlink "*)
      printf '%s' "${output#dotlink }"
      ;;
  esac
}

VERSION="${DOTLINK_VERSION:-latest}"
REPO="${DOTLINK_REPO:-abird-ai/dotlink}"
BASE_URL="${DOTLINK_RELEASE_BASE_URL:-}"

if [ -n "${DOTLINK_INSTALL_DIR:-}" ]; then
  INSTALL_DIR="$DOTLINK_INSTALL_DIR"
elif [ -n "${HOME:-}" ]; then
  INSTALL_DIR="$HOME/.local/bin"
else
  die "HOME is not set; set DOTLINK_INSTALL_DIR explicitly"
fi

case "$(uname -s 2>/dev/null || printf unknown)" in
  Linux)
    case "$(uname -m 2>/dev/null || printf unknown)" in
      x86_64|amd64)
        ASSET="dotlink-linux-x86_64"
        BINARY="dotlink"
        ;;
      aarch64|arm64)
        ASSET="dotlink-linux-aarch64"
        BINARY="dotlink"
        ;;
      *)
        die "unsupported Linux architecture: $(uname -m). Published: x86_64, aarch64."
        ;;
    esac
    ;;
  MINGW*|MSYS*|CYGWIN*)
    case "$(uname -m 2>/dev/null || printf unknown)" in
      x86_64|amd64)
        ASSET="dotlink-windows-x86_64.exe"
        BINARY="dotlink.exe"
        ;;
      aarch64|arm64)
        ASSET="dotlink-windows-aarch64.exe"
        BINARY="dotlink.exe"
        ;;
      *)
        die "unsupported Windows architecture: $(uname -m). Published: x86_64, arm64."
        ;;
    esac
    ;;
  Darwin)
    case "$(uname -m 2>/dev/null || printf unknown)" in
      arm64|aarch64)
        ASSET="dotlink-macos-aarch64"
        BINARY="dotlink"
        ;;
      *)
        die "unsupported macOS architecture: $(uname -m). Published: Apple Silicon (arm64)."
        ;;
    esac
    ;;
  *)
    die "unsupported OS: $(uname -s 2>/dev/null || printf unknown)"
    ;;
esac

if [ -z "$BASE_URL" ]; then
  if [ "$VERSION" = "latest" ]; then
    BASE_URL="https://github.com/${REPO}/releases/latest/download"
  else
    case "$VERSION" in
      v*) TAG="$VERSION" ;;
      *) TAG="v$VERSION" ;;
    esac
    BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
  fi
fi

command -v curl >/dev/null 2>&1 || die "curl is required"

TMPDIR_ROOT="${TMPDIR:-/tmp}"
WORKDIR="$(mktemp -d "${TMPDIR_ROOT%/}/dotlink-install.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT HUP INT TERM

say "Downloading dotlink..."
curl -fsSL "${BASE_URL%/}/${ASSET}" -o "$WORKDIR/$ASSET"
curl -fsSL "${BASE_URL%/}/${ASSET}.sha256" -o "$WORKDIR/$ASSET.sha256"

expected="$(awk 'NR == 1 { print $1; exit }' "$WORKDIR/$ASSET.sha256" | tr 'A-F' 'a-f')"
[ "${#expected}" -eq 64 ] || die "invalid SHA-256 sidecar"
case "$expected" in
  *[!0-9a-f]*) die "invalid SHA-256 sidecar" ;;
esac

actual="$(sha256_file "$WORKDIR/$ASSET" | tr 'A-F' 'a-f')"
[ "$expected" = "$actual" ] || die "SHA-256 verification failed"

chmod 0755 "$WORKDIR/$ASSET"
new_version="$(dotlink_version "$WORKDIR/$ASSET")"
[ -n "$new_version" ] || die "downloaded asset did not report a valid dotlink version"
if [ "$VERSION" != "latest" ]; then
  case "$VERSION" in
    v*) requested_version="${VERSION#v}" ;;
    *) requested_version="$VERSION" ;;
  esac
  [ "$new_version" = "$requested_version" ] ||
    die "downloaded dotlink version $new_version does not match requested version $requested_version"
fi

mkdir -p "$INSTALL_DIR"
DESTINATION="$INSTALL_DIR/$BINARY"
had_existing=false
old_version=""
installed_hash=""

if [ -f "$DESTINATION" ]; then
  had_existing=true
  old_version="$(dotlink_version "$DESTINATION")"
  installed_hash="$(sha256_file "$DESTINATION" | tr 'A-F' 'a-f')"
fi

if [ "$installed_hash" = "$expected" ]; then
  if [ -n "$new_version" ]; then
    say "dotlink $new_version is already up to date at $DESTINATION"
  else
    say "dotlink is already up to date at $DESTINATION"
  fi
else
  DEST_TMP="$INSTALL_DIR/.$BINARY.tmp.$$"
  trap 'rm -rf "$WORKDIR"; [ -z "${DEST_TMP:-}" ] || rm -f "$DEST_TMP"' EXIT HUP INT TERM
  cp "$WORKDIR/$ASSET" "$DEST_TMP"
  chmod 0755 "$DEST_TMP"
  mv -f "$DEST_TMP" "$DESTINATION"
  DEST_TMP=""

  if [ "$had_existing" = true ]; then
    if [ -n "$old_version" ] && [ -n "$new_version" ]; then
      say "Updated dotlink $old_version -> $new_version at $DESTINATION"
    elif [ -n "$new_version" ]; then
      say "Updated dotlink to $new_version at $DESTINATION"
    else
      say "Updated dotlink at $DESTINATION"
    fi
  elif [ -n "$new_version" ]; then
    say "Installed dotlink $new_version to $DESTINATION"
  else
    say "Installed dotlink to $DESTINATION"
  fi
fi

case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    say "Add $INSTALL_DIR to PATH to run 'dotlink' directly."
    ;;
esac
