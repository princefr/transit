#!/usr/bin/env bash
# Install a user crontab entry for daily IDFM GTFS sync at 03:30 Europe/Paris.
# Usage:
#   ./scripts/install_cron.sh          # install
#   ./scripts/install_cron.sh --print  # show line only
#   ./scripts/install_cron.sh --remove # remove our marker lines
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SYNC="$ROOT/scripts/daily_sync.sh"
MARKER="# transit-idfm-daily-sync"
# 03:30 Europe/Paris year-round: use TZ= in crontab (cronie / vixie-cron with TZ support)
CRON_LINE="30 3 * * * TZ=Europe/Paris $SYNC >>$ROOT/data/idfm/sync.log 2>&1 $MARKER"

chmod +x "$SYNC" 2>/dev/null || true

if [[ "${1:-}" == "--print" ]]; then
  echo "$CRON_LINE"
  exit 0
fi

if [[ "${1:-}" == "--remove" ]]; then
  if crontab -l 2>/dev/null | grep -qF "$MARKER"; then
    crontab -l 2>/dev/null | grep -vF "$MARKER" | crontab -
    echo "Removed transit IDFM daily sync from crontab."
  else
    echo "No transit IDFM cron entry found."
  fi
  exit 0
fi

if [[ ! -x "$SYNC" ]]; then
  chmod +x "$SYNC"
fi

mkdir -p "$ROOT/data/idfm"

TMP="$(mktemp)"
crontab -l 2>/dev/null | grep -vF "$MARKER" >"$TMP" || true
echo "$CRON_LINE" >>"$TMP"
crontab "$TMP"
rm -f "$TMP"

echo "Installed crontab entry:"
echo "  $CRON_LINE"
echo
echo "First run (recommended before enabling the server with idfm):"
echo "  $SYNC"
echo
echo "Verify:"
echo "  crontab -l | grep transit-idfm"
echo "  tail -f $ROOT/data/idfm/sync.log"
