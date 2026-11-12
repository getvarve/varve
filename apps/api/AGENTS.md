# AGENTS.md

This is a Symfony project. Check `composer.json` for the exact Symfony/PHP version
in use, and read `symfony.lock` to see which recipes ran. Don't assume Doctrine,
Twig, API Platform, Messenger, or Lock are installed unless one of those says so.

## Ask before generating

If the task doesn't specify, ask rather than guess:

- Persistence: Doctrine ORM, Doctrine ODM, or none?
- Interface: server-rendered (Twig), API (Serializer, maybe API Platform), or both?
- Auth: SecurityBundle, and which authenticator?

If you can't ask (no interactive channel), state the assumption you're making and
pick the smallest option (e.g. no persistence layer) rather than scaffolding a
full stack nobody asked for.

## Adding features: Flex, not hand-wiring

Install new capabilities with `composer require <package>` (e.g. `symfony/lock`,
`symfony/messenger`, `orm-pack`) and let the Flex recipe register the bundle and
generate its config. Don't hand-edit `config/bundles.php` or hand-write a bundle's
base config; that's what the recipe is for. Don't skip a good-fit component just
because it isn't installed yet; installing it is one command.

## Conventions

Follow https://symfony.com/doc/current/best_practices.html to write idiomatic
Symfony:

- Use PHP attributes for framework metadata, and not only on controllers:
  `#[Route]`, `#[MapRequestPayload]`, `#[IsGranted]` on actions, `#[Assert\...]`
  on properties, `#[AsCommand]`, `#[AsEventListener]`, `#[AsMessageHandler]`, and
  `#[AsAlias]` / `#[AsTaggedItem]` / `#[Autoconfigure]` on services. No YAML or
  XML routing.
- Rely on autowiring and autoconfiguration. Type-hint constructor arguments and
  let the container resolve them. Where a type-hint can't express it, stay in the
  class with `#[Autowire]` (parameters, env vars, expressions) or `#[Target]` (one
  of several implementations of an interface). A YAML service definition is the
  last resort, not the first.
- Controllers extend `AbstractController`, stay thin, and delegate to services.
- Use the framework for what it already does: Form for server-rendered forms,
  Validator for validation, Serializer for JSON, Messenger for async work,
  Security (voters, authenticators) for access control, Twig `path()`/`url()`
  instead of hardcoded URLs.
- Before hand-writing infrastructure (locks, queues, caches, HTTP clients,
  mailers, schedulers) or reaching for a third-party library, check whether a
  Symfony component covers it. It usually does.

Three specifics worth spelling out, because they are easy to get wrong:

- Bind request data with `#[MapRequestPayload]` / `#[MapQueryString]` on action
  arguments, which wires up Serializer and Validator for you, instead of calling
  `json_decode()` or `SerializerInterface` by hand. If neither package is
  installed yet, `composer require` them rather than falling back to manual
  parsing.
- Use constructor property promotion, and `readonly` for DTOs and value objects.
  Don't mark a service `readonly` if it might become `lazy: true`: a lazy proxy
  can't extend a `readonly` class.
- Use `symfony/lock` (`LockFactory`) for mutual exclusion. A hand-built flag or
  lock file looks fine in review and is usually wrong under concurrency.

## Everyday workflow

The API runs in Docker (PHP 8.5 on FrankenPHP); run every command from the repository root through the `api`
container, never with the host's PHP. See README.md in this folder for starting the stack.

- Commands: `docker compose exec api php bin/console ...`.
- When something fails, read `var/log/dev.log` before changing code. FrankenPHP keeps the app in memory: after
  clearing the cache, restart it with `docker compose restart api`.
- If `maker-bundle` is installed, prefer `bin/console make:*` with every argument
  passed up front and `--no-interaction` where supported: makers prompt on a
  terminal by default, which hangs a non-interactive shell. If a maker still
  needs interactive input, hand-write the code instead.
- Schema changes go through migrations (`bin/console make:migration`, then
  `doctrine:migrations:migrate`), never `doctrine:schema:update` or hand-written SQL.
- `.env` is committed and holds defaults only. Real secrets belong in `.env.local`
  (git-ignored) or the deployment environment, read via `%env(...)%`. Settings of
  this app are prefixed `VARVE_` where they are not a framework or bundle setting.
- Every setting a production server needs goes into `App\Deployment\ProductionSettings`
  (with a test in `tests/Unit/Deployment/ProductionSettingsTest.php`): the production
  image runs `app:settings:check` before anything else and refuses to start on a
  missing or development value.

## Testing

Functional/HTTP tests extend `WebTestCase` (or `App\Tests\Functional\DatabaseTestCase`,
which rolls every test back); service-level tests extend `KernelTestCase`. A
feature isn't done until it has a test that exercises it the way a caller would,
an HTTP request for a controller or a service call for a service, not just "it
didn't throw."

Run one test file at a time, never the whole suite or a whole test directory:
`docker compose exec -e APP_ENV=test api php bin/phpunit tests/Functional/HealthTest.php`.
Static analysis: `docker compose exec api sh -c 'bin/console cache:warmup --env=test && vendor/bin/phpstan analyse'`.

## Code style

Symfony's coding standard, the `@Symfony` php-cs-fixer ruleset (a PSR-12-derived
superset). Comments and messages are plain English; no em or en dashes.

## Discover, don't guess

Framework APIs change between versions and your training data may be stale. Look
things up in the project instead of relying on memory:

- `bin/console about`: versions, environment, paths.
- `bin/console debug:router`, `debug:container`, `debug:autowiring <name>`,
  `debug:config <bundle>`, `config:dump-reference <bundle>`: what exists and how
  it is configured.
- `bin/console lint:container`, plus `lint:twig templates/` and
  `lint:yaml config/` where those packages are installed: validate before running.
- Read the installed source and docblocks under `vendor/`.
- Docs: https://symfony.com/doc/current/ (switch to the version matching
  `composer.json` if it differs).
