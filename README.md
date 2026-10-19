<p align="center">
  <img src="assets/logo/varve-icon.svg" width="88" height="88" alt="Varve logo">
</p>

<h1 align="center">Varve</h1>

<p align="center">
  A calm desktop manager for PostgreSQL, MySQL, MariaDB and Redis.
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-5B4BE0"></a>
  <img alt="Status: design phase" src="https://img.shields.io/badge/status-design%20phase-4A5160">
  <img alt="Platforms: macOS, Windows, Linux" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Windows%20%7C%20Linux-4A5160">
</p>

---

## Why Varve

- **Every engine, fully.** PostgreSQL, MySQL, MariaDB and Redis, each with its own objects: materialized views, partitions, events, system-versioned tables, streams.
- **Safe around production.** Environment tags, read-only connections and confirmation before destructive statements. Passwords stay in your OS keychain.
- **Quiet by design.** A dense, calm interface where color only means something: production is red, and nothing else is.

## Features

| | |
|---|---|
| **Query** | Dialect-aware editor, multiple result sets, cancel, history, visual EXPLAIN |
| **Data** | Fast grid with inline editing, filters and a record view |
| **Structure** | Editors for tables, indexes, constraints, partitions, views, functions, sequences, triggers, events, types and extensions |
| **Redis** | Key browser, value editors for every type, streams, console, pub/sub, slow log |
| **Users** | Roles and privilege matrices, row-level security, MySQL accounts, Redis ACLs |
| **Files** | Dump a database as SQL, run SQL files, import and export CSV, JSON, Excel, Parquet |
| **Insight** | Live server status and a local activity log |

## Editions

| | Community | Pro |
|---|---|---|
| Everything above | ✓ | ✓ |
| Statistics history and trends | | ✓ |
| Structure Sync, Data Sync, Data Transfer | | ✓ |
| Cloud sync and team workspaces | | ✓ |
| Wrapped, your year in databases | | ✓ |
| License | Apache-2.0 | Commercial |

Varve Community is this repository: free, open source and complete on its own. Pro adds closed extensions through a public extension API. See [docs/open-core.md](docs/open-core.md).

## Status

Varve is in its design phase; there is no code yet. Next up:

1. **M0** Foundations: workspace, Tauri shell, extension API, CI
2. **M1** Connections and query editor
3. **M2** Object browser and editors

## Built with

[Tauri 2](https://tauri.app) · Rust · [Vue 3](https://vuejs.org) · TypeScript · SQLite

## Contributing

Contributions are welcome once the code lands. Read [CONTRIBUTING.md](CONTRIBUTING.md) first; every commit needs a `Signed-off-by` line.

## License

[Apache-2.0](LICENSE). The Varve name and logo are trademarks and are not covered by the license; see [TRADEMARKS.md](TRADEMARKS.md).
