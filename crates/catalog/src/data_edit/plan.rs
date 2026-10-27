//! Turns grid changes into SQL ([`plan_edits`]).

use std::collections::{BTreeMap, BTreeSet};

use varve_core::Engine;
use varve_driver::{Dialect, ServerInfo, Value};

use super::expr::check_expression;
use super::model::{
    CellValue, ChangeOp, ColumnInfo, IdentityKind, KeyInfo, PlanOptions, PlannedChange,
    QualifiedOperator, RowChange, RowEditPlan, RowKey, TableTarget,
};
use crate::model::{CatalogError, CatalogResult};
use crate::sql;

/// The SQL dialect of an engine that supports grid editing.
pub(crate) fn dialect_of(engine: Engine) -> CatalogResult<Dialect> {
    match engine {
        Engine::Postgres => Ok(Dialect::Postgres),
        Engine::Mysql | Engine::Mariadb => Ok(Dialect::Mysql),
        Engine::Redis => Err(CatalogError::Unsupported {
            message: "Redis values are edited with the key browser".into(),
        }),
    }
}

/// The quoted table name: `"schema"."table"` (PostgreSQL) or
/// `` `database`.`table` `` (MySQL, MariaDB).
pub(crate) fn table_name(dialect: Dialect, target: &TableTarget) -> CatalogResult<String> {
    match dialect {
        Dialect::Postgres => {
            let schema = target
                .schema
                .as_deref()
                .ok_or_else(|| invalid("a PostgreSQL table needs its schema"))?;
            sql::qualified(dialect, &[Some(schema), Some(&target.table)])
        }
        Dialect::Mysql => {
            if target.schema.is_some() {
                return Err(invalid(
                    "MySQL and MariaDB tables are named by database and table; leave the schema \
                     empty",
                ));
            }
            sql::qualified(dialect, &[Some(&target.database), Some(&target.table)])
        }
    }
}

fn invalid(message: impl Into<String>) -> CatalogError {
    CatalogError::Invalid {
        message: message.into(),
    }
}

/// Builds the SQL for grid changes: one statement per change, in order.
///
/// - Every name and value is quoted through [`crate::sql`]; raw SQL appears
///   only for [`CellValue::Expression`], after a check that it cannot reach
///   outside its parentheses.
/// - `UPDATE` and `DELETE` find the row by its key (`WHERE` on every key
///   column, which must match `options.key`); PostgreSQL compares each key
///   column with the equality operator of the key's index, named with its
///   schema (`OPERATOR(pg_catalog.=)`, [`KeyInfo::equality_operators`]),
///   so nothing in the user's `search_path` can stand in for it. With
///   [`PlanOptions::check_original`] they also require the original values
///   (the changed columns of an update, every listed column of a delete),
///   compared `NULL`-safely: PostgreSQL compares the text forms of the
///   column and of the literal cast to the column's type in the `C`
///   collation with `OPERATOR(pg_catalog.=)` (`IS NULL` for an original
///   `NULL`; exact for every type, including `json`, `real` and geometric
///   types that have no or a loose `=`); MySQL and MariaDB use
///   `<=>`, with text compared under a binary no-pad collation (so a change
///   in case or trailing spaces is seen), `FLOAT` literals cast to `FLOAT`
///   and MySQL `JSON` literals cast to `JSON`.
/// - Inserts return the stored row: `INSERT ... RETURNING *` on PostgreSQL
///   and MariaDB 10.5+, otherwise a `SELECT` by the inserted key values or
///   `LAST_INSERT_ID()` for the auto-increment column.
/// - Updates on PostgreSQL use `RETURNING *`. On MySQL and MariaDB, which
///   report changed rather than matched rows, the row is first locked with
///   `SELECT ... FOR UPDATE` (which must return exactly one row) and read
///   back after the update.
/// - PostgreSQL literals are cast to the column's type by its own name
///   (`CAST('...' AS "pg_catalog"."uuid")`), without a type modifier, so
///   nothing is truncated or rounded by the cast itself.
///
/// # Errors
///
/// [`CatalogError::Unsupported`] for Redis. [`CatalogError::Invalid`] for a
/// change that cannot be expressed: an unknown or repeated column, a key
/// that does not match `options.key` or holds `NULL`, an update without
/// changed columns, a write to a generated or `GENERATED ALWAYS` identity
/// column (other than `DEFAULT`), a missing original value with
/// `check_original`, a missing PostgreSQL column type, an invalid
/// expression, or a value the dialect cannot express.
pub fn plan_edits(
    server: &ServerInfo,
    target: &TableTarget,
    changes: &[RowChange],
    options: &PlanOptions,
) -> CatalogResult<RowEditPlan> {
    let dialect = dialect_of(server.engine)?;
    let mut planner = Planner {
        engine: server.engine,
        dialect,
        table: table_name(dialect, target)?,
        key: &options.key,
        check_original: options.check_original,
        insert_returning: match server.engine {
            Engine::Postgres => true,
            Engine::Mariadb => server.at_least(10, 5),
            Engine::Mysql | Engine::Redis => false,
        },
        float_cast: match server.engine {
            // `CAST(... AS FLOAT)` exists since MySQL 8.0.17 and MariaDB
            // 10.4.5.
            Engine::Mysql => server.at_least(8, 1) || server.patch.is_none_or(|p| p >= 17),
            _ => true,
        },
        warnings: Vec::new(),
        has_expressions: false,
    };
    planner.check_key()?;
    let mut planned = Vec::with_capacity(changes.len());
    for (index, change) in changes.iter().enumerate() {
        planned.push(match change {
            RowChange::Insert { values } => planner.insert(index, values)?,
            RowChange::Update { key, original, set } => {
                planner.update(index, key, original, set)?
            }
            RowChange::Delete { key, original } => planner.delete(index, key, original)?,
        });
    }
    Ok(RowEditPlan {
        engine: server.engine,
        changes: planned,
        warnings: planner.warnings,
        has_expressions: planner.has_expressions,
    })
}

struct Planner<'a> {
    engine: Engine,
    dialect: Dialect,
    table: String,
    key: &'a KeyInfo,
    check_original: bool,
    insert_returning: bool,
    float_cast: bool,
    warnings: Vec<String>,
    has_expressions: bool,
}

impl<'a> Planner<'a> {
    fn check_key(&self) -> CatalogResult<()> {
        if self.key.columns.is_empty() {
            return Err(invalid("the key has no columns"));
        }
        let mut seen = BTreeSet::new();
        for name in &self.key.columns {
            let column = self.column(name)?;
            if !seen.insert(column.name.as_str()) {
                return Err(invalid(format!("key column {name} is listed twice")));
            }
        }
        if self.dialect == Dialect::Postgres {
            if self.key.equality_operators.len() != self.key.columns.len() {
                return Err(invalid(
                    "the key has no equality operator for every column; take the key from \
                     editable_key",
                ));
            }
            for operator in &self.key.equality_operators {
                pg_operator(operator)?;
            }
        }
        Ok(())
    }

