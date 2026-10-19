//! The Varve extension API.
//!
//! This crate is the only way closed or third-party code reaches the app
//! (design doc section 2.3). An [`Extension`] receives a [`Registry`] scoped to
//! its own id and can **add**:
//!
//! - commands, callable from the UI as `<extension id>.<name>`
//! - one migration set in its own `app.db` namespace
//! - capabilities the UI can query (`<extension id>.<name>`)
//! - activity actions for the activity recorder
//! - job kinds for the job runner
//! - hooks: on connect, on activity, on record change
//!
//! Everything an extension registers lives under its id, so it cannot replace
//! or shadow anything owned by the app or by another extension.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use varve_core::{DottedId, Edition, Engine};
use varve_store::{CORE_NAMESPACE, MigrationSet, SchemaMigration, Store};

/// A boxed, sendable future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// An extension that plugs into the app.
pub trait Extension: Send + Sync {
    /// Unique dotted id, for example `pro.stats`. Must not be `core` and must
    /// not start with `core.`.
    fn id(&self) -> &'static str;

    /// Human readable name for the About screen.
    fn name(&self) -> &'static str;

    /// Registers everything the extension adds.
    ///
    /// # Errors
    /// Return any [`ExtensionError`] from the registry unchanged; the app
    /// refuses to start with an extension that fails to register.
    fn register(&self, registry: &mut Registry<'_>) -> Result<(), ExtensionError>;
}

/// Errors while registering or calling extensions.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    /// An id did not follow the dotted id rules.
    #[error(transparent)]
    InvalidId(#[from] varve_core::Error),
    /// The id `core` (or anything below it) belongs to the app.
    #[error("extension id `{0}` is reserved for the app")]
    ReservedId(String),
    /// Two extensions used the same id, or one id is nested in another.
    #[error("extension id `{0}` conflicts with an extension that is already registered")]
    DuplicateExtension(String),
    /// A name was registered twice.
    #[error("`{0}` is already registered")]
    Duplicate(String),
    /// The extension registered migrations twice.
    #[error("extension `{0}` already registered its migrations; register all of them in one call")]
    MigrationsAlreadyRegistered(String),
    /// The store rejected the migration set.
    #[error(transparent)]
    Store(#[from] varve_store::StoreError),
}

/// Error returned by a command, sent to the UI as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct CommandError {
    /// Stable machine readable code, for example `not_found`.
    pub code: String,
    /// Message for the user.
    pub message: String,
}

impl CommandError {
    /// Creates an error.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The command does not exist.
    #[must_use]
    pub fn unknown_command(name: &str) -> Self {
        Self::new(
            "unknown_command",
            format!("No command named `{name}` is registered."),
        )
    }

    /// The payload did not match what the command expects.
    #[must_use]
    pub fn invalid_payload(detail: impl std::fmt::Display) -> Self {
        Self::new(
            "invalid_payload",
            format!("The command payload is invalid: {detail}"),
        )
    }

    /// Something failed inside the command.
    #[must_use]
    pub fn internal(detail: impl std::fmt::Display) -> Self {
        Self::new("internal", detail.to_string())
    }
}

/// Result of a command.
pub type CommandResult = Result<Value, CommandError>;

/// What a command, job or hook can use from the app.
#[derive(Clone)]
pub struct AppContext {
    /// The local `app.db`.
    pub store: Arc<Store>,
    /// The running edition.
    pub edition: Edition,
}

impl std::fmt::Debug for AppContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppContext")
            .field("edition", &self.edition)
            .finish_non_exhaustive()
    }
}

/// A command handler.
pub trait CommandHandler: Send + Sync {
    /// Runs the command with its JSON payload.
    fn call(&self, ctx: AppContext, payload: Value) -> BoxFuture<CommandResult>;
}

impl<F, Fut> CommandHandler for F
where
    F: Fn(AppContext, Value) -> Fut + Send + Sync,
    Fut: Future<Output = CommandResult> + Send + 'static,
{
    fn call(&self, ctx: AppContext, payload: Value) -> BoxFuture<CommandResult> {
        Box::pin(self(ctx, payload))
    }
}

/// Category of an activity action (design doc section 9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityCategory {
    /// Connecting and disconnecting.
    Connection,
    /// Query execution.
    Query,
    /// Data edits, imports, exports, dumps.
    Data,
    /// Schema changes.
    Schema,
    /// Long running tools and jobs.
    Tool,
    /// Users, roles and grants.
    Security,
    /// Account and sign-in.
    Account,
    /// Cloud sync.
    Sync,
    /// Workspace management.
    Workspace,
    /// Synced records.
    Record,
    /// The app itself.
    App,
}

