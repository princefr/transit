#!/usr/bin/env bash
# Serve the WASM map UI with Trunk.
# Trunk 0.21 treats NO_COLOR=1 as invalid (expects true/false); strip it here.
set -euo pipefail
cd "$(dirname "$0")"

export PATH="${HOME}/.cargo/bin:${PATH}"

# Prebuilt trunk download is verified against a SHA-256 before install:
#   1. TRUNK_SHA256=<hex>  — enforced when set (pin it in CI / your shell profile);
#   2. otherwise the release's published "<asset>.sha256" file is fetched and used;
#   3. if neither is available the install is refused (use `cargo install trunk --locked`).
# TRUNK_VERSION overrides the pinned release (default below).
if ! command -v trunk >/dev/null 2>&1; then
  echo "trunk not found. Installing prebuilt binary..."
  ARCH=$(uname -m)
  case "$ARCH" in
    x86_64) ASSET=trunk-x86_64-unknown-linux-gnu.tar.gz ;;
    aarch64) ASSET=trunk-aarch64-unknown-linux-gnu.tar.gz ;;
    *)
      echo "Unsupported arch $ARCH — run: cargo install trunk --locked" >&2
      exit 1
      ;;
  esac
  VER="${TRUNK_VERSION:-v0.21.14}"
  URL="https://github.com/trunk-rs/trunk/releases/download/${VER}/${ASSET}"
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  curl -fsSL --proto '=https' --tlsv1.2 "$URL" -o "$TMP/trunk.tgz"

  EXPECTED="${TRUNK_SHA256:-}"
  if [ -z "$EXPECTED" ]; then
    if curl -fsSL --proto '=https' --tlsv1.2 "${URL}.sha256" -o "$TMP/trunk.tgz.sha256"; then
      EXPECTED=$(awk '{print $1; exit}' "$TMP/trunk.tgz.sha256")
    fi
  fi
  if ! printf '%s' "$EXPECTED" | grep -Eq '^[0-9a-fA-F]{64}$'; then
    echo "serve.sh: no SHA-256 available for ${ASSET} (${VER})." >&2
    echo "  Set TRUNK_SHA256=<sha256 of the release asset> or run: cargo install trunk --locked" >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL=$(sha256sum "$TMP/trunk.tgz" | awk '{print $1}')
  else
    ACTUAL=$(shasum -a 256 "$TMP/trunk.tgz" | awk '{print $1}')
  fi
  EXPECTED=$(printf '%s' "$EXPECTED" | tr 'A-F' 'a-f')
  if [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "serve.sh: SHA-256 mismatch for ${ASSET}: expected ${EXPECTED}, got ${ACTUAL}" >&2
    exit 1
  fi

  tar -xzf "$TMP/trunk.tgz" -C "$TMP"
  mkdir -p "${HOME}/.cargo/bin"
  install -m 755 "$TMP/trunk" "${HOME}/.cargo/bin/trunk"
  rm -rf "$TMP"
  trap - EXIT
  echo "Installed trunk ${VER} (sha256 ${ACTUAL}) to ${HOME}/.cargo/bin/trunk"
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
# One clean build first so serve doesn't start from a broken dist.
# A failed build aborts here (set -e) instead of serving stale output.
trunk build "$@"
exec trunk serve "$@"
