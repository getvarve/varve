//! Grid data editing (design doc 4.2): inserting, updating and deleting
//! rows of a table identified by its primary key, or by a unique key whose
//! columns are all `NOT NULL`, on PostgreSQL, MySQL and MariaDB.
//!
//! 1. [`editable_key`] finds the key and the table's columns (types,
//!    defaults, generated and identity columns). `None` keeps the grid
//!    read only.
//! 2. [`plan_edits`] turns the grid's [`RowChange`]s into SQL, shown to
//!    the user ([`RowEditPlan::script`]).
//! 3. [`apply`] runs the plan in one transaction, checks that every update
//!    and delete hit exactly one row, and returns the rows as stored.

mod apply;
mod expr;
mod key;
mod model;
mod plan;

pub use apply::apply;
pub use key::editable_key;
pub use model::{
    AppliedChange, ApplyOutcome, CellValue, ChangeOp, ColumnInfo, ConflictReason, GeneratedKind,
    IdentityKind, KeyCollation, KeyInfo, KeyKind, PlanOptions, PlannedChange, QualifiedOperator,
    RowChange, RowEditPlan, RowKey, TableTarget,
};
pub use plan::plan_edits;
