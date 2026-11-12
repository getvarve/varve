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

`.env` holds development defaults only. In production set at least `APP_SECRET`, `DATABASE_URL`, `API_PUBLIC_URL` and
`CORS_ALLOW_ORIGIN` (origins separated by spaces); `VARVE_BILLING` stays `0` on a self-hosted server. The production
image runs `bin/console app:settings:check` first and does not start while one of them is missing or still a
development value.
