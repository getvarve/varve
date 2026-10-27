//! PostgreSQL editors: typed changes from the UI turned into SQL statements
//! for preview and execution.
//!
//! The UI sends a [`PgChange`] (JSON, tagged by `object` and `action`, field
//! names in camelCase) and [`plan`] returns the [`EditPlan`]: the statements
//! in order, whether they may run in one transaction, whether they destroy
//! data or objects, and warnings about rewrites and locks. Nothing here
//! talks to the server; the app shows the plan and runs it.
//!
//! Names and values are quoted through [`crate::sql`]. Expressions, data
//! types, queries and argument lists are SQL the user wrote. They are read
//! with the driver's lexer ([`varve_driver::lex`], PostgreSQL's lexical
//! rules). Comments between tokens are removed (a `--` comment inside a
//! continued string literal belongs to the literal and stays), and the
//! fragment must have the shape its place needs: one expression (no `;`,
//! balanced brackets, no top-level `,`, parenthesized where something
//! could follow it), a type name, an argument list, one query, or one
//! data-changing statement. So a fragment can never end its statement, add
//! a clause, or hide a second statement.
//!
//! Version-specific syntax is gated on [`ServerInfo::at_least`] and refused
//! with [`crate::CatalogError::Unsupported`] on older servers.

use serde::{Deserialize, Serialize};
use varve_core::Engine;
use varve_driver::ServerInfo;

use crate::model::{CatalogError, CatalogResult, EditPlan};

mod common;
mod database;
mod extension;
mod index;
pub(crate) mod int_text;
mod object;
mod routine;
mod rule;
mod sequence;
mod table;
mod trigger;
mod typename;
mod types;
mod view;

pub use common::{ConfigSetting, DropBehavior, FiringState, QualifiedName, RoleSpec, StorageParam};
pub use database::{
    CreateDatabase, CreateStrategy, DatabaseChange, DatabaseOperation, LocaleProvider,
    SchemaChange, SchemaOperation,
};
pub use extension::{CreateExtension, ExtensionChange};
pub use index::{
    CreateIndex, IndexChange, IndexElement, IndexKey, IndexMethod, IndexOperation, NullsOrder,
    ReindexTarget, SortOrder,
};
pub use object::{CommentChange, OwnerChange};
pub use routine::{
    ArgMode, CreateRoutine, Parallel, ReturnColumn, Returns, RoutineArg, RoutineBody,
    RoutineChange, RoutineKind, RoutineOperation, Volatility,
};
pub use rule::{CreatePolicy, CreateRule, PolicyChange, PolicyCommand, RuleChange, RuleEvent};
pub use sequence::{
    ColumnRef, CreateSequence, SequenceChange, SequenceLimit, SequenceOperation, SequenceOptions,
    SequenceType,
};
pub use table::{
    BoundValue, ColumnDef, ColumnStorage, Compression, ConstraintKind, CreateTable, DetachMode,
    ExclusionElement, Generated, GeneratedStorage, Identity, IdentityGeneration, MatchType,
    PartitionBound, PartitionBy, PartitionKey, PartitionOf, PartitionStrategy, ReferentialAction,
    Restart, TableChange, TableConstraint, TableOperation,
};
pub use trigger::{
    CreateEventTrigger, CreateTrigger, EventTriggerChange, EventTriggerEvent, TriggerChange,
    TriggerEvent, TriggerLevel, TriggerTiming,
};
pub use types::{
    CompositeAttribute, CompositeOperation, CreateDomain, CreateRangeType, DomainChange,
    DomainCheck, DomainOperation, EnumPosition, TypeChange,
};
pub use view::{
    CheckOption, CreateMaterializedView, CreateView, MaterializedViewChange,
    MaterializedViewOperation, ViewChange, ViewOperation, ViewOption,
};

/// A change to a PostgreSQL object, tagged by `object`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(
    tag = "object",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PgChange {
    /// Databases.
    Database(DatabaseChange),
    /// Schemas.
    Schema(SchemaChange),
    /// Tables, partitioned tables and partitions (the table designer).
    Table(TableChange),
    /// Indexes.
    Index(IndexChange),
    /// Views.
    View(ViewChange),
    /// Materialized views.
    MaterializedView(MaterializedViewChange),
    /// Sequences.
    Sequence(SequenceChange),
    /// Functions and procedures.
    Routine(RoutineChange),
    /// Triggers.
    Trigger(TriggerChange),
    /// Event triggers.
    EventTrigger(EventTriggerChange),
    /// Enum, composite and range types.
    Type(TypeChange),
    /// Domains.
    Domain(DomainChange),
    /// Extensions.
    Extension(ExtensionChange),
    /// Rewrite rules.
    Rule(RuleChange),
    /// Row level security policies.
    Policy(PolicyChange),
    /// A comment on any object.
    Comment(CommentChange),
    /// The owner of any owned object.
    Owner(OwnerChange),
}

