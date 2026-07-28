#!/usr/bin/env bash
# Daily IDFM (or FEED_ID) static GTFS sync.
# Downloads public GTFS zip → data/<feed>/incoming/ → validates → atomic promote current.zip
#
# Env:
#   FEED_ID          default idfm
#   GTFS_URL / IDFM_GTFS_URL  static zip URL
#   DATA_DIR         default $ROOT/data
#   RESTART=1        systemctl restart transit when unit exists
#   BUILD_LINES=1    rebuild lignes.geojson when inputs present
#   TRACES_GEOJSON   path to IDFM traces for BUILD_LINES
#   SKIP_LINES=1     never build lines
#   PRIM_DATASET_KEY  optional X-API-KEY (PRIM dataset download key — not IDFM_PRIM_API_KEY)
#   Legacy fallbacks: DATASETS_API_KEY, then DATAGOUV_API_KEY
#
# Cron 03:30 Europe/Paris: ./scripts/install_cron.sh
# systemd: scripts/transit.timer + transit-daily-sync.service
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -f "$ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$ROOT/.env"
  set +a
fi

FEED_ID="${FEED_ID:-idfm}"
DATA_DIR="${DATA_DIR:-$ROOT/data}"
FEED_DIR="$DATA_DIR/$FEED_ID"
INCOMING="$FEED_DIR/incoming"
LOG_DIR="$DATA_DIR/logs"
LOG="$LOG_DIR/daily_sync-${FEED_ID}.log"
FEED_LOG="$FEED_DIR/sync.log"
GTFS_URL="${GTFS_URL:-${IDFM_GTFS_URL:-https://www.data.gouv.fr/api/1/datasets/r/413988ed-d340-467b-8be2-7b999fcd207a}}"
UA="${SYNC_USER_AGENT:-transit-rs/0.1 daily_sync}"
RESTART="${RESTART:-0}"
BUILD_LINES="${BUILD_LINES:-0}"
LOCK="/tmp/transit-daily-sync-${FEED_ID}.lock"

CURL_EXTRA=()
if [[ -n "${PRIM_DATASET_KEY:-}" ]]; then
  CURL_EXTRA+=(-H "X-API-KEY: ${PRIM_DATASET_KEY}")
elif [[ -n "${DATASETS_API_KEY:-}" ]]; then
  CURL_EXTRA+=(-H "X-API-KEY: ${DATASETS_API_KEY}")
elif [[ -n "${DATAGOUV_API_KEY:-}" ]]; then
  CURL_EXTRA+=(-H "X-API-KEY: ${DATAGOUV_API_KEY}")
fi

mkdir -p "$INCOMING" "$FEED_DIR" "$LOG_DIR"
touch "$LOG" "$FEED_LOG"

log() {
  local msg="[$(date -Iseconds)] $*"
  echo "$msg" | tee -a "$LOG" | tee -a "$FEED_LOG" >/dev/null
  echo "$msg"
}

# Single-flight: concurrent cron/timer exits cleanly
exec 9>"$LOCK"
if ! flock -n 9; then
  log "another daily_sync for feed=$FEED_ID holds $LOCK; exit 0"
  exit 0
fi

log "=== daily_sync start feed=$FEED_ID ==="
log "url=$GTFS_URL root=$ROOT"

STAMP="$(date +%Y%m%d%H%M%S)"
TMP_ZIP="$INCOMING/${STAMP}.partial.zip"
FINAL_INCOMING="$INCOMING/${STAMP}.zip"

cleanup_partial() {
  rm -f "$TMP_ZIP" 2>/dev/null || true
}
trap cleanup_partial EXIT

log "downloading → $TMP_ZIP"
if ! curl -fL --retry 3 --retry-delay 5 --retry-all-errors \
  -A "$UA" \
  "${CURL_EXTRA[@]}" \
  --connect-timeout 30 \
  --max-time 1800 \
  -o "$TMP_ZIP" \
  "$GTFS_URL"; then
  log "ERROR: download failed"
  exit 1
fi

BYTES="$(wc -c <"$TMP_ZIP" | tr -d ' ')"
log "downloaded bytes=$BYTES"

