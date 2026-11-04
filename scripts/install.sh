#!/bin/sh
set -eu

REPOSITORY="dchya24/takka"
DEFAULT_RELEASE_BASE_URL="https://github.com/$REPOSITORY/releases/latest/download"
INSTALL_DIR=${TAKKA_INSTALL_DIR:-${AGENTIC_INSTALL_DIR:-"${HOME:?HOME is not set}/.local/bin"}}
RUN_INIT=false

# Pre-rebrand compat: honor legacy AGENTIC_* installer vars for one
# release cycle, with a deprecation warning (never silently).
if [ -z "${TAKKA_INSTALL_DIR:-}" ] && [ -n "${AGENTIC_INSTALL_DIR:-}" ]; then
  printf 'takka: warning: using legacy AGENTIC_INSTALL_DIR; set TAKKA_INSTALL_DIR instead\n' >&2
fi

fail() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Install takka into a user-local directory.

Usage: install.sh [--init] [--help]

Options:
  --init  Run `takka config init --interactive` after installation.
  --help  Show this help.

Environment:
  TAKKA_VERSION           Release version, for example v0.3.2 or 0.3.2.
  TAKKA_INSTALL_DIR       Destination directory (default: $HOME/.local/bin).
  TAKKA_RELEASE_BASE_URL  Override the GitHub Release download base URL.

  Pre-rebrand legacy names (AGENTIC_VERSION, AGENTIC_INSTALL_DIR,
  AGENTIC_RELEASE_BASE_URL) are still honored with a deprecation warning.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --init) RUN_INIT=true ;;
    --help|-h) usage; exit 0 ;;
    *) fail "Unknown option: $1" ;;
  esac
  shift
done

case "$(uname -s)" in
  Linux) OS=linux ;;
  Darwin) OS=macos ;;
  *) fail "Unsupported operating system: $(uname -s)" ;;
esac

case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  arm64|aarch64)
    if [ "$OS" = macos ]; then
      ARCH=aarch64
    else
      fail "Linux aarch64 is not supported yet. Prebuilt releases currently support Linux x86_64 only."
    fi
    ;;
  *) fail "Unsupported architecture: $(uname -m)" ;;
esac

ASSET="takka-$OS-$ARCH.tar.gz"
RUNTIME_ASSET="takka-runtime-$OS-$ARCH"
CHECKSUMS="checksums-$OS.txt"
RELEASE_BASE_URL=${TAKKA_RELEASE_BASE_URL:-${AGENTIC_RELEASE_BASE_URL:-$DEFAULT_RELEASE_BASE_URL}}
if [ -z "${TAKKA_RELEASE_BASE_URL:-}" ] && [ -n "${AGENTIC_RELEASE_BASE_URL:-}" ]; then
  printf 'takka: warning: using legacy AGENTIC_RELEASE_BASE_URL; set TAKKA_RELEASE_BASE_URL instead\n' >&2
fi

if [ -n "${TAKKA_VERSION:-}" ]; then
  VERSION=$TAKKA_VERSION
else
  VERSION=${AGENTIC_VERSION:-}
  if [ -n "$VERSION" ]; then
    printf 'takka: warning: using legacy AGENTIC_VERSION; set TAKKA_VERSION instead\n' >&2
  fi
fi
if [ -n "$VERSION" ]; then
  case "$VERSION" in
    v*) ;;
    *) VERSION="v$VERSION" ;;
  esac
  if ! printf '%s\n' "$VERSION" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
    fail "Invalid TAKKA_VERSION '$VERSION'; expected vX.Y.Z or X.Y.Z"
  fi
  # Preserve custom hosts used by mirrors/tests while replacing the standard
  # GitHub latest-release suffix with the requested tag.
  case "$RELEASE_BASE_URL" in
    */releases/latest/download)
      RELEASE_BASE_URL=${RELEASE_BASE_URL%/releases/latest/download}/releases/download/$VERSION
      ;;
    *) RELEASE_BASE_URL=${RELEASE_BASE_URL%/}/$VERSION ;;
  esac
fi
RELEASE_BASE_URL=${RELEASE_BASE_URL%/}

command -v curl >/dev/null 2>&1 || fail "curl is required to download takka"
command -v tar >/dev/null 2>&1 || fail "tar is required to extract Agentic"

if command -v sha256sum >/dev/null 2>&1; then
  sha256_file() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_file() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  fail "sha256sum or shasum is required to verify the download"