    fn column(&self, name: &str) -> CatalogResult<&'a ColumnInfo> {
        let key: &'a KeyInfo = self.key;
        key.column(name, self.dialect == Dialect::Mysql)
            .ok_or_else(|| invalid(format!("the table has no column {name}")))
    }

    fn ident(&self, column: &ColumnInfo) -> CatalogResult<String> {
        sql::ident(self.dialect, &column.name)
    }

    fn insert(
        &mut self,
        index: usize,
        values: &[(String, CellValue)],
    ) -> CatalogResult<PlannedChange> {
        let mut seen = BTreeSet::new();
        let mut names = Vec::with_capacity(values.len());
        let mut cells = Vec::with_capacity(values.len());
        let mut given: BTreeMap<String, &CellValue> = BTreeMap::new();
        for (name, cell) in values {
            let column = self.column(name)?;
            if !seen.insert(column.name.clone()) {
                return Err(invalid(format!("column {name} is set twice")));
            }
            check_writable(column, cell)?;
            names.push(self.ident(column)?);
            cells.push(self.cell(column, cell)?);
            given.insert(column.name.clone(), cell);
        }
        let mut statement = if names.is_empty() {
            match self.dialect {
                Dialect::Postgres => format!("INSERT INTO {} DEFAULT VALUES", self.table),
                Dialect::Mysql => format!("INSERT INTO {} () VALUES ()", self.table),
            }
        } else {
            format!(
                "INSERT INTO {} ({}) VALUES ({})",
                self.table,
                names.join(", "),
                cells.join(", ")
            )
        };
        let refresh = if self.insert_returning {
            statement.push_str(" RETURNING *");
            None
        } else {
            self.inserted_row_query(index, &given)?
        };
        Ok(PlannedChange {
            change: index,
            op: ChangeOp::Insert,
            lock: None,
            statement,
            returns_row: self.insert_returning,
            refresh,
            checks_original: false,
        })
    }

    /// MySQL: the `SELECT` that finds an inserted row again, by the key
    /// values given or `LAST_INSERT_ID()` for the auto-increment column.
    fn inserted_row_query(
        &mut self,
        index: usize,
        given: &BTreeMap<String, &CellValue>,
    ) -> CatalogResult<Option<String>> {
        let mut conditions = Vec::with_capacity(self.key.columns.len());
        for name in &self.key.columns {
            let column = self.column(name)?;
            let ident = self.ident(column)?;
            let cell = given.get(&column.name).copied();
            let value = match cell {
                Some(CellValue::Value(value)) if !value_is_null(value) => {
                    if column.auto_increment && is_zero(value) {
                        // Zero asks for a generated value unless the
                        // NO_AUTO_VALUE_ON_ZERO SQL mode is on.
                        "IF(FIND_IN_SET('NO_AUTO_VALUE_ON_ZERO', @@SESSION.sql_mode) > 0, 0, \
                         LAST_INSERT_ID())"
                            .to_owned()
                    } else {
                        self.literal(column, value)?
                    }
                }
                None | Some(CellValue::Default | CellValue::Value(_)) if column.auto_increment => {
                    "LAST_INSERT_ID()".to_owned()
                }
                _ => {
                    self.warnings.push(format!(
                        "change {}: the new row cannot be read back because the server sets key \
                         column {}; reload the grid to see it",
                        index + 1,
                        column.name
                    ));
                    return Ok(None);
                }
            };
            conditions.push(format!("{ident} = {value}"));
        }
        Ok(Some(format!(
            "SELECT * FROM {} WHERE {}",
            self.table,
            conditions.join(" AND ")
        )))
    }

    fn update(
        &mut self,
        index: usize,
        key: &RowKey,
        original: &[(String, Value)],
        set: &[(String, CellValue)],
    ) -> CatalogResult<PlannedChange> {
        if set.is_empty() {
            return Err(invalid(format!(
                "change {}: an update needs at least one changed column",
                index + 1
            )));
        }
        let key_values = self.key_values(key)?;
        let originals = self.originals(original)?;
        let mut conditions = self.key_conditions(&key_values)?;
        let mut seen = BTreeSet::new();
        let mut assignments = Vec::with_capacity(set.len());
        let mut new_key = key_values.clone();
        let mut key_unknown = None;
        for (name, cell) in set {
            let column = self.column(name)?;
            if !seen.insert(column.name.clone()) {
                return Err(invalid(format!("column {name} is set twice")));
            }
            check_writable(column, cell)?;
            assignments.push(format!(
                "{} = {}",
                self.ident(column)?,
                self.cell(column, cell)?
            ));
            if self.check_original {
                let value = originals.get(&column.name).ok_or_else(|| {
                    invalid(format!(
                        "change {}: the original value of {} is needed to check that nobody \
                         else changed it",
                        index + 1,
                        column.name
                    ))
                })?;
                if let Some(condition) = self.compare(index, column, value)? {
                    conditions.push(condition);
                }
            }
            if let Some(slot) = new_key.get_mut(&column.name) {
                match cell {
                    CellValue::Value(value) if !value_is_null(value) => *slot = value.clone(),
                    _ => key_unknown = Some(column.name.clone()),
                }
            }
        }
        let filter = conditions.join(" AND ");
        let set_sql = assignments.join(", ");
        Ok(match self.dialect {
            Dialect::Postgres => PlannedChange {
                change: index,
                op: ChangeOp::Update,
                lock: None,
                statement: format!(
                    "UPDATE {} SET {set_sql} WHERE {filter} RETURNING *",
                    self.table
                ),
                returns_row: true,
                refresh: None,
                checks_original: self.check_original,
            },
            Dialect::Mysql => {
                let key_columns = self
                    .key
                    .columns
                    .iter()
                    .map(|name| self.column(name).and_then(|c| self.ident(c)))
                    .collect::<CatalogResult<Vec<_>>>()?
                    .join(", ");
                let refresh = match key_unknown {
                    Some(column) => {
                        self.warnings.push(format!(
                            "change {}: the updated row cannot be read back because key column \
                             {column} is set by the server; reload the grid to see it",
                            index + 1
                        ));
                        None
                    }
                    None => Some(format!(
                        "SELECT * FROM {} WHERE {}",
                        self.table,
                        self.key_conditions(&new_key)?.join(" AND ")
                    )),
                };
                PlannedChange {
                    change: index,
                    op: ChangeOp::Update,
                    lock: Some(format!(
                        "SELECT {key_columns} FROM {} WHERE {filter} LIMIT 2 FOR UPDATE",
                        self.table
                    )),
                    statement: format!("UPDATE {} SET {set_sql} WHERE {filter}", self.table),
                    returns_row: false,
                    refresh,
                    checks_original: self.check_original,
                }
            }
        })
    }

    fn delete(
        &mut self,
        index: usize,
        key: &RowKey,
        original: &[(String, Value)],
    ) -> CatalogResult<PlannedChange> {
        let key_values = self.key_values(key)?;
        let mut conditions = self.key_conditions(&key_values)?;
        if self.check_original {
            // Refuses columns given twice, as for updates.
            self.originals(original)?;
            for (name, value) in original {
                let column = self.column(name)?;
                if let Some(condition) = self.compare(index, column, value)? {
                    conditions.push(condition);
                }
            }
        }
        Ok(PlannedChange {
            change: index,
            op: ChangeOp::Delete,
            lock: None,
            statement: format!(
                "DELETE FROM {} WHERE {}",
                self.table,
                conditions.join(" AND ")
            ),
            returns_row: false,
            refresh: None,
            checks_original: self.check_original,
        })
    }

    /// The key values by column name; they must cover exactly the key
    /// columns and none may be `NULL`.
    fn key_values(&self, key: &RowKey) -> CatalogResult<BTreeMap<String, Value>> {
        let mut values = BTreeMap::new();
        for (name, value) in &key.columns {
            let column = self.column(name)?;
            if !self.key.columns.contains(&column.name) {
                return Err(invalid(format!(
                    "{name} is not a column of the key {}",
                    self.key.name
                )));
            }
            if value_is_null(value) {
                return Err(invalid(format!("key column {name} cannot be NULL")));
            }
            if values.insert(column.name.clone(), value.clone()).is_some() {
                return Err(invalid(format!("key column {name} is given twice")));
            }
        }
        if let Some(missing) = self.key.columns.iter().find(|k| !values.contains_key(*k)) {
            return Err(invalid(format!("the row key has no value for {missing}")));
        }
        Ok(values)
    }

    /// `column = literal` for every key column, in key order. PostgreSQL
    /// writes the key index's own equality operator with its schema
    /// (`OPERATOR(pg_catalog.=)`), so an operator with the same name in
    /// the user's `search_path` cannot stand in for it.
    fn key_conditions(&self, values: &BTreeMap<String, Value>) -> CatalogResult<Vec<String>> {
        self.key
            .columns
            .iter()
            .enumerate()
            .map(|(position, name)| {
                let column = self.column(name)?;
                let value = values
                    .get(&column.name)
                    .ok_or_else(|| invalid(format!("the row key has no value for {name}")))?;
                let operator = match self.dialect {
                    Dialect::Postgres => {
                        let operator =
                            self.key.equality_operators.get(position).ok_or_else(|| {
                                invalid(format!("the key has no equality operator for {name}"))
                            })?;
                        pg_operator(operator)?
                    }
                    Dialect::Mysql => "=".to_owned(),
                };
                let left = match self.dialect {
                    Dialect::Postgres => {
                        let mut left = pg_comparable(column, &self.ident(column)?);
                        if let Some(Some(collation)) = self.key.key_collations.get(position) {
                            left = format!(
                                "{left} COLLATE {}",
                                sql::qualified(
                                    self.dialect,
                                    &[Some(&collation.schema), Some(&collation.name)]
                                )?
                            );
                        }
                        left
                    }
                    Dialect::Mysql => self.ident(column)?,
                };
                Ok(format!(
                    "{left} {operator} {}",
                    self.literal(column, value)?
                ))
            })
            .collect()
    }

    fn originals(&self, original: &[(String, Value)]) -> CatalogResult<BTreeMap<String, Value>> {
        let mut values = BTreeMap::new();
        for (name, value) in original {
            let column = self.column(name)?;
            if values.insert(column.name.clone(), value.clone()).is_some() {
                return Err(invalid(format!(
                    "the original value of {name} is given twice"
                )));
            }
        }
        Ok(values)
    }

    /// The `NULL`-safe condition that the column still holds `value`.
    /// `None` when the server cannot compare the column exactly (`FLOAT`
    /// before MySQL 8.0.17) or Varve cannot write its value (a PostgreSQL
    /// array of a domain over an array), with a warning.
    fn compare(
        &mut self,
        index: usize,
        column: &ColumnInfo,
        value: &Value,
    ) -> CatalogResult<Option<String>> {
        let ident = self.ident(column)?;
        if column.array_of_arrays && !value_is_null(value) {
            // No literal can be written for it (see `literal`): the rest of
            // the row is still checked.
            self.warnings.push(format!(
                "change {}: {} is not compared with its original value because Varve cannot \
                 write a value of an array of a domain over an array",
                index + 1,
                column.name
            ));
            return Ok(None);
        }
        match self.dialect {
            // `IS NOT DISTINCT FROM` would look `=` up by name, so the
            // NULL case is written out and the comparison names
            // `pg_catalog`'s text equality.
            Dialect::Postgres => Ok(Some(if value_is_null(value) {
                format!("CAST({ident} AS pg_catalog.text) IS NULL")
            } else if is_xml(column) {
                // The client got the server's xml output (which drops an
                // XML declaration that says nothing beyond the default,
                // and keeps what follows it): compare with exactly that.
                format!(
                    "(pg_catalog.textin(pg_catalog.xml_out({ident})) COLLATE pg_catalog.\"C\" \
                     OPERATOR(pg_catalog.=) {}) IS TRUE",
                    sql::literal(self.dialect, value)?
                )
            } else {
                let left = format!("CAST({} AS pg_catalog.text)", pg_comparable(column, &ident));
                let right = format!("CAST({} AS pg_catalog.text)", self.literal(column, value)?);
                format!("({left} COLLATE pg_catalog.\"C\" OPERATOR(pg_catalog.=) {right}) IS TRUE")
            })),
            Dialect::Mysql => {
                if column.type_name == "float" && !self.float_cast && !value_is_null(value) {
                    self.warnings.push(format!(
                        "change {}: {} is not compared with its original value because this \
                         MySQL version cannot write a FLOAT literal",
                        index + 1,
                        column.name
                    ));
                    return Ok(None);
                }
                let right = match value {
                    Value::Text(_) if is_mysql_text(&column.type_name) => {
                        let collation = match self.engine {
                            Engine::Mariadb => "utf8mb4_nopad_bin",
                            _ => "utf8mb4_0900_bin",
                        };
                        format!(
                            "CONVERT({} USING utf8mb4) COLLATE {collation}",
                            self.literal(column, value)?
                        )
                    }
                    _ => self.literal(column, value)?,
                };
                Ok(Some(format!("{ident} <=> {right}")))
            }
        }
    }

    fn cell(&mut self, column: &ColumnInfo, cell: &CellValue) -> CatalogResult<String> {
        match cell {
            CellValue::Value(value) => self.literal(column, value),
            CellValue::Default => Ok("DEFAULT".to_owned()),
            CellValue::Expression(text) => {
                check_expression(self.dialect, text).map_err(|e| match e {
                    CatalogError::Invalid { message } => {
                        invalid(format!("column {}: {message}", column.name))
                    }
                    other => other,
                })?;
                self.has_expressions = true;
                Ok(format!("({text})"))
            }
        }
    }

    /// The literal for a value written to or compared with `column`.
    fn literal(&self, column: &ColumnInfo, value: &Value) -> CatalogResult<String> {
        if value_is_null(value) {
            return Ok("NULL".to_owned());
        }
        match self.dialect {
            Dialect::Postgres => {
                if column.array_of_arrays {
                    return Err(invalid(format!(
                        "column {} is an array of a domain over an array; Varve cannot \
                         write or compare its values, so this change has to be made in SQL",
                        column.name
                    )));
                }
                let schema = column.type_schema.as_deref().ok_or_else(|| {
                    invalid(format!(
                        "the type of column {} is not known; take the columns from \
                         editable_key",
                        column.name
                    ))
                })?;
                let ty = sql::qualified(self.dialect, &[Some(schema), Some(&column.type_name)])?;
                Ok(format!(
                    "CAST({} AS {ty})",
                    sql::literal(self.dialect, value)?
                ))
            }
            Dialect::Mysql => {
                let literal = mysql_literal(value)?;
                Ok(match column.type_name.as_str() {
                    "float" if self.float_cast => format!("CAST({literal} AS FLOAT)"),
                    "json" if self.engine == Engine::Mysql => format!("CAST({literal} AS JSON)"),
                    _ => literal,
                })
            }
        }
    }
}

