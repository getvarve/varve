# Commit scopes (varve)

Every commit uses Conventional Commits with one of these scopes: `<type>(<scope>): <summary>`.

| Scope | Area |
|---|---|
| `core` | `crates/core`: shared types, values, capabilities, errors |
| `extension` | `crates/extension`: extension API and registry |
| `postgres` | `crates/driver-postgres` |
| `mysql` | `crates/driver-mysql` (MySQL and MariaDB) |
| `redis` | `crates/driver-redis` |
| `catalog` | `crates/catalog`: catalog models, extraction, DDL generators |
| `jobs` | `crates/jobs`: job runner, dump, run SQL file, import, export |
| `live-stats` | `crates/live-stats`: current server status |
| `activity` | `crates/activity`: local activity recorder and redactor |
| `store` | `crates/store`: local `app.db` and migrations |
| `secrets` | `crates/secrets`: OS keychain |
| `tunnel` | `crates/tunnel`: SSH tunnels |
| `app` | `app/src-tauri`: Tauri commands, events, extension loading |
| `ui` | `app/src`: Vue UI and extension slots |
| `docs` | `docs/`, README, contributing |
| `ci` | GitHub workflows |
| `deps` | dependency bumps |
| `repo` | repository setup and tooling that fits no other scope |
