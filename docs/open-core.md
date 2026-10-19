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

## How extensions plug in

An edition is a small binary that calls `varve_app::run` with its Tauri context and a list of extensions. Varve Community passes an empty list. Everything an extension adds is placed under its id, so it can add but never replace.

### Backend (`crates/extension`)

```rust
use varve_extension::{ActivityCategory, AppContext, Extension, ExtensionError, Registry};
use varve_store::SchemaMigration;

struct Hello;

impl Extension for Hello {
    fn id(&self) -> &'static str { "acme.hello" }
    fn name(&self) -> &'static str { "Hello" }

    fn register(&self, r: &mut Registry<'_>) -> Result<(), ExtensionError> {
        // Callable from the UI as "acme.hello.greet".
        r.command("greet", |_ctx: AppContext, payload: serde_json::Value| async move {
            Ok(serde_json::json!({ "hello": payload }))
        })?;
        // Tables must start with "acme_hello_"; history is kept per namespace.
        r.migrations(vec![SchemaMigration::new(1, "create_greetings",
            "CREATE TABLE acme_hello_greetings (id INTEGER PRIMARY KEY, at TEXT NOT NULL)")])?;
        r.capability("greet")?;                                  // "acme.hello.greet"
        r.activity_action("greeted", ActivityCategory::Tool)?;   // "acme.hello.greeted"
        r.on_connect(|_ctx, event| tracing::info!(?event, "connected"));
        Ok(())
    }
}
```

Registration happens before any window opens. An extension that breaks a rule (bad id, duplicate name, unprefixed table) stops the app with a clear error, and nothing of it is kept.

The UI calls extension commands through one Tauri command, `ext_invoke`, by full name. Extensions never add Tauri commands of their own.

### Frontend (`@varve/ui`)

```ts
import { createVarveApp } from '@varve/ui';
import type { UiExtension } from '@varve/ui';
import GreetButton from './GreetButton.vue';

const hello: UiExtension = {
  id: 'acme.hello',
  setup(ctx) {
    ctx.addSlotItem('tools', { id: 'acme.hello.greet-button', component: GreetButton });
    ctx.addRoute({ path: '/x/acme.hello/history', component: GreetButton });
    // ctx.invoke('greet', payload) calls "acme.hello.greet".
  },
};

createVarveApp({ extensions: [hello] }).mount('#app');
```

Slots: `title-bar`, `main-menu`, `tools`, `sidebar-sections`, `connection-tabs`, `settings-sections`. With no extensions, every slot renders nothing.

### Rules

- Dependencies go one way: Pro depends on this repository, and this repository never references Pro. CI enforces this with `scripts/check-no-pro.mjs`.
- Community and Pro share the same local data folder (`<OS data dir>/Varve`), so moving between editions keeps your connections, queries and activity. Pro tables stay on disk, unused, when you go back to Community.
- Anyone can write an extension against the same API.
