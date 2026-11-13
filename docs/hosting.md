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

A backend stores your connection profiles, folders and saved queries so they sync between devices, never a password, passphrase or key: those stay in the keychain of each computer.

## Run your own server

Every release publishes the server as an image on GitHub's container registry, `ghcr.io/getvarve/varve-server`, for amd64 and arm64, tagged with the release's version (such as `0.4.0`), its minor version (`0.4`) and `latest`. The app and the server share one version number. Varve Cloud runs the same image.

> **The image is not public yet.** The server is new: the desktop app cannot sign in to it yet, and nobody outside its tests has run it. Until that changes, pulling the image answers `denied` for anyone outside the getvarve organization. To try the server now, build the image yourself in a checkout of this repository, under the name `compose.prod.yaml` looks for (use the `VARVE_VERSION` you put in `.env`, or `latest` when you set none):
>
> ```bash
> docker build --target prod --file docker/api/Dockerfile --tag ghcr.io/getvarve/varve-server:latest .
> ```
>
> Then follow the steps below. Compose uses the image you built and pulls nothing; to update, check out the new release and build again instead of `docker compose pull`.

### What you need

- A Linux machine with [Docker Engine](https://docs.docker.com/engine/install/) and its Compose plugin, version 2.24 or newer (`docker compose version`). 1 GB of RAM is enough.
- A DNS name for the server, such as `varve.example.com`, whose A (and, if you use IPv6, AAAA) record points at that machine.
- Ports 80 and 443 of that machine reachable from the internet. The proxy uses port 80 to get its HTTPS certificate from Let's Encrypt and to send plain HTTP on to HTTPS.
- A mail server (SMTP) that may send mail for an address on a domain of yours, such as your mail provider's SMTP server or a transactional mail service. The server mails a link to confirm a new account's address and a link to reset a forgotten password; without working mail nobody can finish signing up or reset a password.

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
| `OAUTH_PASSPHRASE` | Encrypts the key the server signs its sign-in tokens with. At least 16 characters. | `openssl rand -hex 24` |
| `OAUTH_ENCRYPTION_KEY` | Encrypts the refresh tokens the apps keep their sign-in with. At least 32 characters. | `openssl rand -hex 32` |
| `MAILER_DSN` | The mail server account mail goes out through, as a URL: `smtp://<user>:<password>@<host>:<port>`. Characters such as `@`, `:` or `/` in the user or password are URL-encoded (`%40`, `%3A`, `%2F`). | From your mail provider, such as `smtp://varve%40example.com:<password>@smtp.example.com:587`. |
| `VARVE_MAIL_FROM` | The sender of account mail: an address on your own domain that the mail server may send for. In quotes, because of the space. | Such as `"Varve <accounts@varve.example.com>"`. |
| `VARVE_WEB_URL` | Optional: the address of a web client for this server, once you run one, written like `VARVE_API_URL`. Left out, only the desktop app signs in. | Your web client's address. |
| `VARVE_SIGN_UP` | Optional: `0` closes sign-up, so only accounts you make yourself exist ([Who can make an account](#who-can-make-an-account)). Left out or `1`, anyone who can reach the server can sign up. | `0` or `1`. |
| `VARVE_VERSION` | The release to run. Set it: left out, the server runs `latest`, and every `docker compose pull` then moves it to the newest release and migrates its database without you choosing to. | The version whose files you downloaded, such as `0.4.0`. |

Generate every secret on the machine itself and keep a copy of `.env` somewhere safe. Set `POSTGRES_PASSWORD` before the first start: the database takes it once, when its volume is created, and ignores later changes. To change it afterwards, change it in the database first (`docker compose -f compose.prod.yaml exec postgres psql -U varve -d varve -c "ALTER USER varve PASSWORD '<new password>'"`), then in `.env`, then run `up -d` again.

Two more settings are best set once and kept:

- `OAUTH_PASSPHRASE` only applies when the signing key is made, on the first start. To change it, remove the key (`docker compose -f compose.prod.yaml run --rm --no-deps --entrypoint rm api -f config/jwt/private.pem config/jwt/public.pem`), set the new passphrase and run `up -d`: the API makes a new key, every access token the server handed out stops working, and each app has to renew its sign-in.
- A new `OAUTH_ENCRYPTION_KEY` makes every refresh token unreadable: everyone signs in again on every device.

A new `APP_SECRET` ends "Keep me signed in" in every browser.

Many mail services only accept a sender on a domain you have verified with them. Send yourself a password reset after the first start (below) to check the mail settings: a wrong `MAILER_DSN` shows in `docker compose -f compose.prod.yaml logs api`.

### First start

```bash
docker compose -f compose.prod.yaml up -d
```

This starts five containers, each of which restarts by itself after a crash or a reboot of the machine:

- `postgres`: PostgreSQL 18, with everything the server keeps in the volume `varve_postgres_data`.
- `redis`: Redis 8.8, which holds the sign-in pages' sessions, the limits on sign-ups, sign-in attempts and account mail, and a short grace period when an app renews its sign-in. It writes to disk (append-only, the volume `varve_redis_data`), so a restart keeps them.
- `api`: the Varve server. It keeps the key it signs sign-in tokens with in the volume `varve_oauth_keys`.
- `maintenance`: the same image, running the scheduled jobs: every night it removes sign-ups whose address was never confirmed, sign-in tokens that have expired, and the markers of synced records that were deleted more than 90 days ago.
- `proxy`: Caddy, which gets the HTTPS certificate for `VARVE_API_URL` and passes every request on to the API. It tells browsers to use HTTPS only from then on (`Strict-Transport-Security`, for a year). The API trusts it (and only containers of this stack can reach the API) to tell it each visitor's address, which the sign-in and sign-up limits count by.

Each container keeps at most 50 MB of its own log (five files of 10 MB).

Before anything else, the API checks its settings (`app:settings:check`). It refuses to start while a setting is missing, too short, still one of the development values in this repository, while `VARVE_API_URL` (or `VARVE_WEB_URL`, when set) is not a plain `https://` address, while the mail server or sender is missing, or while debug mode is on, and lists every problem in its log. Once the settings are right, it makes its signing key on the first start (it keeps the one in `varve_oauth_keys` on every later start, and every container checks on every start that `OAUTH_PASSPHRASE` still opens it), migrates the database, creates or updates the OAuth2 clients of the desktop app and the web client, and starts serving. The `maintenance` container starts once the API is healthy. The first start takes up to a minute, including the certificate and the key.

### Who can make an account

Out of the box anyone who can reach the server can sign up at `/register`, and the server has to be reachable from the internet for its certificate. Sign-ups are limited to 10 an hour per network address, every address has to be confirmed by mail, and one account can store at most 50 000 records, but a stranger can still make an account on your server.

If the server is only for you, or for people you choose, close sign-up once your own account exists: set `VARVE_SIGN_UP=0` in `.env` and run `docker compose -f compose.prod.yaml up -d`. `/register` then no longer makes accounts, and you add each person yourself:

```bash
docker compose -f compose.prod.yaml exec api php bin/console app:users:create ada@example.com "Ada Lovelace"
```

The command asks for a password, which you pass on to that person; they can sign in at once, without a confirmation mail, and change the password in the app. Signing in, password resets and everything else keep working for the accounts that exist. Set `VARVE_SIGN_UP=1` (or remove the line) and run `up -d` again to open sign-up.

### Check that it works

All commands run in the server's folder.

1. All five containers run and are `healthy`:

   ```bash
   docker compose -f compose.prod.yaml ps
   ```

   If `api` restarts over and over, read why: `docker compose -f compose.prod.yaml logs api`. A settings problem is listed there under "The production settings are not safe to start with". `docker compose -f compose.prod.yaml logs maintenance` shows `Consuming messages from transport "scheduler_default"` once the worker runs.

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

   `/health` answers `{"status":"ok","checks":{"database":"ok","redis":"ok"}}` (and status 503, with `failing` for the one that does not answer, when the database or Redis is down). `/api/v1/server-info` answers `{"product":"varve","apiVersion":1,"billing":false,"trialDays":0}`; the app reads it to tell a Varve server, and whether it bills, before it signs in. If curl reports a certificate problem, check that the DNS name points at this machine and that port 80 is open, then read `docker compose -f compose.prod.yaml logs proxy`.

4. Sign-in works: `curl https://varve.example.com/.well-known/oauth-authorization-server` names your address as `issuer`. Open `https://varve.example.com/register` in a browser, create your account, and open the link in the confirmation mail (then decide [who else may make one](#who-can-make-an-account)). Then use "Forgot your password?" on `https://varve.example.com/login` once: the reset mail proves the mail settings work for both kinds of mail.

### Updating

Back up the database first (below): an update can migrate it, and an older version cannot read it after that.

1. Download the new release's `compose.prod.yaml` over yours, and compare its `.env.prod.example` with your `.env`. A release can add containers and settings; with the old file, a new setting in `.env` reaches no container.

   ```bash
   curl -fsSLo compose.prod.yaml https://raw.githubusercontent.com/getvarve/varve/v<version>/compose.prod.yaml
   curl -fsSL https://raw.githubusercontent.com/getvarve/varve/v<version>/.env.prod.example
   ```

2. Set `VARVE_VERSION` in `.env` to the new version, then:

   ```bash
   docker compose -f compose.prod.yaml pull
   docker compose -f compose.prod.yaml up -d
   docker compose -f compose.prod.yaml up -d --force-recreate proxy
   ```

   The API checks its settings and migrates the database as it starts. The last line is needed because the proxy's settings are written inside `compose.prod.yaml`, and Docker Compose does not start the proxy again when only they changed. Check the server as above.

To go back to an older version, set `VARVE_VERSION` back to it (and put back that version's `compose.prod.yaml`), then restore the dump you made before the update with the steps under [Backups](#backups). Never start an older image on a database a newer one migrated: the restore replaces the whole database, which is what makes this safe.

### Backups

Back up three things, and keep them off the machine:

- The database, the volume `varve_postgres_data`: accounts, devices and everything the apps sync.
- The signing key, the volume `varve_oauth_keys`. If it is lost, the API makes a new one on its next start: every access token the server handed out stops working at once, and each app has to renew its sign-in. The key is encrypted with `OAUTH_PASSPHRASE`, so a copy of it only works with that setting.
- Your `.env`: without `OAUTH_PASSPHRASE` the key cannot be opened, and without `OAUTH_ENCRYPTION_KEY` no refresh token can be read.

Redis (`varve_redis_data`) needs no backup: losing it only signs browsers out of the sign-in pages and resets the limits. The proxy's volumes hold only its certificates, which it fetches again by itself.

While the server runs, a dump is the simplest backup of the database:

```bash
docker compose -f compose.prod.yaml exec -T postgres pg_dump -U varve -d varve --format=custom > varve-$(date +%F).dump
```

To restore one, stop everything that uses the database, replace the database with a new, empty one, fill that from the dump, and start again:

```bash
docker compose -f compose.prod.yaml stop api maintenance proxy
docker compose -f compose.prod.yaml exec -T postgres dropdb -U varve --force varve
docker compose -f compose.prod.yaml exec -T postgres createdb -U varve varve
docker compose -f compose.prod.yaml exec -T postgres pg_restore -U varve -d varve --no-owner --single-transaction < varve-2026-11-12.dump
docker compose -f compose.prod.yaml up -d
```

`dropdb` deletes everything the server holds now, so be sure of the dump first. The restore goes into an empty database on purpose: restoring over the tables that are there leaves behind whatever a newer version added, and the API then fails to migrate and restarts forever. `--single-transaction` restores all of the dump or nothing. As it starts, the API migrates the restored database to the version it runs, so a dump from an older version works with a newer image, never the other way around. Then check the server as after the first start. Redis needs nothing: a browser whose session no longer fits signs in again.

The signing key never changes while the server runs, so copy it once after the first start (and again only if you make a new one):

```bash
docker run --rm -v varve_oauth_keys:/keys:ro -v "$PWD":/backup alpine tar -czf /backup/varve-oauth-keys.tar.gz -C /keys .
```

To move the server to a new machine, put `compose.prod.yaml`, your `.env`, the dump and the key copy in the server's folder there, and restore the key and the database before the API starts for the first time; otherwise the API makes a new key and the restore has to replace it (stop the API first). `--numeric-owner` keeps the files the API's own (user id 1000) and the private key readable by it alone:

```bash
docker compose -f compose.prod.yaml up --no-start
docker run --rm -v varve_oauth_keys:/keys -v "$PWD":/backup alpine tar -xzf /backup/varve-oauth-keys.tar.gz --numeric-owner -C /keys
docker compose -f compose.prod.yaml up -d --wait postgres
docker compose -f compose.prod.yaml exec -T postgres dropdb -U varve --force varve
docker compose -f compose.prod.yaml exec -T postgres createdb -U varve varve
docker compose -f compose.prod.yaml exec -T postgres pg_restore -U varve -d varve --no-owner --single-transaction < varve-2026-11-12.dump
docker compose -f compose.prod.yaml up -d
```

To put back only the key on a machine that already runs, leave out the four `postgres` lines.

To copy the database volume itself, as a disk snapshot would, stop the whole stack first so the files are consistent:

```bash
docker compose -f compose.prod.yaml stop
docker run --rm -v varve_postgres_data:/data:ro -v "$PWD":/backup alpine tar -czf /backup/varve-postgres-$(date +%F).tar.gz -C /data .
docker compose -f compose.prod.yaml up -d
```

A volume copy only works with the same major PostgreSQL version (18); a dump works with any newer one too.

### Billing is off

A self-hosted server never bills anyone. `compose.prod.yaml` sets `VARVE_BILLING` to `0`, `server-info` reports `"billing": false`, nobody is ever blocked for payment, and the server never contacts Stripe or any other payment service. Billing exists only for Varve Cloud.

### Without this compose file

The image works with any container runtime. Give it the settings `compose.prod.yaml` gives the `api` container: `APP_ENV=prod`, `APP_SECRET`, `API_PUBLIC_URL` and `DEFAULT_URI` (both your `https://` address), `WEB_APP_URL` (your web client's address, or empty), `DATABASE_URL` (`postgresql://<user>:<password>@<host>:5432/<database>?serverVersion=18&charset=utf8`), `REDIS_URL` (`redis://<host>:6379`, or `rediss://` for TLS), `OAUTH_PASSPHRASE`, `OAUTH_ENCRYPTION_KEY`, `MAILER_DSN`, `VARVE_MAIL_FROM`, `CORS_ALLOW_ORIGIN` (the web client's address when there is one, then `tauri://localhost http://tauri.localhost`, separated by spaces), `SYMFONY_TRUSTED_PROXIES`, `VARVE_SIGN_UP` (`1` or `0`) and `VARVE_BILLING=0`. Leave `APP_DEBUG` out: the API does not start with it on.

`SYMFONY_TRUSTED_PROXIES` lists the addresses or ranges of your proxy, separated by commas (or `private_ranges` when only the proxy can reach the API over a private network). The API takes a visitor's address from the proxy's `X-Forwarded-For`, and that the visitor came over HTTPS from `X-Forwarded-Proto`, only when the request comes from one of them, and the limits on sign-ups, sign-in attempts and account mail count per visitor's address: without it, every visitor would count as the proxy's one address and a few wrong passwords would lock the sign-in form for everyone. So the API refuses to start while it is empty, while it names something that is not an address or a range, and while it trusts every address (`0.0.0.0/0`, `::/0`, `REMOTE_ADDR`), with which a visitor could claim any address. Never trust more than your proxy.

What sync may store is capped per organization and per user. Each limit has a default built into the image and is a whole number above 0; give the API and the worker another value only if you need one (with `compose.prod.yaml`, set it in `.env`):

| Setting | Default | Caps |
| --- | --- | --- |
| `VARVE_SYNC_MAX_RECORDS` | `50000` | The connections, folders and saved queries one organization holds. |
| `VARVE_SYNC_MAX_USER_RECORDS` | `10000` | The favourites, preferences and overrides one user holds. |
| `VARVE_SYNC_MAX_BYTES` | `536870912` (512 MiB) | The size of those records together, per organization. |
| `VARVE_SYNC_MAX_USER_BYTES` | `67108864` (64 MiB) | The size of those records together, per user. |
| `VARVE_SYNC_MAX_REVISIONS` | `50` | The earlier versions kept of one saved query. |
| `VARVE_SYNC_MAX_REVISION_BYTES` | `8388608` (8 MiB) | The size of those earlier versions together, per saved query. |

Your proxy has to terminate HTTPS, pass the `Host` header on unchanged, replace (not extend) a visitor's own `X-Forwarded-For`, and send `Strict-Transport-Security: max-age=31536000` with every answer, as the proxy of `compose.prod.yaml` does; the API does not send that header itself. The API's cookies are always marked `Secure`, so sign-in only works over HTTPS.

Exactly one API container needs `RUN_MIGRATIONS=1`: when it starts it makes the signing key in `/app/config/jwt` (once; keep that folder on a volume owned by user id 1000), migrates the database and creates the apps' OAuth2 clients. Every other container, more API containers and the worker, needs the same key and the same `OAUTH_PASSPHRASE`: mount the volume read-only at `/app/config/jwt`. Every container checks on every start that the passphrase opens the key and that the two key files belong together, and stops with a message when they do not; start the container with `RUN_MIGRATIONS=1` first.

Run exactly one worker with the same settings and the command `php bin/console messenger:consume scheduler_default --time-limit=3600 --memory-limit=256M`, restarted whenever it exits. Pass that command to the image's own entrypoint, which checks the settings and the key first: as `command:` in Compose, as `args:` (not `command:`, which replaces the entrypoint) on Kubernetes.

The API listens on plain HTTP on port 8080 inside the container and runs as user id 1000; it needs no Linux capability, so all of them can be dropped. Its health check is `GET /health` on that port, which answers 503 while the database or Redis does not answer.

## Updates

The macOS app (through Sparkle), the Windows setup (`.exe`) and the Linux AppImage update themselves. From 0.2.0 on, a `.deb` or `.msi` install does not: update it by installing the new package from the [latest release](https://github.com/getvarve/varve/releases/latest). Version 0.1.7 and older still update such an install in the app, once, to the current release.

## Upgrading from Varve Community

Up to 0.1.7 the app was called Varve Community. It is the same app with the same bundle id, so updates keep coming in, and your connections, queries and activity stay where they were: in `<OS data dir>/Varve`, with secrets in the OS keychain under the service `varve`. Neither depends on the app's name.

- **macOS:** `install.sh` installs `Varve.app` and moves `Varve Community.app` to the Trash. An in-app update keeps the old file name, `Varve Community.app`; the app inside is Varve.
- **Windows:** the installer of Varve removes the Varve Community install with its own uninstaller, without its data, and moves its Start menu and desktop shortcuts to the new name. A taskbar pin keeps its old name but opens Varve.
- **Linux:** the AppImage keeps its path and `varve.desktop`. That entry still shows Varve Community in the app menu after an in-app update; run `install.sh` again to rename it. The `varve` deb package replaces `varve-community`.
