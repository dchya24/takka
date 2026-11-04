#!/usr/bin/env bash
set -u

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
INSTALLER="$ROOT_DIR/scripts/install.sh"
TEST_ROOT=$(mktemp -d)
PASS=0
FAIL=0

cleanup() {
  rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

record_pass() {
  PASS=$((PASS + 1))
  printf 'ok - %s\n' "$1"
}

record_fail() {
  FAIL=$((FAIL + 1))
  printf 'not ok - %s\n' "$1" >&2
  [ -n "${2:-}" ] && printf '  %s\n' "$2" >&2
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

make_release_fixture() {
  fixture_dir=$1
  marker=$2
  mkdir -p "$fixture_dir/build"
  cat > "$fixture_dir/build/takka" <<EOF
#!/bin/sh
printf '%s\\n' '$marker'
if [ "\${1:-}" = config ]; then
  printf '%s\\n' "\$*" > "\${TAKKA_INIT_LOG:?}"
fi
EOF
  chmod +x "$fixture_dir/build/takka"

  # The installer also fetches the takka-runtime daemon (a bare binary
  # asset per platform) and verifies it against the same checksum file —
  # ship fixture runtime binaries and their hashes alongside the CLI.
  cat > "$fixture_dir/build/takka-runtime" <<EOF
#!/bin/sh
printf '%s\n' '$marker-runtime'
EOF
  chmod +x "$fixture_dir/build/takka-runtime"

  for asset in takka-linux-x86_64.tar.gz takka-macos-x86_64.tar.gz takka-macos-aarch64.tar.gz; do
    tar -czf "$fixture_dir/$asset" -C "$fixture_dir/build" takka
  done
  for runtime in takka-runtime-linux-x86_64 takka-runtime-macos-x86_64 takka-runtime-macos-aarch64; do
    cp "$fixture_dir/build/takka-runtime" "$fixture_dir/$runtime"
  done

  : > "$fixture_dir/checksums-linux.txt"
  : > "$fixture_dir/checksums-macos.txt"
  for asset in takka-linux-x86_64.tar.gz takka-runtime-linux-x86_64; do
    printf '%s  %s\n' "$(sha256_file "$fixture_dir/$asset")" "$asset" >> "$fixture_dir/checksums-linux.txt"
  done
  for asset in takka-macos-x86_64.tar.gz takka-macos-aarch64.tar.gz takka-runtime-macos-x86_64 takka-runtime-macos-aarch64; do
    printf '%s  %s\n' "$(sha256_file "$fixture_dir/$asset")" "$asset" >> "$fixture_dir/checksums-macos.txt"
  done
}

make_stubs() {
  stub_dir=$1
  mkdir -p "$stub_dir"
  cat > "$stub_dir/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -s) printf '%s\n' "${TEST_UNAME_S:-Linux}" ;;
  -m) printf '%s\n' "${TEST_UNAME_M:-x86_64}" ;;
  *) printf '%s\n' "${TEST_UNAME_S:-Linux}" ;;
esac
EOF
  cat > "$stub_dir/curl" <<'EOF'
#!/bin/sh
output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o|--output) output=$2; shift 2 ;;
    -*) shift ;;
    *) url=$1; shift ;;
  esac
done
[ -n "$output" ] || exit 64
[ -n "$url" ] || exit 64
printf '%s\n' "$url" >> "${TEST_DOWNLOAD_LOG:?}"
asset=${url##*/}
cp "${TEST_FIXTURE_DIR:?}/$asset" "$output"
EOF
  chmod +x "$stub_dir/uname" "$stub_dir/curl"
}

run_installer() {
  case_dir=$1
  shift
  mkdir -p "$case_dir/home" "$case_dir/bin" "$case_dir/stubs"
  make_stubs "$case_dir/stubs"
  : > "$case_dir/download.log"
  : > "$case_dir/init.log"

  env_args=()
  installer_args=()
  parsing_installer_args=false
  for arg in "$@"; do
    if [ "$arg" = -- ]; then
      parsing_installer_args=true
    elif [ "$parsing_installer_args" = true ]; then
      installer_args+=("$arg")
    else
      env_args+=("$arg")
    fi
  done

  HOME="$case_dir/home" \
  PATH="$case_dir/stubs:/usr/bin:/bin" \
  TAKKA_INSTALL_DIR="$case_dir/bin" \
  TAKKA_RELEASE_BASE_URL="https://fixtures.invalid/releases/latest/download" \
  TAKKA_INIT_LOG="$case_dir/init.log" \
  TEST_FIXTURE_DIR="$FIXTURE_DIR" \
  TEST_DOWNLOAD_LOG="$case_dir/download.log" \
  env "${env_args[@]}" sh "$INSTALLER" "${installer_args[@]}"
}