/// A PostgreSQL column as compared with a value read by the client: an
/// array (also a domain over an array) with lower bounds other than 1
/// (`'[0:1]={5,6}'`) loses its bounds on the way to the client, so arrays
/// are compared as the slice `col[:]`, whose lower bounds are always 1.
fn pg_comparable(column: &ColumnInfo, ident: &str) -> String {
    if column.array || column.data_type.ends_with("[]") {
        format!("({ident})[:]")
    } else {
        ident.to_owned()
    }
}

/// True for an `xml` column or a domain over `xml`.
fn is_xml(column: &ColumnInfo) -> bool {
    let xml = |schema: &str, name: &str| schema == "pg_catalog" && name == "xml";
    column
        .type_schema
        .as_deref()
        .is_some_and(|schema| xml(schema, &column.type_name))
        || column
            .base_type
            .as_ref()
            .is_some_and(|(schema, name)| xml(schema, name))
}

/// `OPERATOR(schema.name)` for a PostgreSQL operator. The name is checked
/// against the characters operators are made of (it is not an identifier
/// and cannot be quoted); the schema is quoted, except `pg_catalog`.
fn pg_operator(operator: &QualifiedOperator) -> CatalogResult<String> {
    const ALLOWED: &str = "+-*/<>=~!@#%^&|`?";
    let name = &operator.name;
    if name.is_empty()
        || name.len() > 63
        || !name.chars().all(|c| ALLOWED.contains(c))
        || name.contains("--")
        || name.contains("/*")
    {
        return Err(invalid(format!("{name:?} is not an operator name")));
    }
    let schema = if operator.schema == "pg_catalog" {
        "pg_catalog".to_owned()
    } else {
        sql::ident(Dialect::Postgres, &operator.schema)?
    };
    Ok(format!("OPERATOR({schema}.{name})"))
}

