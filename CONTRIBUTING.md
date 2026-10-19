# Contributing to Varve

Thank you for helping. The project is in its design phase; code arrives with milestone M0. This file describes how contributions will work.

## Developer Certificate of Origin

Every commit must carry a sign-off that certifies you wrote the change or have the right to submit it under the project license ([developercertificate.org](https://developercertificate.org/)):

```text
Signed-off-by: Your Name <you@example.com>
```

Add it automatically with `git commit -s`. Pull requests with unsigned commits cannot be merged.

Contributions are licensed under Apache-2.0, the same license as the project. There is no separate contributor license agreement.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) with a scope from [.agents/commit-scopes.md](.agents/commit-scopes.md):

```text
fix(postgres): cancel running query when the tab closes
```

## Scope of this repository

This repository is Varve Community. Some features live in closed Pro extensions (see [docs/open-core.md](docs/open-core.md)). Changes here must not depend on, reference or feature-flag closed code. New extension points in the `extension` crate are welcome when they are generic and useful to any extension.

## Safety rules for changes

- Never log, store or send passwords, passphrases, keys or tokens. Secrets go through the OS keychain only.
- Destructive actions on connections tagged as production must ask for confirmation.
- Add tests against the database version matrix in `dev/docker-compose.yml` for driver changes.
