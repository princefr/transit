#!/usr/bin/env bash
# Serve the WASM map UI with Trunk.
# Trunk 0.21 treats NO_COLOR=1 as invalid (expects true/false); strip it here.
set -euo pipefail
cd "$(dirname "$0")"

export PATH="${HOME}/.cargo/bin:${PATH}"

if ! command -v trunk >/dev/null 2>&1; then
  echo "trunk not found. Installing prebuilt binary..."
  ARCH=$(uname -m)
  case "$ARCH" in
    x86_64) ASSET=trunk-x86_64-unknown-linux-gnu.tar.gz ;;
    aarch64) ASSET=trunk-aarch64-unknown-linux-gnu.tar.gz ;;
    *)
      echo "Unsupported arch $ARCH — run: cargo install trunk"
      exit 1
      ;;
  esac
  VER=v0.21.14
  TMP=$(mktemp -d)
  curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/${VER}/${ASSET}" -o "$TMP/trunk.tgz"
  tar -xzf "$TMP/trunk.tgz" -C "$TMP"
  mkdir -p "${HOME}/.cargo/bin"
  install -m 755 "$TMP/trunk" "${HOME}/.cargo/bin/trunk"
  rm -rf "$TMP"
  echo "Installed trunk to ${HOME}/.cargo/bin/trunk"
fi

rustup target add wasm32-unknown-unknown >/dev/null

# Unset env vars that break trunk's clap --no-color parser
unset NO_COLOR CLICOLOR CLICOLOR_FORCE FORCE_COLOR 2>/dev/null || true
export TRUNK_COLOR="${TRUNK_COLOR:-never}"
export TRUNK_SKIP_VERSION_CHECK="${TRUNK_SKIP_VERSION_CHECK:-true}"

# Stop other trunk serve on this project (they race on dist/.stage)
WEB_DIR="$(pwd)"
for pid in $(pgrep -x trunk 2>/dev/null || true); do
  # Only kill trunk whose cwd is this web dir (best-effort)
  if [ -r "/proc/$pid/cwd" ] && [ "$(readlink "/proc/$pid/cwd" 2>/dev/null || true)" = "$WEB_DIR" ]; then
    echo "Stopping existing trunk (pid $pid)…"
    kill "$pid" 2>/dev/null || true
  fi
done
sleep 0.3

# Clean partial stage from a previous failed build
rm -rf dist/.stage
mkdir -p dist

echo "Building & serving → http://127.0.0.1:8088"
echo "GraphQL API should be running at http://127.0.0.1:8080 (cargo run in parent dir)"
# One clean build first so serve doesn't start from a broken dist
trunk build "$@" || true
exec trunk serve "$@"
