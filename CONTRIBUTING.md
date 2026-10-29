# Contributing to Varve

Thank you for helping. The project is in its design phase; code arrives with milestone M0. This file describes how contributions will work.

## Developer Certificate of Origin

Every commit must carry a sign-off that certifies you wrote the change or have the right to submit it under the project license ([developercertificate.org](https://developercertificate.org/)):

```text
Signed-off-by: Your Name <you@example.com>
```

Add it automatically with `git commit -s`. Pull requests with unsigned commits cannot be merged.

Contributions are licensed under AGPL-3.0-only, the same license as the project. There is no separate contributor license agreement.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) with a scope from [.agents/commit-scopes.md](.agents/commit-scopes.md):

```text
fix(postgres): cancel running query when the tab closes
```

## Scope of this repository

This repository is the whole Varve app: every feature lands here, and nothing in it depends on closed code. The self-hostable backend for cloud sync and teams (planned) lives in this repository too (see [docs/hosting.md](docs/hosting.md)). New extension points in the `extension` crate are welcome when they are generic and useful to any extension (see [docs/extensions.md](docs/extensions.md)).

## Safety rules for changes

- Never log, store or send passwords, passphrases, keys or tokens. Secrets go through the OS keychain only.
- Destructive actions on connections tagged as production must ask for confirmation.
- Add tests against the database version matrix in `dev/docker-compose.yml` for driver changes.
