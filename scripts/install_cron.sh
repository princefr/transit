#!/usr/bin/env bash
# Install a user crontab entry for daily IDFM GTFS sync at 03:30 Europe/Paris.
# Usage:
#   ./scripts/install_cron.sh          # install
#   ./scripts/install_cron.sh --print  # show lines only
#   ./scripts/install_cron.sh --remove # remove our marker block
#
# Scheduling timezone: `TZ=...` in front of the command only changes the job's
# environment, NOT when cron fires it. The schedule is pinned to Paris time
# with a `CRON_TZ=Europe/Paris` line (cronie, fcron, busybox crond ≥1.35).
# Debian/Ubuntu's vixie-cron ignores CRON_TZ and uses the system timezone —
# prefer the systemd timer (scripts/transit.timer) there.
# Note: CRON_TZ applies to every crontab line after it. This script appends its
# block at the end of the crontab; entries you add below it later also run on
# Paris time unless you set CRON_TZ again.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SYNC="$ROOT/scripts/daily_sync.sh"
LOGFILE="$ROOT/data/idfm/sync.log"
MARKER="# transit-idfm-daily-sync"
BEGIN_MARKER="$MARKER begin"
END_MARKER="$MARKER end"

# crontab treats `%` as newline and lines are parsed by /bin/sh: refuse paths
# we cannot quote safely, single-quote the rest.
case "$ROOT" in
  *%*|*$'\n'*)
    echo "install_cron: repo path contains '%' or a newline; cannot install safely: $ROOT" >&2
    exit 1
    ;;
esac
sh_quote() {
  local s=${1//\'/\'\\\'\'}
  printf "'%s'" "$s"
}

CRON_LINE="30 3 * * * TZ=Europe/Paris $(sh_quote "$SYNC") >>$(sh_quote "$LOGFILE") 2>&1 $MARKER"
CRON_BLOCK="$BEGIN_MARKER
CRON_TZ=Europe/Paris
$CRON_LINE
$END_MARKER"

chmod +x "$SYNC" 2>/dev/null || true

# Current crontab without our block (BEGIN..END) and without legacy one-liners.
current_without_ours() {
  crontab -l 2>/dev/null \
    | awk -v b="$BEGIN_MARKER" -v e="$END_MARKER" '
        $0 == b { skip = 1; next }
        $0 == e { skip = 0; next }
        !skip' \
    | grep -vF "$MARKER" || true
}

if [[ "${1:-}" == "--print" ]]; then
  echo "$CRON_BLOCK"
  exit 0
fi

if [[ "${1:-}" == "--remove" ]]; then
  if crontab -l 2>/dev/null | grep -qF "$MARKER"; then
    current_without_ours | crontab -
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
trap 'rm -f "$TMP"' EXIT
current_without_ours >"$TMP"
printf '%s\n' "$CRON_BLOCK" >>"$TMP"
crontab "$TMP"

echo "Installed crontab entry:"
printf '  %s\n' "$CRON_BLOCK"
echo
echo "First run (recommended before enabling the server with idfm):"
echo "  $SYNC"
echo
echo "Verify:"
echo "  crontab -l | grep -A3 'transit-idfm-daily-sync begin'"
echo "  tail -f $LOGFILE"