/// Progress reported by a running job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobProgress {
    /// Fraction done, from 0.0 to 1.0, when known.
    pub fraction: Option<f64>,
    /// Short status line.
    pub message: String,
}

/// Context passed to a running job.
#[derive(Clone)]
pub struct JobContext {
    /// App services.
    pub app: AppContext,
    /// Reports progress to the UI.
    pub progress: Arc<dyn Fn(JobProgress) + Send + Sync>,
}

impl std::fmt::Debug for JobContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobContext")
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}

/// A kind of long running job, such as a data transfer.
pub trait JobKind: Send + Sync {
    /// Short snake_case name, unique within the extension.
    fn name(&self) -> &'static str;

    /// Runs one job with its parameters and returns its summary.
    fn run(&self, ctx: JobContext, params: Value) -> BoxFuture<Result<Value, CommandError>>;
}

/// A connection was opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectEvent {
    /// Connection profile id.
    pub connection_id: String,
    /// Engine of the connection.
    pub engine: Engine,
}

/// An activity event was recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    /// Action, for example `connection.opened`.
    pub action: String,
    /// Action-specific details, never secrets.
    pub details: Value,
}

/// How a synced record changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// The record was created.
    Created,
    /// The record was updated.
    Updated,
    /// The record was deleted.
    Deleted,
}

/// A local record (connection, folder, saved query, job profile) changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordChange {
    /// Record type, for example `connection`.
    pub record_type: String,
    /// Record id.
    pub record_id: String,
    /// What happened.
    pub kind: ChangeKind,
}

/// A hook callback. Hooks run on the app's async runtime; they must return
/// quickly and spawn their own work for anything slow.
pub type Hook<E> = Arc<dyn Fn(&AppContext, &E) + Send + Sync>;

/// Public description of a registered extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionInfo {
    /// Extension id.
    pub id: DottedId,
    /// Display name.
    pub name: String,
}

/// Everything collected from all extensions, before the app starts.
#[derive(Default)]
pub struct HostBuilder {
    extensions: Vec<ExtensionInfo>,
    commands: BTreeMap<DottedId, Arc<dyn CommandHandler>>,
    migrations: Vec<MigrationSet>,
    capabilities: BTreeSet<DottedId>,
    activity_actions: BTreeMap<DottedId, ActivityCategory>,
    job_kinds: BTreeMap<DottedId, Arc<dyn JobKind>>,
    on_connect: Vec<Hook<ConnectEvent>>,
    on_activity: Vec<Hook<ActivityEvent>>,
    on_record_change: Vec<Hook<RecordChange>>,
}

impl HostBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one extension.
    ///
    /// # Errors
    /// Fails when the id is invalid, reserved, already used, or when the
    /// extension's own registration fails. On error nothing of this extension
    /// is kept.
    pub fn add(&mut self, extension: &dyn Extension) -> Result<(), ExtensionError> {
        let id = DottedId::new(extension.id())?;
        if id.as_str() == CORE_NAMESPACE || id.as_str().starts_with("core.") {
            return Err(ExtensionError::ReservedId(id.to_string()));
        }
        if self
            .extensions
            .iter()
            .any(|e| e.id.is_within(&id) || id.is_within(&e.id))
        {
            return Err(ExtensionError::DuplicateExtension(id.to_string()));
        }

        let mut staged = HostBuilder::default();
        let mut registry = Registry {
            id: id.clone(),
            staged: &mut staged,
            existing: self,
        };
        extension.register(&mut registry)?;

        self.extensions.push(ExtensionInfo {
            id,
            name: extension.name().to_string(),
        });
        self.commands.extend(staged.commands);
        self.migrations.extend(staged.migrations);
        self.capabilities.extend(staged.capabilities);
        self.activity_actions.extend(staged.activity_actions);
        self.job_kinds.extend(staged.job_kinds);
        self.on_connect.extend(staged.on_connect);
        self.on_activity.extend(staged.on_activity);
        self.on_record_change.extend(staged.on_record_change);
        Ok(())
    }

    /// Finishes registration.
    #[must_use]
    pub fn build(self) -> ExtensionHost {
        ExtensionHost {
            inner: Arc::new(self),
        }
    }
}

