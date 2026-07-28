#!/usr/bin/env bash
# Best-effort listing of French National Access Point (PAN) datasets that
# mention GTFS / GTFS-RT. Not a full crawler — use for multi-feed discovery.
#
# Usage:
#   ./scripts/list_pan_feeds.sh
#   QUERY=idfm ./scripts/list_pan_feeds.sh
#   QUERY="ile-de-france" LIMIT=20 ./scripts/list_pan_feeds.sh
#
# API: https://transport.data.gouv.fr/api/datasets
set -euo pipefail

QUERY="${QUERY:-gtfs}"
LIMIT="${LIMIT:-40}"
API="${PAN_API:-https://transport.data.gouv.fr/api/datasets}"

if ! command -v curl >/dev/null 2>&1; then
  echo "curl required" >&2
  exit 1
fi

echo "# PAN feed discovery (query=${QUERY}, limit=${LIMIT})"
echo "# Source: ${API}"
echo

# The API returns a JSON array of datasets. We grep for useful fields without
# requiring jq (but prefer jq when present).
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

if ! curl -fsSL --max-time 30 \
  -H "Accept: application/json" \
  -H "User-Agent: transit-rs/0.1 (list_pan_feeds)" \
  "${API}" -o "$tmp"; then
  echo "Failed to fetch ${API}" >&2
  exit 1
fi

if command -v jq >/dev/null 2>&1; then
  jq -r --arg q "$QUERY" --argjson lim "$LIMIT" '
    def hay: ([.title, .slug, (.description // "")] | join(" ") | ascii_downcase);
    [ .[]
      | select(hay | test($q; "i"))
      | {
          title: .title,
          slug: .slug,
          page: ("https://transport.data.gouv.fr/datasets/" + (.slug // .id // "")),
          resources: [
            (.resources // [])[]
            | select(
                ((.format // "") | ascii_downcase | test("gtfs|neptune|zip"))
                or ((.title // "") | test("gtfs|GTFS|GTFS-RT|gtfs-rt"; "i"))
                or ((.url // "") | test("gtfs"; "i"))
              )
            | {title: .title, format: .format, url: .url}
          ]
        }
      | select(.resources | length > 0)
    ]
    | .[0:$lim]
    | .[]
    | "### \(.title)\n\(.page)\n"
      + (.resources[] | "  - [\(.format // "?")] \(.title // "")\n    \(.url // "")\n")
  ' "$tmp"
else
  # Fallback: crude line filter for gtfs-ish URLs and titles
  echo "(install jq for structured output; showing raw matches)"
  # shellcheck disable=SC2002
  cat "$tmp" | tr '{' '\n' | grep -iE 'gtfs|trip.?update|service.?alert|vehicle.?position' \
    | grep -iE "$QUERY|gtfs" \
    | head -n "$LIMIT" || true
fi

echo
echo "# Tip: open the dataset page and copy static_url / trip_updates_url into config/default.toml [[feeds]]."
echo "# IDFM example block is commented in config/default.toml."
