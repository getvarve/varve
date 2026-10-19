# Open core

Varve comes in two editions built from the same app:

- **Varve Community** is built from this repository alone. It is free, licensed under Apache-2.0 and fully usable.
- **Varve Pro** is the official build. It is Varve Community plus closed extensions.

## What is in each edition

| Area | Community | Pro |
|---|---|---|
| Connections, SSH, TLS, keychain, folders | ✓ | ✓ |
| PostgreSQL, MySQL, MariaDB and Redis drivers and object editors | ✓ | ✓ |
| Query editor, result grid, data editing, EXPLAIN, DDL view | ✓ | ✓ |
| User and permission management | ✓ | ✓ |
| Dump database as SQL, run SQL file, import and export wizards | ✓ | ✓ |
| Live server status | ✓ | ✓ |
| Local activity log | ✓ | ✓ |
| Statistics history and trend dashboards | | ✓ |
| Wrapped, the yearly recap | | ✓ |
| Structure Sync, Data Sync and Data Transfer | | ✓ |
| Cloud sync across devices | | ✓ |
| Team workspaces | | ✓ |

## How Pro plugs in

The `extension` crate in this repository defines the only way an extension reaches the app. An extension can add Tauri commands, job kinds, database migrations in its own namespace, activity actions, hooks (on connect, on activity, on record change) and capabilities. It can only add, never replace. The Vue app has named slots (menu items, tool routes, connection tabs, settings sections) that extensions fill. With no extensions, the slots render nothing.

Dependencies go one way: Pro depends on this repository, and this repository never references Pro. Community and Pro share the same local data file, so moving between editions keeps your connections, queries and history of actions.

Anyone can write an extension against the same API.