FIXTURE_DIR="$TEST_ROOT/release"
mkdir -p "$FIXTURE_DIR"
make_release_fixture "$FIXTURE_DIR" fixture-v1

case_unsupported_linux_arm64() {
  dir="$TEST_ROOT/unsupported-linux-arm64"
  output=$(run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=aarch64 2>&1)
  status=$?
  if [ "$status" -ne 0 ] && printf '%s' "$output" | grep -qi 'Linux aarch64.*not supported'; then
    record_pass "unsupported Linux aarch64 exits with guidance"
  else
    record_fail "unsupported Linux aarch64 exits with guidance" "status=$status output=$output"
  fi
}

case_linux_install() {
  dir="$TEST_ROOT/linux-install"
  if run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 >/dev/null 2>&1 \
    && [ -x "$dir/bin/takka" ] \
    && [ -x "$dir/bin/takka-runtime" ] \
    && [ "$(TAKKA_INIT_LOG="$dir/init.log" "$dir/bin/takka")" = fixture-v1 ]; then
    record_pass "Linux x86_64 installs verified binaries"
  else
    record_fail "Linux x86_64 installs verified binaries"
  fi
}

case_macos_x86_64_install() {
  dir="$TEST_ROOT/macos-x86_64"
  if run_installer "$dir" TEST_UNAME_S=Darwin TEST_UNAME_M=x86_64 >/dev/null 2>&1 \
    && grep -q 'takka-macos-x86_64.tar.gz' "$dir/download.log" \
    && grep -q 'checksums-macos.txt' "$dir/download.log" \
    && [ -x "$dir/bin/takka" ]; then
    record_pass "macOS x86_64 selects matching release assets"
  else
    record_fail "macOS x86_64 selects matching release assets"
  fi
}

case_macos_aarch64_install() {
  dir="$TEST_ROOT/macos-aarch64"
  if run_installer "$dir" TEST_UNAME_S=Darwin TEST_UNAME_M=arm64 >/dev/null 2>&1 \
    && grep -q 'takka-macos-aarch64.tar.gz' "$dir/download.log" \
    && grep -q 'checksums-macos.txt' "$dir/download.log" \
    && [ -x "$dir/bin/takka" ]; then
    record_pass "macOS aarch64 selects matching release assets"
  else
    record_fail "macOS aarch64 selects matching release assets"
  fi
}

case_checksum_mismatch() {
  dir="$TEST_ROOT/checksum-mismatch"
  mkdir -p "$dir/bin"
  printf '%s\n' old-binary > "$dir/bin/takka"
  chmod +x "$dir/bin/takka"
  cp "$FIXTURE_DIR/checksums-linux.txt" "$FIXTURE_DIR/checksums-linux.txt.good"
  printf '%064d  %s\n' 0 takka-linux-x86_64.tar.gz > "$FIXTURE_DIR/checksums-linux.txt"
  output=$(run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 2>&1)
  status=$?
  mv "$FIXTURE_DIR/checksums-linux.txt.good" "$FIXTURE_DIR/checksums-linux.txt"
  if [ "$status" -ne 0 ] \
    && printf '%s' "$output" | grep -qi 'checksum mismatch' \
    && grep -q old-binary "$dir/bin/takka"; then
    record_pass "checksum mismatch preserves existing binary"
  else
    record_fail "checksum mismatch preserves existing binary" "status=$status"
  fi
}

case_explicit_version() {
  dir="$TEST_ROOT/explicit-version"
  if run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 TAKKA_VERSION=0.3.2 >/dev/null 2>&1 \
    && grep -q '/releases/download/v0.3.2/' "$dir/download.log"; then
    record_pass "explicit version selects tagged release URL"
  else
    record_fail "explicit version selects tagged release URL"
  fi
}