/// The registry an extension sees during [`Extension::register`]. Every name
/// is placed under the extension id.
pub struct Registry<'a> {
    id: DottedId,
    staged: &'a mut HostBuilder,
    existing: &'a HostBuilder,
}

impl Registry<'_> {
    /// The id of the extension being registered.
    #[must_use]
    pub fn extension_id(&self) -> &DottedId {
        &self.id
    }

    fn scoped(&self, name: &str) -> Result<DottedId, ExtensionError> {
        Ok(DottedId::new(format!("{}.{name}", self.id))?)
    }

    /// Adds a command callable as `<extension id>.<name>`.
    ///
    /// # Errors
    /// Fails when the name is invalid or already registered.
    pub fn command(
        &mut self,
        name: &str,
        handler: impl CommandHandler + 'static,
    ) -> Result<(), ExtensionError> {
        let full = self.scoped(name)?;
        if self.staged.commands.contains_key(&full) || self.existing.commands.contains_key(&full) {
            return Err(ExtensionError::Duplicate(full.to_string()));
        }
        self.staged.commands.insert(full, Arc::new(handler));
        Ok(())
    }

    /// Adds the extension's migrations, in the namespace equal to its id.
    /// Call once with every migration.
    ///
    /// # Errors
    /// Fails when called twice or when the set breaks the namespacing rules
    /// of [`MigrationSet::validate`].
    pub fn migrations(&mut self, migrations: Vec<SchemaMigration>) -> Result<(), ExtensionError> {
        if self
            .staged
            .migrations
            .iter()
            .any(|m| m.namespace() == &self.id)
        {
            return Err(ExtensionError::MigrationsAlreadyRegistered(
                self.id.to_string(),
            ));
        }
        let set = MigrationSet::new(self.id.clone(), migrations);
        set.validate()?;
        self.staged.migrations.push(set);
        Ok(())
    }

    /// Adds a capability `<extension id>.<name>` the UI can check.
    ///
    /// # Errors
    /// Fails when the name is invalid or already registered.
    pub fn capability(&mut self, name: &str) -> Result<(), ExtensionError> {
        let full = self.scoped(name)?;
        if !self.staged.capabilities.insert(full.clone())
            || self.existing.capabilities.contains(&full)
        {
            return Err(ExtensionError::Duplicate(full.to_string()));
        }
        Ok(())
    }

    /// Adds an activity action `<extension id>.<name>` in a category.
    ///
    /// # Errors
    /// Fails when the name is invalid or already registered.
    pub fn activity_action(
        &mut self,
        name: &str,
        category: ActivityCategory,
    ) -> Result<(), ExtensionError> {
        let full = self.scoped(name)?;
        if self.staged.activity_actions.contains_key(&full)
            || self.existing.activity_actions.contains_key(&full)
        {
            return Err(ExtensionError::Duplicate(full.to_string()));
        }
        self.staged.activity_actions.insert(full, category);
        Ok(())
    }

    /// Adds a job kind `<extension id>.<kind name>`.
    ///
    /// # Errors
    /// Fails when the name is invalid or already registered.
    pub fn job_kind(&mut self, kind: impl JobKind + 'static) -> Result<(), ExtensionError> {
        let full = self.scoped(kind.name())?;
        if self.staged.job_kinds.contains_key(&full) || self.existing.job_kinds.contains_key(&full)
        {
            return Err(ExtensionError::Duplicate(full.to_string()));
        }
        self.staged.job_kinds.insert(full, Arc::new(kind));
        Ok(())
    }

    /// Runs `hook` every time a connection opens.
    pub fn on_connect(
        &mut self,
        hook: impl Fn(&AppContext, &ConnectEvent) + Send + Sync + 'static,
    ) {
        self.staged.on_connect.push(Arc::new(hook));
    }

    /// Runs `hook` every time an activity event is recorded.
    pub fn on_activity(
        &mut self,
        hook: impl Fn(&AppContext, &ActivityEvent) + Send + Sync + 'static,
    ) {
        self.staged.on_activity.push(Arc::new(hook));
    }

    /// Runs `hook` every time a local record changes.
    pub fn on_record_change(
        &mut self,
        hook: impl Fn(&AppContext, &RecordChange) + Send + Sync + 'static,
    ) {
        self.staged.on_record_change.push(Arc::new(hook));
    }
}

