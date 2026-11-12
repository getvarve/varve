#!/usr/bin/env bash
# Starts compose.prod.yaml with a server image built on this machine, the way an operator does, and checks that:
#
#   1. Compose refuses to start without each required setting, and the image refuses to start on missing, development
#      or malformed settings (among them the OAuth2 secrets, the mail server and sender, the web address and Redis),
#      and lists every problem;
#   2. with good settings it makes its OAuth2 signing keys (owned by the unprivileged user, the private one readable by
#      it alone), migrates the database, creates the apps' OAuth2 clients, starts the maintenance worker, and serves
#      /health, /api/v1/server-info, the authorization server metadata, /login and /register over HTTPS through the
#      proxy, as an unprivileged user, without development dependencies;
#   3. the keys survive a new API container (they live in a volume) and are never made again.
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
# Each run makes a new database password and OAuth2 passphrase, and the volumes keep the ones they were created with.
left="$(docker volume ls -q --filter "label=com.docker.compose.project=${project}" | tr '\n' ' ')"
if [ -n "$left" ]; then
  fail "volumes are left from an earlier run (${left% }); set VARVE_CHECK_PROJECT to a new name or remove them"
fi

echo "== The image alone, without any setting"
status=0
# The refusal wraps long lines, so only the start of each problem is compared.
output="$(docker run --rm "$image" 2>&1)" || status=$?
expect_refusal "$status" \
  'The production settings are not safe to start with:' \
  'APP_SECRET is not set.' \
  'OAUTH_PASSPHRASE is still a development or example value' \
  'OAUTH_ENCRYPTION_KEY is still a development or example value' \
  'API_PUBLIC_URL must be an https address' \
  'MAILER_DSN is not set to your mail server' \
  'VARVE_MAIL_FROM must be the sender of account mail' \
  'DATABASE_URL is not set to your database.'

# Every setting compose.prod.yaml requires, each with a value of this run. The check sends no mail, so the mail server
# only has to be a valid address.
cat >"$work/good.env" <<EOF
VARVE_VERSION=$version
VARVE_API_URL=https://localhost
APP_SECRET=$(openssl rand -hex 32)
POSTGRES_PASSWORD=$(openssl rand -hex 24)
OAUTH_PASSPHRASE=$(openssl rand -hex 24)
OAUTH_ENCRYPTION_KEY=$(openssl rand -hex 32)
MAILER_DSN=smtp://smtp.invalid:587
VARVE_MAIL_FROM="Varve <accounts@varve.example.com>"
VARVE_CHECK_PORT=$port
EOF

echo "== compose.prod.yaml without each required setting"
for setting in VARVE_API_URL APP_SECRET POSTGRES_PASSWORD OAUTH_PASSPHRASE OAUTH_ENCRYPTION_KEY MAILER_DSN VARVE_MAIL_FROM; do
  grep -v "^${setting}=" "$work/good.env" >"$work/without.env"
  status=0
  output="$(compose "$work/without.env" config --quiet 2>&1)" || status=$?
  expect_refusal "$status" "set ${setting}"
done

echo "== compose.prod.yaml with development and too-short settings"
development_secret="$(sed -n 's/^APP_SECRET=//p' apps/api/.env.dev)"
[ -n "$development_secret" ] || fail "apps/api/.env.dev has no APP_SECRET to try"
development_key="$(sed -n 's/^OAUTH_ENCRYPTION_KEY=//p' apps/api/.env)"
[ -n "$development_key" ] || fail "apps/api/.env has no OAUTH_ENCRYPTION_KEY to try"
cat >"$work/bad.env" <<EOF
VARVE_VERSION=$version
VARVE_API_URL=http://varve.example.com
VARVE_WEB_URL=http://web.varve.example.com
APP_SECRET=$development_secret
POSTGRES_PASSWORD=short
OAUTH_PASSPHRASE=short
OAUTH_ENCRYPTION_KEY=$development_key
MAILER_DSN=null://null
VARVE_MAIL_FROM="Varve <accounts@varve.example.invalid>"
EOF
status=0
# --no-deps: the check runs before anything touches the database, so none is started.
output="$(compose "$work/bad.env" run --rm --no-deps -T api 2>&1)" || status=$?
expect_refusal "$status" \
  'APP_SECRET is still a development or example value; generate a new one.' \
  'OAUTH_PASSPHRASE must be at least 16 characters.' \
  'OAUTH_ENCRYPTION_KEY is still a development or example value' \
  'API_PUBLIC_URL must be an https address' \
  'WEB_APP_URL must be empty or an https address' \
  'MAILER_DSN is not set to your mail server' \
  'VARVE_MAIL_FROM must be the sender of account mail' \
  'The database password in DATABASE_URL must be at least 16 characters.'

echo "== compose.prod.yaml with good settings but a REDIS_URL that is not Redis"
status=0
output="$(compose "$work/good.env" run --rm --no-deps -T -e REDIS_URL=http://redis:6379 api 2>&1)" || status=$?
expect_refusal "$status" 'REDIS_URL must be the redis:// or rediss:// address'
if grep -qF 'is not set' <<<"$output"; then
  fail "a good setting is reported as missing"
