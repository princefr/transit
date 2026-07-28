#!/usr/bin/env bash
# Smoke-check a running transit server (default http://127.0.0.1:8080).
# Usage: BASE_URL=http://127.0.0.1:8080 ./scripts/smoke_live.sh
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8080}"

echo "==> GET ${BASE_URL}/health"
health="$(curl -fsS "${BASE_URL}/health")"
echo "${health}" | head -c 500
echo

echo "==> POST ${BASE_URL}/graphql (health query)"
gql_body='{"query":"query { health { status stopCount tripCount epochId feeds { id ok } } }"}'
resp="$(curl -fsS -X POST "${BASE_URL}/graphql" \
  -H 'Content-Type: application/json' \
  -d "${gql_body}")"
echo "${resp}" | head -c 800
echo

# Basic sanity: JSON should mention status
if echo "${resp}" | grep -q '"status"'; then
  echo "OK: GraphQL health responded with status field"
else
  echo "WARN: GraphQL response missing status field" >&2
  exit 1
fi

echo "smoke_live: all checks passed"
