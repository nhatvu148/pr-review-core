#!/usr/bin/env sh
# Install the `kaniscope` binary from a GitHub Release.
#
#   curl -fsSL https://raw.githubusercontent.com/nhatvu148/pr-review-core/main/packaging/install.sh | sh
#   ... | sh -s -- --version v0.26.0 --dir /usr/local/bin
#
# For everyone npm and PyPI do not reach: a Go or Ruby bot, a CI job, a plain
# shell. It downloads one archive and verifies it against the Release's
# SHA256SUMS before anything is unpacked.
#
# POSIX sh, not bash: this is piped into whatever /bin/sh a machine has, and on
# Debian/Ubuntu and Alpine that is dash, where `[[`, arrays and `local -n` are
# syntax errors. Tested with `sh -n` in CI.
set -eu

REPO="nhatvu148/pr-review-core"
VERSION="${KANISCOPE_VERSION:-latest}"
DIR="${KANISCOPE_INSTALL_DIR:-$HOME/.local/bin}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --dir)     DIR="$2"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

say() { echo "kaniscope: $*" >&2; }
die() { say "$*"; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "need $1 on PATH"; }

need uname
need tar
# curl or wget, whichever exists — a minimal container often has only one.
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  die "need curl or wget on PATH"
fi

# The Rust target triple for this host. Deliberately explicit rather than clever:
# an unknown platform must say so and name what exists, not download an archive
# that cannot run and fail later with "exec format error".
os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Darwin/arm64)          TARGET="aarch64-apple-darwin" ;;
  Darwin/x86_64)         TARGET="x86_64-apple-darwin" ;;
  Linux/aarch64|Linux/arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  Linux/x86_64)          TARGET="x86_64-unknown-linux-gnu" ;;
  *) die "no prebuilt binary for $os/$arch. Build it: cargo install pr-review-core --features cli" ;;
esac

# `KANISCOPE_BASE_URL` points the download at an internal mirror or a directory
# served on a local network, for a machine that cannot reach github.com — and is
# what lets this script be tested end to end without cutting a real release.
if [ -n "${KANISCOPE_BASE_URL:-}" ]; then
  BASE="$KANISCOPE_BASE_URL"
elif [ "$VERSION" = "latest" ]; then
  BASE="https://github.com/$REPO/releases/latest/download"
else
  BASE="https://github.com/$REPO/releases/download/$VERSION"
fi
ARCHIVE="kaniscope-$TARGET.tar.gz"

TMP="$(mktemp -d)"
# Clean up on every exit path, including the die()s above this point in flow —
# an installer that litters /tmp on failure gets run again, and again.
trap 'rm -rf "$TMP"' EXIT INT TERM

say "downloading $ARCHIVE ($VERSION)"
fetch "$BASE/$ARCHIVE" "$TMP/$ARCHIVE" || die "download failed — does release $VERSION exist?"

# Verify BEFORE unpacking. The checksum file is published beside the archives by
# the Distribute workflow; a release without one is a release this script will
# not silently trust, because "the download was truncated" and "the download was
# replaced" look identical to tar.
if fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null; then
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$TMP" && grep " $ARCHIVE\$" SHA256SUMS | sha256sum -c -) >/dev/null \
      || die "checksum MISMATCH for $ARCHIVE — do not use this download"
  elif command -v shasum >/dev/null 2>&1; then
    # macOS has shasum, not sha256sum.
    (cd "$TMP" && grep " $ARCHIVE\$" SHA256SUMS | shasum -a 256 -c -) >/dev/null \
      || die "checksum MISMATCH for $ARCHIVE — do not use this download"
  else
    say "WARNING: no sha256sum/shasum — installing UNVERIFIED"
  fi
  say "checksum ok"
else
  say "WARNING: no SHA256SUMS in release $VERSION — installing UNVERIFIED"
fi

tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
[ -f "$TMP/kaniscope" ] || die "archive did not contain a kaniscope binary"

mkdir -p "$DIR"
chmod +x "$TMP/kaniscope"
# Move into place rather than copying onto the target: replacing a RUNNING
# binary in place is what produces "text file busy", and mv over an existing
# path is atomic within a filesystem.
mv -f "$TMP/kaniscope" "$DIR/kaniscope"

say "installed $("$DIR/kaniscope" --version) to $DIR/kaniscope"
case ":$PATH:" in
  *":$DIR:"*) ;;
  *) say "NOTE: $DIR is not on your PATH — add it, or run $DIR/kaniscope directly" ;;
esac
