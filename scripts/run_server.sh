#!/usr/bin/env bash
# Load local secrets and run the transit GraphQL server (release).
#
# Usage:
#   ./scripts/run_server.sh
#   ./scripts/run_server.sh --bind 127.0.0.1:8080   # extra args forwarded to binary after cargo
#
# Env:
#   TRANSIT_ROOT   — repo root (default: parent of scripts/)
#   RUST_LOG       — default info,transit=info
#   SKIP_CARGO     — if 1, run target/release/transit instead of cargo run
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${TRANSIT_ROOT:-$(cd "${SCRIPT_DIR}/.." && pwd)}"
cd "${ROOT}"

# Export key=value from .env (ignore comments / blank lines). Do not print values.
if [[ -f "${ROOT}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT}/.env"
  set +a
fi

export RUST_LOG="${RUST_LOG:-info,transit=info}"

if [[ ! -f "${ROOT}/config/default.toml" ]]; then
  echo "run_server: missing config/default.toml under ${ROOT}" >&2
  exit 1
fi

if [[ "${SKIP_CARGO:-0}" == "1" ]]; then
  BIN="${ROOT}/target/release/transit"
  if [[ ! -x "${BIN}" ]]; then
    echo "run_server: ${BIN} not found; build with: cargo build --release" >&2
    exit 1
  fi
  exec "${BIN}" "$@"
fi

exec cargo run --release --bin transit -- "$@"
