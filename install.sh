#!/bin/sh
set -eu

say() {
  printf '%s\n' "$*"
}

die() {
  printf 'abird-link installer: %s\n' "$*" >&2
  exit 1
}

VERSION="${ABIRD_LINK_VERSION:-latest}"
REPO="${ABIRD_LINK_REPO:-}"
BASE_URL="${ABIRD_LINK_RELEASE_BASE_URL:-}"
INSTALL_DIR="${ABIRD_LINK_INSTALL_DIR:-${HOME:-}/.local/bin}"

case "$(uname -s 2>/dev/null || printf unknown)" in
  Linux)
    case "$(uname -m 2>/dev/null || printf unknown)" in
      x86_64|amd64)
        ASSET="abird-link-linux-x86_64"
        BINARY="abird-link"
        ;;
      *)
        die "unsupported Linux architecture: $(uname -m). Currently published: x86_64."
        ;;
    esac
    ;;
  MINGW*|MSYS*|CYGWIN*)
    case "$(uname -m 2>/dev/null || printf unknown)" in
      x86_64|amd64)
        ASSET="abird-link-windows-x86_64.exe"
        BINARY="abird-link.exe"
        ;;
      *)
        die "unsupported Windows architecture: $(uname -m). Currently published: x86_64."
        ;;
    esac
    ;;
  Darwin)
    die "prebuilt macOS releases are not configured yet; build from source with Cargo. Apple Silicon can also use 'nix build'."
    ;;
  *)
    die "unsupported OS: $(uname -s 2>/dev/null || printf unknown)"
    ;;
esac

if [ -z "$BASE_URL" ]; then
  [ -n "$REPO" ] || die "set ABIRD_LINK_REPO=owner/repo or ABIRD_LINK_RELEASE_BASE_URL=https://... before running this installer"

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
WORKDIR="$(mktemp -d "${TMPDIR_ROOT%/}/abird-link-install.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT HUP INT TERM

say "Downloading ${ASSET}..."
curl -fsSL "${BASE_URL%/}/${ASSET}" -o "$WORKDIR/$ASSET"
curl -fsSL "${BASE_URL%/}/${ASSET}.sha256" -o "$WORKDIR/$ASSET.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  (
    cd "$WORKDIR"
    sha256sum -c "$ASSET.sha256"
  ) >/dev/null
elif command -v shasum >/dev/null 2>&1; then
  expected="$(awk '{print $1}' "$WORKDIR/$ASSET.sha256")"
  actual="$(shasum -a 256 "$WORKDIR/$ASSET" | awk '{print $1}')"
  [ "$expected" = "$actual" ] || die "SHA-256 verification failed"
else
  die "sha256sum or shasum is required for checksum verification"
fi

mkdir -p "$INSTALL_DIR"
cp "$WORKDIR/$ASSET" "$INSTALL_DIR/$BINARY"
chmod 0755 "$INSTALL_DIR/$BINARY"

say "Installed abird-link to $INSTALL_DIR/$BINARY"

case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    say "Add $INSTALL_DIR to PATH to run 'abird-link' directly."
    ;;
esac