fi

echo "== compose.prod.yaml with good settings"
trap 'status=$?; if [ "$status" -ne 0 ]; then compose "$work/good.env" ps -a; compose "$work/good.env" logs --no-color --tail 200; fi' EXIT
compose "$work/good.env" up -d --wait --wait-timeout 300
compose "$work/good.env" ps

compose "$work/good.env" exec -T api php bin/console app:settings:check --no-interaction

uid="$(compose "$work/good.env" exec -T api id -u)"
echo "The API runs as user id ${uid}."
[ "$uid" != "0" ] || fail "the API runs as root"
compose "$work/good.env" exec -T api sh -c '! test -e vendor/phpunit && ! test -e tests && ! test -e .env.local' \
  || fail "the image carries development dependencies, tests or local settings"

# The signing keys: made by the API as its own user, the private one readable by that user alone, and opened by the
# passphrase of these settings.
keys="$(compose "$work/good.env" exec -T api stat -c '%n %u %a' config/jwt/private.pem config/jwt/public.pem)"
echo "OAuth2 signing keys (file, owner, mode):"
printf '%s\n' "$keys"
grep -qx "config/jwt/private.pem ${uid} 600" <<<"$keys" || fail "the private key is not the API user's alone"
grep -qx "config/jwt/public.pem ${uid} 644" <<<"$keys" || fail "the public key is not the API user's, readable by all"
compose "$work/good.env" exec -T api sh -c 'openssl pkey -in config/jwt/private.pem -passin env:OAUTH_PASSPHRASE -noout' \
  || fail "the private key does not open with OAUTH_PASSPHRASE"
fingerprint() {
  compose "$work/good.env" exec -T api sha256sum config/jwt/private.pem config/jwt/public.pem
}
first_keys="$(fingerprint)"
compose "$work/good.env" exec -T maintenance test -r config/jwt/private.pem \
  || fail "the maintenance worker cannot read the signing keys"

# The apps' OAuth2 clients: the desktop app on, the web client off while VARVE_WEB_URL is empty.
clients="$(compose "$work/good.env" exec -T postgres psql -U varve -d varve -tAc \
  "SELECT identifier || ' ' || active FROM oauth2_client ORDER BY identifier")"
echo "OAuth2 clients (identifier, active):"
printf '%s\n' "$clients"
[ "$clients" = $'desktop true\nweb false' ] || fail "the OAuth2 clients are not bootstrapped as expected"

# The maintenance worker consumes the schedule, and keeps running.
consuming='Consuming messages from transport "scheduler_default"'
for _ in $(seq 1 30); do
  if compose "$work/good.env" logs --no-color maintenance 2>&1 | grep -qF "$consuming"; then
    break
  fi
  sleep 2
done
compose "$work/good.env" logs --no-color maintenance 2>&1 | grep -F "$consuming" \
  || fail "the maintenance worker does not consume scheduler_default"
worker="$(compose "$work/good.env" ps -q maintenance)"
sleep 5
worker_state="$(docker inspect -f '{{.State.Status}} {{.RestartCount}}' "$worker")"
echo "maintenance (state, restarts): ${worker_state}"
[ "$worker_state" = "running 0" ] || fail "the maintenance worker does not keep running"
schedule="$(compose "$work/good.env" exec -T maintenance php bin/console debug:scheduler --no-interaction)"
for job in 'App\Account\PurgeAbandonedSignUps' 'App\Security\OAuth\PurgeExpiredOAuthTokens'; do
  grep -qF "$job" <<<"$schedule" || fail "the schedule the worker runs has no ${job}"
done
echo "The maintenance worker's schedule has the nightly purges."

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

metadata="$(get /.well-known/oauth-authorization-server)"
echo "GET /.well-known/oauth-authorization-server: ${metadata}"
jq -e '.issuer == "https://localhost" and .token_endpoint == "https://localhost/oauth/token"
  and .code_challenge_methods_supported == ["S256"]' <<<"$metadata" >/dev/null \
  || fail "the authorization server metadata does not name this server"

for page in /login /register; do
  html="$(get "$page")"
  grep -qi '<form' <<<"$html" || fail "${page} shows no form"
  echo "GET ${page}: 200 with a form"
done

echo "== A new API container keeps the signing keys"
compose "$work/good.env" up -d --wait --wait-timeout 300 --force-recreate --no-deps api
[ "$(fingerprint)" = "$first_keys" ] || fail "the signing keys changed when the API container was made again"
echo "The signing keys are the same after the API container was made again."
compose "$work/good.env" logs --no-color api 2>&1 | grep -F 'First-party OAuth2 clients are up to date' \
  || fail "the new API container did not bring the OAuth2 clients up to date"
health="$(get /health)"
echo "GET /health after the new container: ${health}"
jq -e '.status == "ok"' <<<"$health" >/dev/null || fail "/health is not ok after the new container"

echo "server-image-check: all checks passed."
echo "Remove the stack with:"
echo "  docker compose -p $project -f compose.prod.yaml -f scripts/ci/compose.prod.check.yaml --env-file $work/good.env down"