case_invalid_version() {
  dir="$TEST_ROOT/invalid-version"
  output=$(run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 TAKKA_VERSION=0.3.2-beta 2>&1)
  status=$?
  if [ "$status" -ne 0 ] \
    && printf '%s' "$output" | grep -q 'Invalid TAKKA_VERSION' \
    && [ ! -e "$dir/bin/takka" ]; then
    record_pass "invalid version is rejected before installation"
  else
    record_fail "invalid version is rejected before installation" "status=$status output=$output"
  fi
}

case_legacy_env_fallback() {
  dir="$TEST_ROOT/legacy-env-fallback"
  output=$(run_installer_env_legacy "$dir" 2>&1)
  status=$?
  if [ "$status" -eq 0 ] \
    && [ -x "$dir/bin/takka" ] \
    && printf '%s' "$output" | grep -q 'using legacy AGENTIC_INSTALL_DIR' \
    && printf '%s' "$output" | grep -q 'using legacy AGENTIC_RELEASE_BASE_URL'; then
    record_pass "legacy AGENTIC_* installer vars still work with warning"
  else
    record_fail "legacy AGENTIC_* installer vars still work with warning" "status=$status output=$output"
  fi
}

# Same harness as run_installer but passes pre-rebrand AGENTIC_* vars.
run_installer_env_legacy() {
  case_dir=$1
  mkdir -p "$case_dir/home" "$case_dir/bin" "$case_dir/stubs"
  make_stubs "$case_dir/stubs"
  : > "$case_dir/download.log"
  : > "$case_dir/init.log"

  HOME="$case_dir/home" \
  PATH="$case_dir/stubs:/usr/bin:/bin" \
  AGENTIC_INSTALL_DIR="$case_dir/bin" \
  AGENTIC_RELEASE_BASE_URL="https://fixtures.invalid/releases/latest/download" \
  TAKKA_INIT_LOG="$case_dir/init.log" \
  TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 \
  TEST_FIXTURE_DIR="$FIXTURE_DIR" \
  TEST_DOWNLOAD_LOG="$case_dir/download.log" \
  sh "$INSTALLER"
}

case_config_preserved() {
  dir="$TEST_ROOT/config-preserved"
  mkdir -p "$dir/home/.config/takka"
  printf '%s\n' '{"keep":true}' > "$dir/home/.config/takka/config.json"
  if run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 >/dev/null 2>&1 \
    && grep -q '"keep":true' "$dir/home/.config/takka/config.json"; then
    record_pass "existing config is preserved"
  else
    record_fail "existing config is preserved"
  fi
}

case_legacy_config_hint() {
  dir="$TEST_ROOT/legacy-config-hint"
  mkdir -p "$dir/home/.config/agentic"
  printf '%s\n' '{"keep":true}' > "$dir/home/.config/agentic/config.json"
  output=$(run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 2>&1)
  status=$?
  if [ "$status" -eq 0 ] \
    && grep -q '"keep":true' "$dir/home/.config/agentic/config.json" \
    && printf '%s' "$output" | grep -q 'takka config migrate'; then
    record_pass "pre-rebrand config is left in place with a migrate hint"
  else
    record_fail "pre-rebrand config is left in place with a migrate hint" "status=$status"
  fi
}

case_default_skips_init() {
  dir="$TEST_ROOT/default-skips-init"
  if run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 >/dev/null 2>&1 \
    && [ ! -s "$dir/init.log" ]; then
    record_pass "default install does not invoke config init"
  else
    record_fail "default install does not invoke config init"
  fi
}

case_init_opt_in() {
  dir="$TEST_ROOT/init-opt-in"
  if run_installer "$dir" TEST_UNAME_S=Linux TEST_UNAME_M=x86_64 -- --init >/dev/null 2>&1 \
    && grep -q '^config init --interactive$' "$dir/init.log"; then
    record_pass "--init invokes config wizard after install"
  else
    record_fail "--init invokes config wizard after install"
  fi
}

case_unsupported_linux_arm64
case_linux_install
case_macos_x86_64_install
case_macos_aarch64_install
case_checksum_mismatch
case_explicit_version
case_invalid_version
case_legacy_env_fallback
case_config_preserved
case_legacy_config_hint
case_default_skips_init
case_init_opt_in

printf '\n%d passed; %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
