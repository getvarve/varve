//! Reading catalog query results by column name.

use varve_driver::{ExecOptions, ExecOutput, Session, StatementResult, Value};

use crate::model::{CatalogError, CatalogResult};

/// Catalog queries may return large lists (every column of a big
/// database), but never unbounded: more rows than this is an error rather
/// than a silently cut list.
pub const MAX_CATALOG_ROWS: usize = 1_000_000;

/// The rows of one result set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Rows {
    /// Every result set of an output that returned rows, in order.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Unexpected`] when a result set was cut at
    /// [`MAX_CATALOG_ROWS`].
    pub fn all(output: ExecOutput) -> CatalogResult<Vec<Rows>> {
        let mut sets = Vec::new();
        for result in output.results {
            if let StatementResult::Rows {
                columns,
                rows,
                truncated,
            } = result
            {
                if truncated {
                    return Err(CatalogError::Unexpected {
                        message: format!("more than {MAX_CATALOG_ROWS} catalog rows"),
                    });
                }
                sets.push(Rows {
                    columns: columns.into_iter().map(|c| c.name).collect(),
                    rows,
                });
            }
        }
        Ok(sets)
    }

    /// The first result set that returned rows.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Unexpected`] when no statement returned rows or the
    /// result was cut.
    pub fn first(output: ExecOutput) -> CatalogResult<Rows> {
        Self::all(output)?
            .into_iter()
            .next()
            .ok_or_else(|| CatalogError::Unexpected {
                message: "the query returned no result set".into(),
            })
    }

    /// Column names.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows.
    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(|values| Row {
            columns: &self.columns,
            values,
        })
    }

    /// The single row of a result that must have exactly one.
    ///
    /// # Errors
    ///
    /// [`CatalogError::NotFound`] for no rows, [`CatalogError::Unexpected`]
    /// for more than one.
    pub fn single(&self, what: &str) -> CatalogResult<Row<'_>> {
        match self.rows.as_slice() {
            [values] => Ok(Row {
                columns: &self.columns,
                values,
            }),
            [] => Err(CatalogError::NotFound {
                message: format!("{what} was not found"),
            }),
            _ => Err(CatalogError::Unexpected {
                message: format!("{what} matched more than one row"),
            }),
        }
    }
}

/// One row, read by column name.
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    columns: &'a [String],
    values: &'a [Value],
}