if [[ "$BYTES" -lt 1000000 ]]; then
  log "ERROR: zip too small ($BYTES bytes); refusing to promote"
  exit 1
fi

log "validating zip members (stops.txt, stop_times.txt)"
if ! python3 - "$TMP_ZIP" <<'PY'
import sys, zipfile
from pathlib import Path
path = sys.argv[1]
need = {"stops.txt", "stop_times.txt"}
with zipfile.ZipFile(path, "r") as z:
    names = {Path(n).name.lower() for n in z.namelist()}
missing = need - names
if missing:
    print("missing:", ", ".join(sorted(missing)), file=sys.stderr)
    sys.exit(1)
print("ok members:", ", ".join(sorted(need)))
PY
then
  log "ERROR: zip validation failed"
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  NEW_SHA="$(sha256sum "$TMP_ZIP" | awk '{print $1}')"
else
  NEW_SHA="$(shasum -a 256 "$TMP_ZIP" | awk '{print $1}')"
fi
log "sha256=$NEW_SHA"

CURRENT="$FEED_DIR/current.zip"
if [[ -f "$CURRENT" ]]; then
  if command -v sha256sum >/dev/null 2>&1; then
    OLD_SHA="$(sha256sum "$CURRENT" | awk '{print $1}')"
  else
    OLD_SHA="$(shasum -a 256 "$CURRENT" | awk '{print $1}')"
  fi
  if [[ "$OLD_SHA" == "$NEW_SHA" ]]; then
    log "content unchanged (sha256 match); skip promote"
    mv -f "$TMP_ZIP" "$FINAL_INCOMING" 2>/dev/null || rm -f "$TMP_ZIP"
    trap - EXIT
    ls -1t "$INCOMING"/*.zip 2>/dev/null | tail -n +4 | xargs -r rm -f || true
    log "=== daily_sync done feed=$FEED_ID (no change) ==="
    exit 0
  fi
fi

mv -f "$TMP_ZIP" "$FINAL_INCOMING"
trap - EXIT

PROMOTE_TMP="$FEED_DIR/current.zip.tmp.$$"
cp -f "$FINAL_INCOMING" "$PROMOTE_TMP"
mv -f "$PROMOTE_TMP" "$CURRENT"
log "promoted → $CURRENT"

META="$FEED_DIR/current.meta.json"
cat >"$META" <<EOF
{
  "sha256": "$NEW_SHA",
  "loaded_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "static_url": "$GTFS_URL",
  "source": "daily_sync.sh"
}
EOF
log "meta=$META"

touch "$FEED_DIR/reload"
log "touched $FEED_DIR/reload"

if [[ "${SKIP_LINES:-0}" != "1" ]] && [[ "$BUILD_LINES" == "1" ]]; then
  REF="$FEED_DIR/referentiel-des-lignes.geojson"
  TRACES="${TRACES_GEOJSON:-}"
  if [[ -f "$REF" ]] && [[ -n "$TRACES" ]] && [[ -f "$TRACES" ]] \
    && [[ -x "$ROOT/scripts/build_idfm_lines.py" || -f "$ROOT/scripts/build_idfm_lines.py" ]]; then
    log "building lignes.geojson (BUILD_LINES=1)"
    if python3 "$ROOT/scripts/build_idfm_lines.py" "$REF" "$TRACES"; then
      log "lignes.geojson refreshed"
    else
      log "WARN: build_idfm_lines.py failed (non-fatal)"
    fi
  else
    log "BUILD_LINES=1 but missing referentiel and/or TRACES_GEOJSON; skip"
  fi
fi

ls -1t "$INCOMING"/*.zip 2>/dev/null | tail -n +4 | xargs -r rm -f || true

if [[ "$RESTART" == "1" ]]; then
  if command -v systemctl >/dev/null 2>&1 && systemctl cat transit.service >/dev/null 2>&1; then
    log "RESTART=1 → systemctl restart transit"
    if systemctl restart transit; then
      log "transit restarted"
    else
      log "WARN: systemctl restart transit failed (non-fatal for sync success)"
    fi
  else
    log "RESTART=1 but transit.service not installed; skip restart (reload on next boot / static check)"
  fi
fi

log "=== daily_sync done feed=$FEED_ID ==="
