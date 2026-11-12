#!/usr/bin/env bash
# Starts compose.prod.yaml with a server image built on this machine, the way an operator does, and checks that:
#
#   1. the image refuses to start on missing or development settings, and lists every problem;
#   2. with good settings it migrates the database and serves /health and /api/v1/server-info over HTTPS through the
#      proxy, as an unprivileged user, without development dependencies.
#
# The proxy takes a certificate for https://localhost from Caddy's own local authority and listens on one local port
# (scripts/ci/compose.prod.check.yaml); everything else is compose.prod.yaml as shipped.
#
# Usage: scripts/ci/server-image-check.sh VERSION
#   VERSION is the tag of ghcr.io/getvarve/varve-server to start, already on this machine (docker build -t ...).
# Environment: VARVE_CHECK_PROJECT, the compose project (default varve-server-check); VARVE_CHECK_PORT, the local
# HTTPS port (default 18443).
#
# The stack stays up afterwards, to look at; the last lines printed say how to remove it.
set -euo pipefail

version="${1:?usage: server-image-check.sh VERSION}"
project="${VARVE_CHECK_PROJECT:-varve-server-check}"
port="${VARVE_CHECK_PORT:-18443}"
image="ghcr.io/getvarve/varve-server:${version}"

cd "$(dirname "$0")/../.."
work="$(mktemp -d)"

compose() {
  local env_file="$1"
  shift
  docker compose -p "$project" -f compose.prod.yaml -f scripts/ci/compose.prod.check.yaml --env-file "$env_file" "$@"
}

fail() {
  echo "server-image-check: $*" >&2
  exit 1
}

# Expects the settings check to refuse: a non-zero exit and every line in $@ in the output ($output).
expect_refusal() {
  local status="$1"
  shift
  printf '%s\n' "$output"
  [ "$status" -ne 0 ] || fail "the server started with bad settings"
  local line
  for line in "$@"; do
    grep -qF -- "$line" <<<"$output" || fail "the refusal does not say: $line"
  done
}

docker image inspect "$image" >/dev/null || fail "no image $image here; build it first"
# Each run makes a new database password, and PostgreSQL keeps the one its volume was created with.
if docker volume inspect "${project}_postgres_data" >/dev/null 2>&1; then
  fail "the volume ${project}_postgres_data is left from an earlier run; set VARVE_CHECK_PROJECT to a new name or remove it"
fi

echo "== The image alone, without any setting"
status=0
# The refusal wraps long lines, so only the start of each problem is compared.
output="$(docker run --rm "$image" 2>&1)" || status=$?
expect_refusal "$status" \
  'The production settings are not safe to start with:' \
  'APP_SECRET is not set.' \
  'API_PUBLIC_URL must be an https address' \
  'DATABASE_URL is not set to your database.'

echo "== compose.prod.yaml with development and too-short settings"
development_secret="$(sed -n 's/^APP_SECRET=//p' apps/api/.env.dev)"
[ -n "$development_secret" ] || fail "apps/api/.env.dev has no APP_SECRET to try"
cat >"$work/bad.env" <<EOF
VARVE_VERSION=$version
VARVE_API_URL=http://varve.example.com
APP_SECRET=$development_secret
POSTGRES_PASSWORD=short
EOF
status=0
# --no-deps: the check runs before anything touches the database, so none is started.
output="$(compose "$work/bad.env" run --rm --no-deps -T api 2>&1)" || status=$?
expect_refusal "$status" \
  'APP_SECRET is still a development or example value; generate a new one.' \
  'API_PUBLIC_URL must be an https address' \
  'The database password in DATABASE_URL must be at least 16 characters.'

echo "== compose.prod.yaml with good settings"
cat >"$work/good.env" <<EOF
VARVE_VERSION=$version
VARVE_API_URL=https://localhost
APP_SECRET=$(openssl rand -hex 32)
POSTGRES_PASSWORD=$(openssl rand -hex 24)
VARVE_CHECK_PORT=$port
EOF
trap 'status=$?; if [ "$status" -ne 0 ]; then compose "$work/good.env" ps -a; compose "$work/good.env" logs --no-color --tail 200; fi' EXIT
compose "$work/good.env" up -d --wait --wait-timeout 300
compose "$work/good.env" ps

compose "$work/good.env" exec -T api php bin/console app:settings:check --no-interaction

uid="$(compose "$work/good.env" exec -T api id -u)"
echo "The API runs as user id ${uid}."
[ "$uid" != "0" ] || fail "the API runs as root"
compose "$work/good.env" exec -T api sh -c '! test -e vendor/phpunit && ! test -e tests && ! test -e .env.local' \
  || fail "the image carries development dependencies, tests or local settings"

# Caddy makes its local authority while it issues the certificate, just after it starts.
for _ in $(seq 1 30); do
  if compose "$work/good.env" exec -T proxy cat /data/caddy/pki/authorities/local/root.crt >"$work/root.crt" 2>/dev/null \
    && [ -s "$work/root.crt" ]; then
    break
  fi
  sleep 2
done
[ -s "$work/root.crt" ] || fail "the proxy made no local certificate authority"

get() {
  curl -fsS --retry 10 --retry-delay 2 --retry-all-errors --cacert "$work/root.crt" "https://localhost:${port}$1"
}

health="$(get /health)"
echo "GET /health: ${health}"
jq -e '.status == "ok" and .checks.database == "ok"' <<<"$health" >/dev/null || fail "/health is not ok"

info="$(get /api/v1/server-info)"
echo "GET /api/v1/server-info: ${info}"
jq -e '.product == "varve" and .apiVersion == 1 and .billing == false' <<<"$info" >/dev/null \
  || fail "/api/v1/server-info is not a Varve server without billing"

echo "server-image-check: all checks passed."
echo "Remove the stack with:"
echo "  docker compose -p $project -f compose.prod.yaml -f scripts/ci/compose.prod.check.yaml --env-file $work/good.env down"