/// The SQL for a change.
///
/// # Errors
///
/// [`CatalogError::Invalid`] when the change cannot be expressed (missing
/// parts, conflicting options, a fragment that does not have its shape,
/// `REFRESH ... CONCURRENTLY` without a unique index);
/// [`CatalogError::Unsupported`] when the server is not PostgreSQL or its
/// version lacks the syntax.
pub fn plan(server: &ServerInfo, change: &PgChange) -> CatalogResult<EditPlan> {
    if server.engine != Engine::Postgres {
        return Err(CatalogError::Unsupported {
            message: "PostgreSQL editors need a PostgreSQL server".into(),
        });
    }
    match change {
        PgChange::Database(c) => database::plan_database(server, c),
        PgChange::Schema(c) => database::plan_schema(server, c),
        PgChange::Table(c) => table::plan_table(server, c),
        PgChange::Index(c) => index::plan_index(server, c),
        PgChange::View(c) => view::plan_view(server, c),
        PgChange::MaterializedView(c) => view::plan_materialized_view(server, c),
        PgChange::Sequence(c) => sequence::plan_sequence(server, c),
        PgChange::Routine(c) => routine::plan_routine(server, c),
        PgChange::Trigger(c) => trigger::plan_trigger(server, c),
        PgChange::EventTrigger(c) => trigger::plan_event_trigger(server, c),
        PgChange::Type(c) => types::plan_type(server, c),
        PgChange::Domain(c) => types::plan_domain(server, c),
        PgChange::Extension(c) => extension::plan_extension(server, c),
        PgChange::Rule(c) => rule::plan_rule(server, c),
        PgChange::Policy(c) => rule::plan_policy(server, c),
        PgChange::Comment(c) => object::plan_comment(server, c),
        PgChange::Owner(c) => object::plan_owner(server, c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::tests::pg;

    #[test]
    fn changes_round_trip_through_json() -> Result<(), Box<dyn std::error::Error>> {
        let json = r#"{
            "object": "table",
            "action": "alter",
            "name": {"schema": "s", "name": "t"},
            "operations": [
                {"op": "addColumn", "column": {"name": "a", "dataType": "int", "notNull": true}},
                {"op": "addConstraint", "constraint": {"name": "c", "kind": {"type": "check", "expression": "a > 0"}}, "notValid": true},
                {"op": "attachPartition", "partition": {"schema": "s", "name": "p"}, "bound": {"kind": "list", "values": [{"kind": "literal", "value": "x"}]}},
                {"op": "owner", "owner": {"kind": "currentUser"}}
            ]
        }"#;
        let change: PgChange = serde_json::from_str(json)?;
        let again: PgChange = serde_json::from_str(&serde_json::to_string(&change)?)?;
        assert_eq!(change, again);
        let alter = plan(&pg(13), &change)?;
        assert_eq!(
            alter.statements,
            vec![
                r#"ALTER TABLE "s"."t" ADD COLUMN "a" int NOT NULL"#,
                r#"ALTER TABLE "s"."t" ADD CONSTRAINT "c" CHECK (a > 0) NOT VALID"#,
                r#"ALTER TABLE "s"."t" ATTACH PARTITION "s"."p" FOR VALUES IN ('x')"#,
                r#"ALTER TABLE "s"."t" OWNER TO CURRENT_USER"#,
            ]
        );

        let index: PgChange = serde_json::from_str(
            r#"{"object": "index", "action": "create", "table": {"name": "t"},
                "method": {"kind": "gin"}, "elements": [{"key": {"expression": "to_tsvector('simple', b)"}}]}"#,
        )?;
        assert_eq!(
            plan(&pg(13), &index)?.statements,
            vec![r#"CREATE INDEX ON "t" USING gin ((to_tsvector('simple', b)))"#]
        );
        let comment: PgChange = serde_json::from_str(
            r#"{"object": "comment", "target": {"kind": "table", "database": "d", "schema": "s",
                "parent": null, "name": "t", "signature": null}, "comment": "hi"}"#,
        )?;
        assert_eq!(
            plan(&pg(13), &comment)?.statements,
            vec![r#"COMMENT ON TABLE "s"."t" IS 'hi'"#]
        );
        Ok(())
    }

    #[test]
    fn unknown_fields_are_refused() {
        for json in [
            // A misspelled option must not be silently dropped.
            r#"{"object": "schema", "action": "drop", "name": "s", "cascade": true}"#,
            r#"{"object": "table", "action": "alter", "name": {"name": "t"},
                "operations": [{"op": "dropColumn", "name": "a", "force": true}]}"#,
            r#"{"object": "table", "action": "alter", "name": {"name": "t", "db": "x"},
                "operations": [{"op": "rowLevelSecurity", "enabled": true}]}"#,
            r#"{"object": "table", "action": "alter", "name": {"name": "t"},
                "operations": [{"op": "addConstraint", "constraint": {"kind": {"type": "check",
                "expression": "a > 0", "valid": false}}}]}"#,
            r#"{"object": "index", "action": "create", "table": {"name": "t"}, "concurrent": true,
                "elements": [{"key": {"column": "a"}}]}"#,
            r#"{"object": "comment", "target": {"kind": "table", "database": null, "schema": null,
                "parent": null, "name": "t", "signature": null}, "text": "x"}"#,
        ] {
            assert!(serde_json::from_str::<PgChange>(json).is_err(), "{json}");
        }
    }

    #[test]
    fn refuses_other_engines() {
        let mut server = pg(16);
        server.engine = Engine::Mysql;
        let change = PgChange::Schema(SchemaChange::Drop {
            name: "s".into(),
            if_exists: false,
            behavior: DropBehavior::Restrict,
        });
        assert!(matches!(
            plan(&server, &change),
            Err(CatalogError::Unsupported { .. })
        ));
    }
}