/// MySQL and MariaDB literals. Temporal values use a space between date
/// and time; `TIMESTAMP` values in UTC drop their zone because Varve
/// sessions run in UTC (MariaDB accepts no zone in a literal at all).
fn mysql_literal(value: &Value) -> CatalogResult<String> {
    match value {
        Value::Timestamp(text) => sql::string(Dialect::Mysql, &text.replacen('T', " ", 1)),
        Value::TimestampTz(text) => {
            let local = text
                .strip_suffix('Z')
                .or_else(|| text.strip_suffix("+00:00"))
                .ok_or_else(|| {
                    invalid(format!(
                        "the timestamp {text} must be in UTC for MySQL and MariaDB"
                    ))
                })?;
            sql::string(Dialect::Mysql, &local.replacen('T', " ", 1))
        }
        other => sql::literal(Dialect::Mysql, other),
    }
}

fn is_mysql_text(type_name: &str) -> bool {
    matches!(
        type_name,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext"
    )
}

fn value_is_null(value: &Value) -> bool {
    matches!(value, Value::Null)
}

fn is_zero(value: &Value) -> bool {
    match value {
        Value::Int(i) => *i == 0,
        Value::UInt(u) => *u == 0,
        Value::Decimal(s) | Value::Text(s) => s.trim().parse::<i64>() == Ok(0),
        _ => false,
    }
}

