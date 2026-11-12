# Hosting

## The app is free and complete

Varve is one app. Every feature is in this repository, under AGPL-3.0, and works without an account or a server. There is no paid edition.

## Cloud features need a backend

Cloud sync across devices and team workspaces are planned, not built yet. They need a server to sync through, so they will only work once you connect the app to a Varve backend. Everything else keeps working without one.

The self-hostable backend, the Varve server, lives in this repository (`apps/api`), under the same license. You can run it today ([Run your own server](#run-your-own-server) below); the app connects to it from a later release on. Two ways to use it are planned:

| | Self-hosted | Varve Cloud |
| --- | --- | --- |
| Who runs the server | You | getvarve |
| Price | Free | Paid |
| Billing | None | Per organization, by seat |

- **Self-hosted** has no billing at all. The billing code is part of the same public code, and it is off by default.
- **Varve Cloud** is the managed hosting by getvarve. An organization pays for its seats; an account without a paid organization cannot use it.

What a backend stores, and how, is decided when it is built; this page will say so then.

## Run your own server

Every release publishes the server as an image on GitHub's container registry, `ghcr.io/getvarve/varve-server`, for amd64 and arm64, tagged with the release's version (such as `0.4.0`), its minor version (`0.4`) and `latest`. The app and the server share one version number. Varve Cloud runs the same image.

### What you need

- A Linux machine with [Docker Engine](https://docs.docker.com/engine/install/) and its Compose plugin, version 2.24 or newer (`docker compose version`). 1 GB of RAM is enough.
- A DNS name for the server, such as `varve.example.com`, whose A (and, if you use IPv6, AAAA) record points at that machine.
- Ports 80 and 443 of that machine reachable from the internet. The proxy uses port 80 to get its HTTPS certificate from Let's Encrypt and to send plain HTTP on to HTTPS.

### Files and settings

Make a folder for the server and download two files of the release you want into it (replace `<version>`, such as `0.4.0`):

```bash
mkdir varve-server && cd varve-server
curl -fsSLo compose.prod.yaml https://raw.githubusercontent.com/getvarve/varve/v<version>/compose.prod.yaml
curl -fsSLo .env https://raw.githubusercontent.com/getvarve/varve/v<version>/.env.prod.example
chmod 600 .env
```

Then fill in `.env`. Docker Compose reads it from the folder by itself.

| Setting | What it is | How to get it |
| --- | --- | --- |
| `VARVE_API_URL` | The address the app reaches the server at: `https://`, the DNS name in lower case, no path, no trailing slash. | Your DNS name, such as `https://varve.example.com`. |
| `APP_SECRET` | The server's own secret. At least 32 characters. | `openssl rand -hex 32` |
| `POSTGRES_PASSWORD` | The database password. At least 16 characters; use letters and digits only, since it goes into a URL. | `openssl rand -hex 24` |
| `VARVE_VERSION` | Optional: the release to run. Left out, `latest`. | A version from the [releases](https://github.com/getvarve/varve/releases), such as `0.4.0`. |

Generate every secret on the machine itself and keep a copy of `.env` somewhere safe. Set `POSTGRES_PASSWORD` before the first start: the database takes it once, when its volume is created, and ignores later changes. To change it afterwards, change it in the database first (`docker compose -f compose.prod.yaml exec postgres psql -U varve -d varve -c "ALTER USER varve PASSWORD '<new password>'"`), then in `.env`, then run `up -d` again.

### First start

```bash
docker compose -f compose.prod.yaml up -d
```

This starts three containers: `postgres` (PostgreSQL 18, its data in the volume `varve_postgres_data`), `api` (the Varve server) and `proxy` (Caddy, which gets the HTTPS certificate for `VARVE_API_URL` and passes every request on to the API). Each restarts by itself after a crash or a reboot of the machine.

Before anything else, the API checks its settings (`app:settings:check`). It refuses to start while a setting is missing, too short, still one of the development values in this repository, or while `VARVE_API_URL` is not a plain `https://` address, and lists every problem in its log. Once the settings are right, it migrates the database and starts serving. The first start takes up to a minute, including the certificate.

### Check that it works

All commands run in the server's folder.

1. All three containers run, and `api` is `healthy`:

   ```bash
   docker compose -f compose.prod.yaml ps
   ```

   If `api` restarts over and over, read why: `docker compose -f compose.prod.yaml logs api`. A settings problem is listed there under "The production settings are not safe to start with".

2. The settings check passes:

   ```bash
   docker compose -f compose.prod.yaml exec api php bin/console app:settings:check
   ```

   It prints `[OK] The production settings are complete.`

3. The server answers over HTTPS, from any machine:

   ```bash
   curl https://varve.example.com/health
   curl https://varve.example.com/api/v1/server-info
   ```

   `/health` answers `{"status":"ok","checks":{"database":"ok"}}` (and status 503 when the database does not answer). `/api/v1/server-info` answers `{"product":"varve","apiVersion":1,"billing":false,"trialDays":0}`; the app reads it to tell a Varve server, and whether it bills, before it signs in. If curl reports a certificate problem, check that the DNS name points at this machine and that port 80 is open, then read `docker compose -f compose.prod.yaml logs proxy`.

### Updating

Back up the database first (below): an update can migrate it, and an older version cannot read it after that.

1. Download the new release's `compose.prod.yaml` over yours, and compare its `.env.prod.example` with your `.env`. A release can add containers and settings; with the old file, a new setting in `.env` reaches no container.

   ```bash
   curl -fsSLo compose.prod.yaml https://raw.githubusercontent.com/getvarve/varve/v<version>/compose.prod.yaml
   curl -fsSL https://raw.githubusercontent.com/getvarve/varve/v<version>/.env.prod.example
   ```

2. Set `VARVE_VERSION` in `.env` to the new version (or leave it out to follow `latest`), then:

   ```bash
   docker compose -f compose.prod.yaml pull
   docker compose -f compose.prod.yaml up -d
   ```

   The API checks its settings and migrates the database as it starts. Check it as above.

To go back to an older version, restore the backup you made before the update into a database of that version; do not start an older image on a database a newer one migrated.

### Backups

Everything the server keeps is in the PostgreSQL volume `varve_postgres_data`. The proxy's volumes hold only its certificates, which it fetches again by itself.

While the server runs, a dump is the simplest backup. Keep it off the machine:

```bash
docker compose -f compose.prod.yaml exec -T postgres pg_dump -U varve -d varve --format=custom > varve-$(date +%F).dump
```

To restore one, stop the API, restore, and start it again:

```bash
docker compose -f compose.prod.yaml stop api proxy
docker compose -f compose.prod.yaml exec -T postgres pg_restore -U varve -d varve --clean --if-exists --no-owner < varve-2026-11-12.dump
docker compose -f compose.prod.yaml up -d
```

To copy the volume itself, as a disk snapshot would, stop the whole stack first so the files are consistent:

```bash
docker compose -f compose.prod.yaml stop
docker run --rm -v varve_postgres_data:/data:ro -v "$PWD":/backup alpine tar -czf /backup/varve-postgres-$(date +%F).tar.gz -C /data .
docker compose -f compose.prod.yaml up -d
```

A volume copy only works with the same major PostgreSQL version (18); a dump works with any newer one too.

### Billing is off

A self-hosted server never bills anyone. `compose.prod.yaml` sets `VARVE_BILLING` to `0`, `server-info` reports `"billing": false`, nobody is ever blocked for payment, and the server never contacts Stripe or any other payment service. Billing exists only for Varve Cloud.

### Without this compose file

The image works with any container runtime. Give it the settings `compose.prod.yaml` gives the `api` container: `APP_ENV=prod`, `APP_SECRET`, `API_PUBLIC_URL` and `DEFAULT_URI` (both your `https://` address), `DATABASE_URL` (`postgresql://<user>:<password>@<host>:5432/<database>?serverVersion=18&charset=utf8`), `CORS_ALLOW_ORIGIN` (`tauri://localhost http://tauri.localhost`, separated by spaces), `SYMFONY_TRUSTED_PROXIES` (the addresses of your proxy, or `private_ranges`) and `VARVE_BILLING=0`. Exactly one API container needs `RUN_MIGRATIONS=1`: it migrates the database when it starts. The API listens on plain HTTP on port 80 inside the container and runs as user id 1000; put a proxy that terminates HTTPS in front of it. Its health check is `GET /health`.

## Updates

The macOS app (through Sparkle), the Windows setup (`.exe`) and the Linux AppImage update themselves. From 0.2.0 on, a `.deb` or `.msi` install does not: update it by installing the new package from the [latest release](https://github.com/getvarve/varve/releases/latest). Version 0.1.7 and older still update such an install in the app, once, to the current release.

## Upgrading from Varve Community

Up to 0.1.7 the app was called Varve Community. It is the same app with the same bundle id, so updates keep coming in, and your connections, queries and activity stay where they were: in `<OS data dir>/Varve`, with secrets in the OS keychain under the service `varve`. Neither depends on the app's name.

- **macOS:** `install.sh` installs `Varve.app` and moves `Varve Community.app` to the Trash. An in-app update keeps the old file name, `Varve Community.app`; the app inside is Varve.
- **Windows:** the installer of Varve removes the Varve Community install with its own uninstaller, without its data, and moves its Start menu and desktop shortcuts to the new name. A taskbar pin keeps its old name but opens Varve.
- **Linux:** the AppImage keeps its path and `varve.desktop`. That entry still shows Varve Community in the app menu after an in-app update; run `install.sh` again to rename it. The `varve` deb package replaces `varve-community`.
