# Hosting

## The app is free and complete

Varve is one app. Every feature is in this repository, under AGPL-3.0, and works without an account or a server. There is no paid edition.

## Cloud features need a backend

Cloud sync across devices and team workspaces are planned, not built yet. They need a server to sync through, so they will only work once you connect the app to a Varve backend. Everything else keeps working without one.

The self-hostable backend (planned) lives in this repository, under the same license. Two ways to use it are planned:

| | Self-hosted | Varve Cloud |
| --- | --- | --- |
| Who runs the server | You | getvarve |
| Price | Free | Paid |
| Billing | None | Per organization, by seat |

- **Self-hosted** has no billing at all. The billing code is part of the same public code, and it is off by default.
- **Varve Cloud** is the managed hosting by getvarve. An organization pays for its seats; an account without a paid organization cannot use it.

What a backend stores, and how, is decided when it is built; this page will say so then.

## Updates

The macOS app (through Sparkle), the Windows setup (`.exe`) and the Linux AppImage update themselves. A `.deb` or `.msi` install does not: update it by installing the new package from the [latest release](https://github.com/getvarve/varve/releases/latest).

## Upgrading from Varve Community

Up to 0.1.7 the app was called Varve Community. It is the same app with the same bundle id, so updates keep coming in, and your connections, queries and activity stay where they were: in `<OS data dir>/Varve`, with secrets in the OS keychain under the service `varve`. Neither depends on the app's name.

- **macOS:** `install.sh` installs `Varve.app` and moves `Varve Community.app` to the Trash. An in-app update keeps the old file name, `Varve Community.app`; the app inside is Varve.
- **Windows:** the installer of Varve removes the Varve Community install with its own uninstaller, without its data, and moves its shortcuts to the new name.
- **Linux:** the AppImage keeps its path and `varve.desktop`; only the name in the app menu changes. The `varve` deb package replaces `varve-community`.