/// The registered extensions, ready for the running app. Cheap to clone.
#[derive(Clone)]
pub struct ExtensionHost {
    inner: Arc<HostBuilder>,
}

impl std::fmt::Debug for ExtensionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionHost")
            .field("extensions", &self.inner.extensions)
            .finish_non_exhaustive()
    }
}

impl ExtensionHost {
    /// A host with no extensions (Varve Community).
    #[must_use]
    pub fn empty() -> Self {
        HostBuilder::new().build()
    }

    /// Registered extensions, in registration order.
    #[must_use]
    pub fn extensions(&self) -> &[ExtensionInfo] {
        &self.inner.extensions
    }

    /// All capabilities registered by extensions.
    #[must_use]
    pub fn capabilities(&self) -> Vec<String> {
        self.inner
            .capabilities
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Names of all extension commands.
    #[must_use]
    pub fn command_names(&self) -> Vec<String> {
        self.inner
            .commands
            .keys()
            .map(ToString::to_string)
            .collect()
    }

    /// Names of all job kinds.
    #[must_use]
    pub fn job_kind_names(&self) -> Vec<String> {
        self.inner
            .job_kinds
            .keys()
            .map(ToString::to_string)
            .collect()
    }

    /// Category of an extension activity action, if registered.
    #[must_use]
    pub fn activity_category(&self, action: &str) -> Option<ActivityCategory> {
        let id = DottedId::new(action).ok()?;
        self.inner.activity_actions.get(&id).copied()
    }

    /// Migration sets of all extensions.
    #[must_use]
    pub fn migration_sets(&self) -> &[MigrationSet] {
        &self.inner.migrations
    }

    /// Calls an extension command by its full name.
    pub fn invoke(&self, ctx: AppContext, name: &str, payload: Value) -> BoxFuture<CommandResult> {
        let handler = DottedId::new(name)
            .ok()
            .and_then(|id| self.inner.commands.get(&id).cloned());
        match handler {
            Some(handler) => handler.call(ctx, payload),
            None => {
                let error = CommandError::unknown_command(name);
                Box::pin(async move { Err(error) })
            }
        }
    }

    /// Runs a job of the given kind.
    pub fn run_job(
        &self,
        ctx: JobContext,
        kind: &str,
        params: Value,
    ) -> BoxFuture<Result<Value, CommandError>> {
        let job = DottedId::new(kind)
            .ok()
            .and_then(|id| self.inner.job_kinds.get(&id).cloned());
        match job {
            Some(job) => job.run(ctx, params),
            None => {
                let error = CommandError::new(
                    "unknown_job_kind",
                    format!("No job kind named `{kind}` is registered."),
                );
                Box::pin(async move { Err(error) })
            }
        }
    }

    /// Notifies extensions that a connection opened.
    pub fn emit_connect(&self, ctx: &AppContext, event: &ConnectEvent) {
        for hook in &self.inner.on_connect {
            hook(ctx, event);
        }
    }

    /// Notifies extensions of an activity event.
    pub fn emit_activity(&self, ctx: &AppContext, event: &ActivityEvent) {
        for hook in &self.inner.on_activity {
            hook(ctx, event);
        }
    }

    /// Notifies extensions that a local record changed.
    pub fn emit_record_change(&self, ctx: &AppContext, event: &RecordChange) {
        for hook in &self.inner.on_record_change {
            hook(ctx, event);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use varve_store::core_migrations;

    use super::*;

    struct Sample {
        id: &'static str,
        connects: Arc<AtomicUsize>,
    }

    impl Extension for Sample {
        fn id(&self) -> &'static str {
            self.id
        }

        fn name(&self) -> &'static str {
            "Sample"
        }

        fn register(&self, registry: &mut Registry<'_>) -> Result<(), ExtensionError> {
            registry.command("echo", |_ctx: AppContext, payload: Value| async move {
                Ok(payload)
            })?;
            registry.capability("echo")?;
            registry.activity_action("echoed", ActivityCategory::Tool)?;
            let prefix = registry.extension_id().to_sql_prefix();
            registry.migrations(vec![SchemaMigration::new(
                1,
                "create_items",
                format!("CREATE TABLE {prefix}_items (id INTEGER PRIMARY KEY)"),
            )])?;
            let connects = Arc::clone(&self.connects);
            registry.on_connect(move |_, _| {
                connects.fetch_add(1, Ordering::SeqCst);
            });
            Ok(())
        }
    }

    fn sample(id: &'static str) -> Sample {
        Sample {
            id,
            connects: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn ctx() -> Result<AppContext, ExtensionError> {
        Ok(AppContext {
            store: Arc::new(Store::open_in_memory()?),
            edition: Edition::Pro,
        })
    }

    #[tokio::test]
    async fn commands_are_namespaced_and_callable() -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = HostBuilder::new();
        builder.add(&sample("pro.sample"))?;
        let host = builder.build();
        assert_eq!(host.command_names(), vec!["pro.sample.echo".to_string()]);
        let out = host
            .invoke(ctx()?, "pro.sample.echo", json!({"a": 1}))
            .await?;
        assert_eq!(out, json!({"a": 1}));
        let missing = host.invoke(ctx()?, "pro.sample.nope", Value::Null).await;
        assert_eq!(
            missing.err().map(|e| e.code),
            Some("unknown_command".to_string())
        );
        Ok(())
    }

    #[test]
    fn registers_capabilities_actions_and_migrations() -> Result<(), Box<dyn std::error::Error>> {
        let mut builder = HostBuilder::new();
        builder.add(&sample("pro.sample"))?;
        let host = builder.build();
        assert_eq!(host.capabilities(), vec!["pro.sample.echo".to_string()]);
        assert_eq!(
            host.activity_category("pro.sample.echoed"),
            Some(ActivityCategory::Tool)
        );
        let store = Store::open_in_memory()?;
        let mut sets = vec![core_migrations()];
        sets.extend(host.migration_sets().iter().cloned());
        store.migrate(&sets)?;
        Ok(())
    }

    #[test]
    fn reserved_and_conflicting_ids_are_rejected() {
        let mut builder = HostBuilder::new();
        assert!(matches!(
            builder.add(&sample("core")),
            Err(ExtensionError::ReservedId(_))
        ));
        assert!(matches!(
            builder.add(&sample("core.x")),
            Err(ExtensionError::ReservedId(_))
        ));
        assert!(matches!(
            builder.add(&sample("Bad")),
            Err(ExtensionError::InvalidId(_))
        ));
        assert!(builder.add(&sample("pro.sample")).is_ok());
        assert!(matches!(
            builder.add(&sample("pro.sample")),
            Err(ExtensionError::DuplicateExtension(_))
        ));
        assert!(matches!(
            builder.add(&sample("pro.sample.inner")),
            Err(ExtensionError::DuplicateExtension(_))
        ));
        assert!(matches!(
            builder.add(&sample("pro")),
            Err(ExtensionError::DuplicateExtension(_))
        ));
    }

    #[test]
    fn failed_registration_keeps_nothing() {
        struct Broken;
        impl Extension for Broken {
            fn id(&self) -> &'static str {
                "pro.broken"
            }
            fn name(&self) -> &'static str {
                "Broken"
            }
            fn register(&self, registry: &mut Registry<'_>) -> Result<(), ExtensionError> {
                registry.capability("ok")?;
                registry.capability("ok")?;
                Ok(())
            }
        }
        let mut builder = HostBuilder::new();
        assert!(matches!(
            builder.add(&Broken),
            Err(ExtensionError::Duplicate(_))
        ));
        let host = builder.build();
        assert!(host.capabilities().is_empty());
        assert!(host.extensions().is_empty());
    }

    #[test]
    fn hooks_fire() -> Result<(), Box<dyn std::error::Error>> {
        let ext = sample("pro.sample");
        let counter = Arc::clone(&ext.connects);
        let mut builder = HostBuilder::new();
        builder.add(&ext)?;
        let host = builder.build();
        host.emit_connect(
            &ctx()?,
            &ConnectEvent {
                connection_id: "c1".into(),
                engine: Engine::Postgres,
            },
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn unprefixed_extension_tables_are_rejected_at_registration() {
        struct BadTables;
        impl Extension for BadTables {
            fn id(&self) -> &'static str {
                "pro.bad"
            }
            fn name(&self) -> &'static str {
                "Bad tables"
            }
            fn register(&self, registry: &mut Registry<'_>) -> Result<(), ExtensionError> {
                registry.migrations(vec![SchemaMigration::new(
                    1,
                    "x",
                    "CREATE TABLE items (id INTEGER)",
                )])
            }
        }
        let mut builder = HostBuilder::new();
        assert!(matches!(
            builder.add(&BadTables),
            Err(ExtensionError::Store(_))
        ));
    }
}