impl<'a> Row<'a> {
    /// The value of a column.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Unexpected`] when the column is missing, which means
    /// the catalog query and its reader disagree.
    pub fn value(&self, column: &str) -> CatalogResult<&'a Value> {
        self.columns
            .iter()
            .position(|c| c == column)
            .and_then(|i| self.values.get(i))
            .ok_or_else(|| CatalogError::Unexpected {
                message: format!("column {column} is missing"),
            })
    }

    /// The value as text; `None` for `NULL`.
    ///
    /// # Errors
    ///
    /// As [`Row::value`].
    pub fn text(&self, column: &str) -> CatalogResult<Option<String>> {
        Ok(self.value(column)?.to_plain_text())
    }

    /// The value as text, which must not be `NULL`.
    ///
    /// # Errors
    ///
    /// As [`Row::value`], and [`CatalogError::Unexpected`] for `NULL`.
    pub fn req_text(&self, column: &str) -> CatalogResult<String> {
        self.text(column)?.ok_or_else(|| CatalogError::Unexpected {
            message: format!("column {column} is NULL"),
        })
    }

    /// The value as an integer; `None` for `NULL`. Accepts integer, decimal
    /// without fraction and numeric text values.
    ///
    /// # Errors
    ///
    /// As [`Row::value`], and [`CatalogError::Unexpected`] for a value that
    /// is not an integer.
    pub fn int(&self, column: &str) -> CatalogResult<Option<i64>> {
        let value = self.value(column)?;
        match value {
            Value::Null => Ok(None),
            Value::Int(i) => Ok(Some(*i)),
            Value::UInt(u) => i64::try_from(*u).map(Some).map_err(|_| not_int(column)),
            Value::Bool(b) => Ok(Some(i64::from(*b))),
            Value::Decimal(s) | Value::Text(s) | Value::Other(s) => {
                s.trim().parse().map(Some).map_err(|_| not_int(column))
            }
            _ => Err(not_int(column)),
        }
    }

    /// The value as a boolean; `None` for `NULL`. Accepts booleans,
    /// integers (MySQL `TINYINT(1)`) and the texts `t`/`f`, `true`/`false`,
    /// `yes`/`no`, `on`/`off`, `1`/`0`, `enabled`/`disabled`.
    ///
    /// # Errors
    ///
    /// As [`Row::value`], and [`CatalogError::Unexpected`] for anything else.
    pub fn bool(&self, column: &str) -> CatalogResult<Option<bool>> {
        let value = self.value(column)?;
        let not_bool = || CatalogError::Unexpected {
            message: format!("column {column} is not a boolean"),
        };
        match value {
            Value::Null => Ok(None),
            Value::Bool(b) => Ok(Some(*b)),
            Value::Int(i) => Ok(Some(*i != 0)),
            Value::UInt(u) => Ok(Some(*u != 0)),
            Value::Text(s) | Value::Other(s) | Value::Decimal(s) => {
                match s.trim().to_ascii_lowercase().as_str() {
                    "t" | "true" | "yes" | "y" | "on" | "1" | "enabled" => Ok(Some(true)),
                    "f" | "false" | "no" | "n" | "off" | "0" | "disabled" => Ok(Some(false)),
                    _ => Err(not_bool()),
                }
            }
            _ => Err(not_bool()),
        }
    }
}

fn not_int(column: &str) -> CatalogError {
    CatalogError::Unexpected {
        message: format!("column {column} is not an integer"),
    }
}

/// Runs a catalog query and returns its first result set.
///
/// # Errors
///
/// Driver errors, and as [`Rows::first`].
pub async fn query(session: &dyn Session, sql: &str) -> CatalogResult<Rows> {
    Rows::first(run(session, sql).await?)
}

/// Runs catalog SQL (one or more statements) with the catalog row limit.
///
/// # Errors
///
/// Driver errors.
pub async fn run(session: &dyn Session, sql: &str) -> CatalogResult<ExecOutput> {
    Ok(session
        .execute(
            sql,
            &ExecOptions {
                max_rows: MAX_CATALOG_ROWS,
            },
        )
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_driver::Column;

    fn output() -> ExecOutput {
        ExecOutput {
            results: vec![
                StatementResult::Command {
                    tag: "SET".into(),
                    rows_affected: None,
                },
                StatementResult::Rows {
                    columns: vec![
                        Column {
                            name: "name".into(),
                            type_name: "text".into(),
                        },
                        Column {
                            name: "n".into(),
                            type_name: "int8".into(),
                        },
                        Column {
                            name: "flag".into(),
                            type_name: "text".into(),
                        },
                    ],
                    rows: vec![vec![
                        Value::Text("t".into()),
                        Value::Decimal("42".into()),
                        Value::Text("YES".into()),
                    ]],
                    truncated: false,
                },
            ],
            notices: vec![],
            elapsed_ms: 0,
        }
    }

    #[test]
    fn reads_by_column_name() -> CatalogResult<()> {
        let rows = Rows::first(output())?;
        let row = rows.single("t")?;
        assert_eq!(row.req_text("name")?, "t");
        assert_eq!(row.int("n")?, Some(42));
        assert_eq!(row.bool("flag")?, Some(true));
        assert!(row.value("missing").is_err());
        Ok(())
    }

    #[test]
    fn truncated_results_are_errors() {
        let mut out = output();
        if let Some(StatementResult::Rows { truncated, .. }) = out.results.get_mut(1) {
            *truncated = true;
        }
        assert!(Rows::first(out).is_err());
    }
}