fi

umask 077
TMP_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t takka-install)
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT HUP INT TERM

ARCHIVE_PATH="$TMP_DIR/$ASSET"
CHECKSUM_PATH="$TMP_DIR/$CHECKSUMS"
STAGE_DIR="$TMP_DIR/stage"
mkdir -p "$STAGE_DIR"

RUNTIME_PATH="$TMP_DIR/$RUNTIME_ASSET"

printf 'Downloading %s...\n' "$ASSET"
curl --fail --location --silent --show-error --output "$ARCHIVE_PATH" "$RELEASE_BASE_URL/$ASSET"
curl --fail --location --silent --show-error --output "$CHECKSUM_PATH" "$RELEASE_BASE_URL/$CHECKSUMS"

EXPECTED_HASH=$(awk -v asset="$ASSET" '$2 == asset || $2 == "*" asset { print $1; exit }' "$CHECKSUM_PATH")
[ -n "$EXPECTED_HASH" ] || fail "Checksum entry for $ASSET was not found in $CHECKSUMS"
ACTUAL_HASH=$(sha256_file "$ARCHIVE_PATH")
[ "$ACTUAL_HASH" = "$EXPECTED_HASH" ] || fail "Checksum mismatch for $ASSET"
printf 'Checksum verified.\n'

# The headless runtime daemon is a hard dependency of the CLI (it spawns
# it for every run). Download it separately (raw asset, not in the archive).
printf 'Downloading %s...\n' "$RUNTIME_ASSET"
curl --fail --location --silent --show-error --output "$RUNTIME_PATH" "$RELEASE_BASE_URL/$RUNTIME_ASSET"
EXPECTED_RUNTIME_HASH=$(awk -v asset="$RUNTIME_ASSET" '$2 == asset || $2 == "*" asset { print $1; exit }' "$CHECKSUM_PATH")
[ -n "$EXPECTED_RUNTIME_HASH" ] || fail "Checksum entry for $RUNTIME_ASSET was not found in $CHECKSUMS"
ACTUAL_RUNTIME_HASH=$(sha256_file "$RUNTIME_PATH")
[ "$ACTUAL_RUNTIME_HASH" = "$EXPECTED_RUNTIME_HASH" ] || fail "Checksum mismatch for $RUNTIME_ASSET"
printf 'Checksum verified.\n'

tar -xzf "$ARCHIVE_PATH" -C "$STAGE_DIR"
[ -f "$STAGE_DIR/takka" ] || fail "Downloaded archive does not contain the takka binary"
mkdir -p "$INSTALL_DIR"
install_binary() {
  src=$1
  dest_name=$2
  dest_tmp="$INSTALL_DIR/.${dest_name}.install.$$"
  cp "$src" "$dest_tmp"
  chmod 755 "$dest_tmp"
  # Stage inside the destination filesystem, then rename atomically over
  # the old executable.
  mv -f "$dest_tmp" "$INSTALL_DIR/$dest_name"
}
install_binary "$STAGE_DIR/takka" "takka"
install_binary "$RUNTIME_PATH" "takka-runtime"
printf 'takka installed successfully: %s\n' "$INSTALL_DIR/takka"
printf 'Runtime daemon installed:     %s\n' "$INSTALL_DIR/takka-runtime"

CONFIG_PATH="${HOME:?HOME is not set}/.config/takka/config.json"
LEGACY_CONFIG_PATH="${HOME:?HOME is not set}/.config/agentic/config.json"
if [ -f "$CONFIG_PATH" ]; then
  printf 'Existing config preserved: %s\n' "$CONFIG_PATH"
elif [ -f "$LEGACY_CONFIG_PATH" ]; then
  printf 'Pre-rebrand config found at %s; it is still read.\n' "$LEGACY_CONFIG_PATH"
  printf 'Run `takka config migrate` to move it to %s.\n' "$(dirname "$CONFIG_PATH")"
else
  printf 'No config was created. Initialize it when ready.\n'
fi

case ":${PATH:-}:" in
  *:"$INSTALL_DIR":*) ;;
  *)
    printf '\nAdd takka to PATH for future shells:\n'
    printf '  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    ;;
esac

if [ "$RUN_INIT" = true ]; then
  "$INSTALL_DIR/takka" config init --interactive
else
  printf '\nNext: %s/takka config init --interactive\n' "$INSTALL_DIR"
fi
