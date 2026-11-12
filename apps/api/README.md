# Varve server API

The Symfony API of the Varve server. It runs in Docker (PHP 8.5 on FrankenPHP, PostgreSQL 18); every command below
runs from the repository root.

## Start the stack

```bash
docker compose up -d
```

On first boot the API installs its dependencies and runs the migrations. Then:

| Service | Address |
|---|---|
| API | http://localhost:8090 (health check at `/health`, server info at `/api/v1/server-info`) |
| API docs (dev only) | http://localhost:8090/docs |
| PostgreSQL | `localhost:54330` (user, password and database: `app`) |

Accounts sign in through the server's own OAuth2 authorization server, whose access tokens are signed with a key pair
in `config/jwt` (never committed). Make it once, then create the desktop app's and the web client's OAuth2 clients:

```bash
docker compose exec api sh -c 'mkdir -p config/jwt && OAUTH_PASSPHRASE="$(php -r "require \"vendor/autoload.php\"; (new Symfony\\Component\\Dotenv\\Dotenv())->bootEnv(\".env\"); echo \$_SERVER[\"OAUTH_PASSPHRASE\"];")" && export OAUTH_PASSPHRASE && openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096 -aes-256-cbc -pass env:OAUTH_PASSPHRASE -out config/jwt/private.pem && openssl pkey -in config/jwt/private.pem -passin env:OAUTH_PASSPHRASE -pubout -out config/jwt/public.pem'
docker compose exec api php bin/console app:oauth:bootstrap-clients
```

Sign up at http://localhost:8090/register. In development account mail goes nowhere (`MAILER_DSN=null://null`), so
confirm an address with `docker compose exec api php bin/console dbal:run-sql "UPDATE app_user SET email_confirmed_at = NOW() WHERE email = 'you@example.com'"`,
or point `MAILER_DSN` at a mail catcher.

Stop it with `docker compose stop`; `docker compose down` also removes the containers, and the data stays in the
volumes.

## Run one test file

Create the test database once:

```bash
docker compose exec -e APP_ENV=test api sh -c 'php bin/console doctrine:database:create --if-not-exists && php bin/console doctrine:migrations:migrate -n --allow-no-migration'
```

Then run the test file for the part you changed:

```bash
docker compose exec -e APP_ENV=test api php bin/phpunit tests/Functional/ServerInfoTest.php
```

## Run PHPStan

```bash
docker compose exec api sh -c 'bin/console cache:warmup --env=test && vendor/bin/phpstan analyse'
```

## Settings

`.env` holds development defaults only. In production set at least `APP_SECRET`, `DATABASE_URL`, `API_PUBLIC_URL`,
`CORS_ALLOW_ORIGIN` (origins separated by spaces), `OAUTH_PASSPHRASE`, `OAUTH_ENCRYPTION_KEY`, `MAILER_DSN`,
`VARVE_MAIL_FROM`, `REDIS_URL` (never localhost) and `SYMFONY_TRUSTED_PROXIES` (the proxy in front of the API, or
`private_ranges`; never a value that trusts every address); `WEB_APP_URL` once the server has a web client.
`VARVE_BILLING` stays `0` on a self-hosted server, and `VARVE_SIGN_UP=0` closes sign-up. The production image runs
`bin/console app:settings:check` first and does not start while one of them is missing or still a development value,
while `APP_ENV` is not `prod`, or while `APP_DEBUG` is on.

What sync may store is capped by six settings, each a whole number above 0 with the default in `.env`; raise them if
you need to:

| Setting | Default | Caps |
|---|---|---|
| `VARVE_SYNC_MAX_RECORDS` | 50000 | Live synced records of one organization |
| `VARVE_SYNC_MAX_USER_RECORDS` | 10000 | Live per-user records of one user |
| `VARVE_SYNC_MAX_BYTES` | 536870912 (512 MiB) | Bytes of an organization's live record payloads |
| `VARVE_SYNC_MAX_USER_BYTES` | 67108864 (64 MiB) | Bytes of a user's live per-user record payloads |
| `VARVE_SYNC_MAX_REVISIONS` | 50 | Earlier versions kept per saved query |
| `VARVE_SYNC_MAX_REVISION_BYTES` | 8388608 (8 MiB) | Bytes of those earlier versions per saved query |

In production the session and remember-me cookies are always `Secure`, and sessions, limits and the refresh token
grace live in Redis; `/health` reports `redis` there next to `database`.

## Sync

Clients sync connection profiles, folders, saved queries and per-user records through `/api/v1/sync/...`; the wire
format, with its limits and errors, is in [docs/sync.md](docs/sync.md). Tombstones are purged 90 days after the delete
by the scheduled `App\Sync\PurgeSyncTombstones`, which the `scheduler_default` worker runs with the other purges.