/// Refuses writes the server would reject or ignore: generated columns
/// and `GENERATED ALWAYS` identity columns take only `DEFAULT`.
fn check_writable(column: &ColumnInfo, cell: &CellValue) -> CatalogResult<()> {
    if matches!(cell, CellValue::Default) {
        return Ok(());
    }
    if column.generated.is_some() {
        return Err(invalid(format!(
            "{} is a generated column; the server computes it",
            column.name
        )));
    }
    if column.identity == Some(IdentityKind::Always) {
        return Err(invalid(format!(
            "{} is GENERATED ALWAYS AS IDENTITY; the server assigns it",
            column.name
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use varve_driver::JsonText;

    use super::*;
    use crate::data_edit::model::{GeneratedKind, KeyKind};

    fn server(engine: Engine, major: u32, minor: u32, patch: Option<u32>) -> ServerInfo {
        ServerInfo {
            engine,
            version: format!("{major}.{minor}"),
            major,
            minor,
            patch,
        }
    }

    fn pg() -> ServerInfo {
        server(Engine::Postgres, 18, 0, None)
    }

    fn mysql() -> ServerInfo {
        server(Engine::Mysql, 9, 7, Some(2))
    }

    fn mariadb() -> ServerInfo {
        server(Engine::Mariadb, 12, 3, Some(3))
    }

    fn pg_column(name: &str, type_name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: type_name.into(),
            type_schema: Some("pg_catalog".into()),
            type_name: type_name.into(),
            base_type: None,
            array: false,
            array_of_arrays: false,
            nullable: true,
            has_default: false,
            generated: None,
            identity: None,
            auto_increment: false,
            editable: true,
        }
    }

    fn my_column(name: &str, type_name: &str) -> ColumnInfo {
        ColumnInfo {
            type_schema: None,
            ..pg_column(name, type_name)
        }
    }

    fn pg_key() -> KeyInfo {
        let mut id = pg_column("id", "int8");
        id.identity = Some(IdentityKind::ByDefault);
        id.has_default = true;
        id.nullable = false;
        let mut total = pg_column("total", "int4");
        total.generated = Some(GeneratedKind::Stored);
        total.editable = false;
        let mut mood = pg_column("mood", "mood");
        mood.type_schema = Some("app".into());
        KeyInfo {
            kind: KeyKind::Primary,
            name: "items_pkey".into(),
            columns: vec!["id".into()],
            table_columns: vec![
                id,
                pg_column("name", "text"),
                pg_column("doc", "json"),
                pg_column("tags", "_text"),
                pg_column("weight", "float4"),
                mood,
                total,
            ],
            equality_operators: vec![QualifiedOperator {
                schema: "pg_catalog".into(),
                name: "=".into(),
            }],
            key_collations: Vec::new(),
        }
    }

    fn my_key(engine_json: &str) -> KeyInfo {
        let mut id = my_column("id", "bigint");
        id.auto_increment = true;
        id.has_default = true;
        id.nullable = false;
        let mut total = my_column("total", "int");
        total.generated = Some(GeneratedKind::Virtual);
        total.editable = false;
        KeyInfo {
            kind: KeyKind::Primary,
            name: "PRIMARY".into(),
            columns: vec!["id".into()],
            table_columns: vec![
                id,
                my_column("name", "varchar"),
                my_column("doc", engine_json),
                my_column("weight", "float"),
                my_column("at", "timestamp"),
                total,
            ],
            equality_operators: Vec::new(),
            key_collations: Vec::new(),
        }
    }

    fn pg_target() -> TableTarget {
        TableTarget {
            database: "varve".into(),
            schema: Some("app".into()),
            table: "items".into(),
        }
    }

    fn my_target() -> TableTarget {
        TableTarget {
            database: "shop".into(),
            schema: None,
            table: "items".into(),
        }
    }

    fn options(key: KeyInfo, check_original: bool) -> PlanOptions {
        PlanOptions {
            check_original,
            key,
        }
    }

    fn key(id: i64) -> RowKey {
        RowKey {
            columns: vec![("id".into(), Value::Int(id))],
        }
    }

    fn value(v: Value) -> CellValue {
        CellValue::Value(v)
    }

    fn only(plan: &RowEditPlan) -> CatalogResult<&PlannedChange> {
        plan.changes
            .first()
            .ok_or_else(|| invalid("the plan has no change"))
    }

    #[test]
    fn postgres_insert_returns_the_row_and_casts_literals() -> CatalogResult<()> {
        let plan = plan_edits(
            &pg(),
            &pg_target(),
            &[RowChange::Insert {
                values: vec![
                    ("name".into(), value(Value::Text("it's".into()))),
                    ("doc".into(), value(Value::json(r#"{"b":1,"a":2}"#))),
                    (
                        "tags".into(),
                        value(Value::Array(vec![Value::Text("x".into()), Value::Null])),
                    ),
                    ("mood".into(), value(Value::Text("happy".into()))),
                    ("weight".into(), value(Value::Null)),
                    ("total".into(), CellValue::Default),
                ],
            }],
            &options(pg_key(), false),
        )?;
        let change = only(&plan)?;
        assert_eq!(
            change.statement,
            "INSERT INTO \"app\".\"items\" (\"name\", \"doc\", \"tags\", \"mood\", \"weight\", \
             \"total\") VALUES (CAST('it''s' AS \"pg_catalog\".\"text\"), \
             CAST('{\"b\":1,\"a\":2}' AS \"pg_catalog\".\"json\"), \
             CAST('{\"x\",NULL}' AS \"pg_catalog\".\"_text\"), \
             CAST('happy' AS \"app\".\"mood\"), NULL, DEFAULT) RETURNING *"
        );
        assert!(change.returns_row);
        assert_eq!(change.refresh, None);
        assert_eq!(change.op, ChangeOp::Insert);
        assert!(!plan.has_expressions);
        Ok(())
    }

    #[test]
    fn postgres_empty_insert_uses_default_values() -> CatalogResult<()> {
        let plan = plan_edits(
            &pg(),
            &pg_target(),
            &[RowChange::Insert { values: vec![] }],
            &options(pg_key(), false),
        )?;
        assert_eq!(
            only(&plan)?.statement,
            "INSERT INTO \"app\".\"items\" DEFAULT VALUES RETURNING *"
        );
        Ok(())
    }

    #[test]
    fn postgres_update_with_and_without_original_checks() -> CatalogResult<()> {
        let change = RowChange::Update {
            key: key(7),
            original: vec![
                ("name".into(), Value::Null),
                ("weight".into(), Value::Float(1.1)),
            ],
            set: vec![
                ("name".into(), value(Value::Text("new".into()))),
                ("weight".into(), CellValue::Expression("2 * 3".into())),
            ],
        };
        let plain = plan_edits(
            &pg(),
            &pg_target(),
            std::slice::from_ref(&change),
            &options(pg_key(), false),
        )?;
        assert_eq!(
            only(&plain)?.statement,
            "UPDATE \"app\".\"items\" SET \"name\" = CAST('new' AS \"pg_catalog\".\"text\"), \
             \"weight\" = (2 * 3) WHERE \"id\" OPERATOR(pg_catalog.=) CAST(7 AS \"pg_catalog\".\"int8\") \
             RETURNING *"
        );
        assert!(plain.has_expressions);
        assert!(!only(&plain)?.checks_original);

        let checked = plan_edits(&pg(), &pg_target(), &[change], &options(pg_key(), true))?;
        assert_eq!(
            only(&checked)?.statement,
            "UPDATE \"app\".\"items\" SET \"name\" = CAST('new' AS \"pg_catalog\".\"text\"), \
             \"weight\" = (2 * 3) WHERE \"id\" OPERATOR(pg_catalog.=) \
             CAST(7 AS \"pg_catalog\".\"int8\") \
             AND CAST(\"name\" AS pg_catalog.text) IS NULL \
             AND (CAST(\"weight\" AS pg_catalog.text) COLLATE pg_catalog.\"C\" \
             OPERATOR(pg_catalog.=) \
             CAST(CAST(1.1 AS \"pg_catalog\".\"float4\") AS pg_catalog.text)) IS TRUE RETURNING *"
        );
        assert!(only(&checked)?.checks_original);
        assert!(only(&checked)?.returns_row);
        assert_eq!(only(&checked)?.lock, None);
        Ok(())
    }

    #[test]
    fn postgres_delete_compares_every_original() -> CatalogResult<()> {
        let change = RowChange::Delete {
            key: key(3),
            original: vec![
                ("id".into(), Value::Int(3)),
                ("doc".into(), Value::json("[1]")),
            ],
        };
        let plain = plan_edits(
            &pg(),
            &pg_target(),
            std::slice::from_ref(&change),
            &options(pg_key(), false),
        )?;
        assert_eq!(
            only(&plain)?.statement,
            "DELETE FROM \"app\".\"items\" WHERE \"id\" OPERATOR(pg_catalog.=) \
             CAST(3 AS \"pg_catalog\".\"int8\")"
        );
        let checked = plan_edits(&pg(), &pg_target(), &[change], &options(pg_key(), true))?;
        assert_eq!(
            only(&checked)?.statement,
            "DELETE FROM \"app\".\"items\" WHERE \"id\" OPERATOR(pg_catalog.=) \
             CAST(3 AS \"pg_catalog\".\"int8\") \
             AND (CAST(\"id\" AS pg_catalog.text) COLLATE pg_catalog.\"C\" \
             OPERATOR(pg_catalog.=) \
             CAST(CAST(3 AS \"pg_catalog\".\"int8\") AS pg_catalog.text)) IS TRUE \
             AND (CAST(\"doc\" AS pg_catalog.text) COLLATE pg_catalog.\"C\" \
             OPERATOR(pg_catalog.=) \
             CAST(CAST('[1]' AS \"pg_catalog\".\"json\") AS pg_catalog.text)) IS TRUE"
        );
        assert!(!only(&checked)?.returns_row);
        Ok(())
    }

    #[test]
    fn postgres_keys_use_the_index_operator_with_its_schema() -> CatalogResult<()> {
        let mut key_info = pg_key();
        let mut code = pg_column("code", "code");
        code.type_schema = Some("app".into());
        code.nullable = false;
        key_info.table_columns.push(code);
        key_info.columns = vec!["code".into()];
        key_info.equality_operators = vec![QualifiedOperator {
            schema: "app".into(),
            name: "==".into(),
        }];
        let plan = plan_edits(
            &pg(),
            &pg_target(),
            &[RowChange::Delete {
                key: RowKey {
                    columns: vec![("code".into(), Value::Text("A1".into()))],
                },
                original: Vec::new(),
            }],
            &options(key_info.clone(), false),
        )?;
        assert_eq!(
            only(&plan)?.statement,
            "DELETE FROM \"app\".\"items\" WHERE \"code\" OPERATOR(\"app\".==) \
             CAST('A1' AS \"app\".\"code\")"
        );
        for name in ["", "=;", "=--", "/*", "a"] {
            let mut bad = key_info.clone();
            bad.equality_operators[0].name = name.into();
            assert!(
                plan_edits(&pg(), &pg_target(), &[], &options(bad, false)).is_err(),
                "{name:?}"
            );
        }
        let mut missing = key_info;
        missing.equality_operators.clear();
        assert!(plan_edits(&pg(), &pg_target(), &[], &options(missing, false)).is_err());
        Ok(())
    }

    #[test]
    fn postgres_typed_literals() -> CatalogResult<()> {
        let mut key_info = pg_key();
        key_info.table_columns.extend([
            pg_column("raw", "bytea"),
            pg_column("uid", "uuid"),
            pg_column("span", "interval"),
            pg_column("at", "timestamptz"),
            pg_column("code", "bpchar"),
        ]);
        let plan = plan_edits(
            &pg(),
            &pg_target(),
            &[RowChange::Insert {
                values: vec![
                    ("raw".into(), value(Value::Bytes(vec![0, 0xff]))),
                    (
                        "uid".into(),
                        value(Value::Uuid("6f1c2b8e-0000-4000-8000-000000000001".into())),
                    ),
                    ("span".into(), value(Value::Interval("P1Y2M3DT4H".into()))),
                    (
                        "at".into(),
                        value(Value::TimestampTz("2026-10-19T11:45:00.5Z".into())),
                    ),
                    ("code".into(), value(Value::Text("ab ".into()))),
                ],
            }],
            &options(key_info, false),
        )?;
        assert_eq!(
            only(&plan)?.statement,
            "INSERT INTO \"app\".\"items\" (\"raw\", \"uid\", \"span\", \"at\", \"code\") VALUES \
             (CAST('\\x00ff'::pg_catalog.bytea AS \"pg_catalog\".\"bytea\"), \
             CAST('6f1c2b8e-0000-4000-8000-000000000001' AS \"pg_catalog\".\"uuid\"), \
             CAST('P1Y2M3DT4H' AS \"pg_catalog\".\"interval\"), \
             CAST('2026-10-19T11:45:00.5Z' AS \"pg_catalog\".\"timestamptz\"), \
             CAST('ab ' AS \"pg_catalog\".\"bpchar\")) RETURNING *"
        );
        Ok(())
    }

    #[test]
    fn mysql_insert_reads_back_by_last_insert_id_or_key() -> CatalogResult<()> {
        let plan = plan_edits(
            &mysql(),
            &my_target(),
            &[
                RowChange::Insert {
                    values: vec![("name".into(), value(Value::Text("a".into())))],
                },
                RowChange::Insert {
                    values: vec![("id".into(), value(Value::Int(42)))],
                },
                RowChange::Insert {
                    values: vec![("id".into(), value(Value::Int(0)))],
                },
                RowChange::Insert { values: vec![] },
            ],
            &options(my_key("json"), false),
        )?;
        let [first, second, third, fourth] = plan.changes.as_slice() else {
            return Err(invalid("expected four changes"));
        };
        assert_eq!(
            first.statement,
            "INSERT INTO `shop`.`items` (`name`) VALUES ('a')"
        );
        assert!(!first.returns_row);
        assert_eq!(
            first.refresh.as_deref(),
            Some("SELECT * FROM `shop`.`items` WHERE `id` = LAST_INSERT_ID()")
        );
        assert_eq!(
            second.refresh.as_deref(),
            Some("SELECT * FROM `shop`.`items` WHERE `id` = 42")
        );
        assert_eq!(
            third.refresh.as_deref(),
            Some(
                "SELECT * FROM `shop`.`items` WHERE `id` = IF(FIND_IN_SET('NO_AUTO_VALUE_ON_ZERO', \
                 @@SESSION.sql_mode) > 0, 0, LAST_INSERT_ID())"
            )
        );
        assert_eq!(fourth.statement, "INSERT INTO `shop`.`items` () VALUES ()");
        assert!(plan.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn mysql_insert_that_cannot_be_read_back_warns() -> CatalogResult<()> {
        let mut key_info = my_key("json");
        key_info.columns = vec!["name".into()];
        let plan = plan_edits(
            &mysql(),
            &my_target(),
            &[RowChange::Insert {
                values: vec![("name".into(), CellValue::Expression("UUID()".into()))],
            }],
            &options(key_info, false),
        )?;
        assert_eq!(only(&plan)?.refresh, None);
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.has_expressions);
        Ok(())
    }

    #[test]
    fn mariadb_insert_returning_depends_on_version() -> CatalogResult<()> {
        let change = RowChange::Insert {
            values: vec![("name".into(), value(Value::Text("a".into())))],
        };
        let new = plan_edits(
            &mariadb(),
            &my_target(),
            std::slice::from_ref(&change),
            &options(my_key("longtext"), false),
        )?;
        assert_eq!(
            only(&new)?.statement,
            "INSERT INTO `shop`.`items` (`name`) VALUES ('a') RETURNING *"
        );
        assert!(only(&new)?.returns_row);
        let old = plan_edits(
            &server(Engine::Mariadb, 10, 4, Some(30)),
            &my_target(),
            &[change],
            &options(my_key("longtext"), false),
        )?;
        assert_eq!(
            only(&old)?.statement,
            "INSERT INTO `shop`.`items` (`name`) VALUES ('a')"
        );
        assert!(only(&old)?.refresh.is_some());
        Ok(())
    }

    #[test]
    fn mysql_update_locks_the_row_and_reads_it_back() -> CatalogResult<()> {
        let change = RowChange::Update {
            key: key(5),
            original: vec![
                ("name".into(), Value::Text("Ab".into())),
                ("weight".into(), Value::Float(1.1)),
                ("doc".into(), Value::Json(json(r#"{"a":1}"#)?)),
                ("at".into(), Value::Null),
            ],
            set: vec![
                ("name".into(), value(Value::Text("ab".into()))),
                ("weight".into(), value(Value::Float(2.5))),
                ("doc".into(), value(Value::Json(json("[]")?))),
                (
                    "at".into(),
                    value(Value::TimestampTz("2026-10-19T11:45:00Z".into())),
                ),
            ],
        };
        let plan = plan_edits(
            &mysql(),
            &my_target(),
            &[change],
            &options(my_key("json"), true),
        )?;
        let change = only(&plan)?;
        let filter = "`id` = 5 AND `name` <=> CONVERT('Ab' USING utf8mb4) COLLATE \
                      utf8mb4_0900_bin AND `weight` <=> CAST(1.1 AS FLOAT) AND `doc` <=> \
                      CAST('{\"a\":1}' AS JSON) AND `at` <=> NULL";
        assert_eq!(
            change.lock.as_deref(),
            Some(
                format!("SELECT `id` FROM `shop`.`items` WHERE {filter} LIMIT 2 FOR UPDATE")
                    .as_str()
            )
        );
        assert_eq!(
            change.statement,
            format!(
                "UPDATE `shop`.`items` SET `name` = 'ab', `weight` = CAST(2.5 AS FLOAT), `doc` = \
                 CAST('[]' AS JSON), `at` = '2026-10-19 11:45:00' WHERE {filter}"
            )
        );
        assert_eq!(
            change.refresh.as_deref(),
            Some("SELECT * FROM `shop`.`items` WHERE `id` = 5")
        );
        assert!(!change.returns_row);
        Ok(())
    }

    #[test]
    fn mariadb_compares_text_with_its_own_collation_and_json_as_text() -> CatalogResult<()> {
        let plan = plan_edits(
            &mariadb(),
            &my_target(),
            &[RowChange::Update {
                key: key(5),
                original: vec![
                    ("name".into(), Value::Text("Ab".into())),
                    ("doc".into(), Value::Text("{}".into())),
                ],
                set: vec![
                    ("name".into(), value(Value::Text("ab".into()))),
                    ("doc".into(), value(Value::Text("[]".into()))),
                ],
            }],
            &options(my_key("longtext"), true),
        )?;
        assert_eq!(
            only(&plan)?.statement,
            "UPDATE `shop`.`items` SET `name` = 'ab', `doc` = '[]' WHERE `id` = 5 AND `name` <=> \
             CONVERT('Ab' USING utf8mb4) COLLATE utf8mb4_nopad_bin AND `doc` <=> CONVERT('{}' \
             USING utf8mb4) COLLATE utf8mb4_nopad_bin"
        );
        Ok(())
    }

    #[test]
    fn mysql_update_of_a_key_reads_back_by_the_new_key() -> CatalogResult<()> {
        let plan = plan_edits(
            &mysql(),
            &my_target(),
            &[
                RowChange::Update {
                    key: key(5),
                    original: vec![],
                    set: vec![("id".into(), value(Value::Int(6)))],
                },
                RowChange::Update {
                    key: key(6),
                    original: vec![],
                    set: vec![("id".into(), CellValue::Expression("id + 1".into()))],
                },
            ],
            &options(my_key("json"), false),
        )?;
        let [first, second] = plan.changes.as_slice() else {
            return Err(invalid("expected two changes"));
        };
        assert_eq!(
            first.refresh.as_deref(),
            Some("SELECT * FROM `shop`.`items` WHERE `id` = 6")
        );
        assert_eq!(second.refresh, None);
        assert_eq!(plan.warnings.len(), 1);
        Ok(())
    }

    #[test]
    fn mysql_delete() -> CatalogResult<()> {
        let plan = plan_edits(
            &mysql(),
            &my_target(),
            &[RowChange::Delete {
                key: key(1),
                original: vec![("weight".into(), Value::Null)],
            }],
            &options(my_key("json"), true),
        )?;
        assert_eq!(
            only(&plan)?.statement,
            "DELETE FROM `shop`.`items` WHERE `id` = 1 AND `weight` <=> NULL"
        );
        assert_eq!(only(&plan)?.lock, None);
        Ok(())
    }

    #[test]
    fn old_mysql_skips_float_comparisons_with_a_warning() -> CatalogResult<()> {
        let plan = plan_edits(
            &server(Engine::Mysql, 8, 0, Some(16)),
            &my_target(),
            &[RowChange::Update {
                key: key(1),
                original: vec![("weight".into(), Value::Float(1.5))],
                set: vec![("weight".into(), value(Value::Float(2.0)))],
            }],
            &options(my_key("json"), true),
        )?;
        assert_eq!(
            only(&plan)?.statement,
            "UPDATE `shop`.`items` SET `weight` = 2.0 WHERE `id` = 1"
        );
        assert_eq!(plan.warnings.len(), 1);
        Ok(())
    }

    #[test]
    fn mysql_column_names_ignore_case() -> CatalogResult<()> {
        let plan = plan_edits(
            &mysql(),
            &my_target(),
            &[RowChange::Delete {
                key: RowKey {
                    columns: vec![("ID".into(), Value::Int(1))],
                },
                original: vec![],
            }],
            &options(my_key("json"), false),
        )?;
        assert_eq!(
            only(&plan)?.statement,
            "DELETE FROM `shop`.`items` WHERE `id` = 1"
        );
        assert!(
            plan_edits(
                &pg(),
                &pg_target(),
                &[RowChange::Delete {
                    key: RowKey {
                        columns: vec![("ID".into(), Value::Int(1))],
                    },
                    original: vec![],
                }],
                &options(pg_key(), false),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn invalid_changes_are_refused() {
        let pg_plan = |change: RowChange, check: bool| {
            plan_edits(&pg(), &pg_target(), &[change], &options(pg_key(), check))
        };
        // Unknown and repeated columns.
        assert!(
            pg_plan(
                RowChange::Insert {
                    values: vec![("nope".into(), CellValue::Default)]
                },
                false
            )
            .is_err()
        );
        assert!(
            pg_plan(
                RowChange::Insert {
                    values: vec![
                        ("name".into(), CellValue::Default),
                        ("name".into(), CellValue::Default)
                    ]
                },
                false
            )
            .is_err()
        );
        // Generated column.
        assert!(
            pg_plan(
                RowChange::Insert {
                    values: vec![("total".into(), value(Value::Int(1)))]
                },
                false
            )
            .is_err()
        );
        // Keys: missing, extra, NULL.
        for columns in [
            vec![],
            vec![("id".into(), Value::Int(1)), ("name".into(), Value::Int(1))],
            vec![("id".into(), Value::Null)],
            vec![("id".into(), Value::Int(1)), ("id".into(), Value::Int(2))],
        ] {
            assert!(
                pg_plan(
                    RowChange::Delete {
                        key: RowKey { columns },
                        original: vec![]
                    },
                    false
                )
                .is_err()
            );
        }
        // Update without changes, and without the original of a changed
        // column when originals are checked.
        assert!(
            pg_plan(
                RowChange::Update {
                    key: key(1),
                    original: vec![],
                    set: vec![]
                },
                false
            )
            .is_err()
        );
        assert!(
            pg_plan(
                RowChange::Update {
                    key: key(1),
                    original: vec![],
                    set: vec![("name".into(), CellValue::Default)]
                },
                true
            )
            .is_err()
        );
        // Expressions that reach outside their parentheses.
        assert!(
            pg_plan(
                RowChange::Update {
                    key: key(1),
                    original: vec![],
                    set: vec![(
                        "name".into(),
                        CellValue::Expression("1) WHERE (true".into())
                    )]
                },
                false
            )
            .is_err()
        );
        // PostgreSQL needs the column types.
        let mut untyped = pg_key();
        for column in &mut untyped.table_columns {
            column.type_schema = None;
        }
        assert!(
            plan_edits(
                &pg(),
                &pg_target(),
                &[RowChange::Delete {
                    key: key(1),
                    original: vec![]
                }],
                &options(untyped, false)
            )
            .is_err()
        );
        // Targets: PostgreSQL needs a schema, MySQL must not have one.
        let mut no_schema = pg_target();
        no_schema.schema = None;
        assert!(plan_edits(&pg(), &no_schema, &[], &options(pg_key(), false)).is_err());
        assert!(plan_edits(&mysql(), &pg_target(), &[], &options(my_key("json"), false)).is_err());
        // MySQL timestamps with a zone other than UTC, and arrays.
        assert!(
            plan_edits(
                &mysql(),
                &my_target(),
                &[RowChange::Insert {
                    values: vec![(
                        "at".into(),
                        value(Value::TimestampTz("2026-10-19T11:45:00+02:00".into()))
                    )]
                }],
                &options(my_key("json"), false)
            )
            .is_err()
        );
        // Redis.
        assert!(
            plan_edits(
                &server(Engine::Redis, 8, 0, None),
                &my_target(),
                &[],
                &options(my_key("json"), false)
            )
            .is_err()
        );
    }

    #[test]
    fn identity_always_takes_only_default() -> CatalogResult<()> {
        let mut key_info = pg_key();
        if let Some(id) = key_info.table_columns.first_mut() {
            id.identity = Some(IdentityKind::Always);
            id.editable = false;
        }
        let options = options(key_info, false);
        assert!(
            plan_edits(
                &pg(),
                &pg_target(),
                &[RowChange::Insert {
                    values: vec![("id".into(), value(Value::Int(1)))]
                }],
                &options
            )
            .is_err()
        );
        plan_edits(
            &pg(),
            &pg_target(),
            &[RowChange::Insert {
                values: vec![("id".into(), CellValue::Default)],
            }],
            &options,
        )?;
        Ok(())
    }

    #[test]
    fn script_wraps_the_plan_in_a_transaction() -> CatalogResult<()> {
        let plan = plan_edits(
            &mysql(),
            &my_target(),
            &[RowChange::Update {
                key: key(1),
                original: vec![],
                set: vec![("name".into(), value(Value::Text("x".into())))],
            }],
            &options(my_key("json"), false),
        )?;
        assert_eq!(
            plan.script(),
            "START TRANSACTION;\n\
             SELECT `id` FROM `shop`.`items` WHERE `id` = 1 LIMIT 2 FOR UPDATE;\n\
             UPDATE `shop`.`items` SET `name` = 'x' WHERE `id` = 1;\n\
             SELECT * FROM `shop`.`items` WHERE `id` = 1;\n\
             COMMIT;\n"
        );
        Ok(())
    }

    #[test]
    fn changes_serialize_in_camel_case() -> Result<(), serde_json::Error> {
        let change = RowChange::Update {
            key: key(1),
            original: vec![("name".into(), Value::Null)],
            set: vec![
                ("name".into(), CellValue::Expression("now()".into())),
                ("doc".into(), CellValue::Default),
            ],
        };
        let json = serde_json::to_value(&change)?;
        assert_eq!(
            json,
            serde_json::json!({
                "op": "update",
                "key": { "columns": [["id", { "t": "int", "v": 1 }]] },
                "original": [["name", { "t": "null" }]],
                "set": [
                    ["name", { "kind": "expression", "value": "now()" }],
                    ["doc", { "kind": "default" }]
                ]
            })
        );
        let back: RowChange = serde_json::from_value(json)?;
        assert_eq!(back, change);
        let options = serde_json::to_value(options(pg_key(), true))?;
        assert_eq!(options["checkOriginal"], true);
        assert_eq!(
            options["key"]["tableColumns"][0]["typeSchema"],
            "pg_catalog"
        );
        assert_eq!(options["key"]["tableColumns"][0]["identity"], "byDefault");
        Ok(())
    }

    fn json(text: &str) -> CatalogResult<JsonText> {
        JsonText::parse(text).ok_or_else(|| invalid("not JSON"))
    }
}
