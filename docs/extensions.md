# Extensions

Varve has a plugin mechanism: a binary calls `varve_app::run` with its Tauri context and a list of extensions, and the frontend entry passes UI extensions to `createVarveApp`. The Varve app in this repository passes an empty list. Everything an extension adds is placed under its id, so it can add but never replace.

## Backend (`crates/extension`)

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

## Frontend (`@varve/ui`)

```ts
import { createVarveApp } from '@varve/ui/createVarveApp';
import type { UiExtension } from '@varve/ui/extensions';
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

## Data

Extensions share the app's data folder (`<OS data dir>/Varve`) and its `app.db`. Each extension migrates its own namespace, so its tables stay on disk, unused, when the extension is no longer installed.
