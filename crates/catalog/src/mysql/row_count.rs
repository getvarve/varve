//! An exact row count under a server-side time cap, for the table
//! designer's review of an engine change that loses rows. The count runs
//! on whatever session it is given (the app uses the metadata session, so
//! it neither occupies the user's tab nor lands in its query history) and
//! gives up after the cap: the caller then keeps the estimate
//! (`information_schema.TABLES.TABLE_ROWS`).

use varve_core::Engine;
use varve_driver::Session;

use crate::model::{CatalogError, CatalogResult};

use super::util;

/// The time cap the designer uses: long enough for a table that fits in the
/// buffer pool, short enough that a review never waits on a huge one.
pub const ROW_COUNT_CAP_MS: u32 = 2000;

/// MySQL `ER_QUERY_TIMEOUT` (3024, `MAX_EXECUTION_TIME` exceeded),
/// MariaDB `ER_STATEMENT_TIMEOUT` (1969, `max_statement_time` exceeded),
/// and `ER_QUERY_INTERRUPTED` (1317), which MySQL also reports when
/// `MAX_EXECUTION_TIME` stops a statement in some plans (joins, views).
/// The driver writes the code as `number (SQLSTATE)`.
const TIMEOUT_CODES: &[&str] = &["3024", "1969", "1317"];

/// `SELECT COUNT(*) FROM <from>` capped at `cap_ms` on the server: an
/// optimizer hint on MySQL, `SET STATEMENT max_statement_time` on MariaDB.
fn capped_count_sql(engine: Engine, from: &str, cap_ms: u32) -> CatalogResult<String> {
    match engine {
        Engine::Mysql => Ok(format!(
            "SELECT /*+ MAX_EXECUTION_TIME({cap_ms}) */ COUNT(*) AS `n` FROM {from}"
        )),
        Engine::Mariadb => Ok(format!(
            "SET STATEMENT max_statement_time={}.{:03} FOR SELECT COUNT(*) AS `n` FROM {from}",
            cap_ms / 1000,
            cap_ms % 1000
        )),
        Engine::Postgres | Engine::Redis => Err(CatalogError::Unsupported {
            message: "the capped row count is for MySQL and MariaDB tables".into(),
        }),
    }
}

/// True for the error a capped statement ends with when it runs out of time.
fn is_timeout(error: &CatalogError) -> bool {
    let CatalogError::Driver { error } = error else {
        return false;
    };
    let number = error
        .code
        .as_deref()
        .and_then(|c| c.split_whitespace().next())
        .unwrap_or_default();
    TIMEOUT_CODES.contains(&number)
}

/// Counts the rows of `FROM <from>` (SQL the caller built from quoted
/// names) with a cap; `None` when the cap ran out.
pub(crate) async fn count_from(
    session: &dyn Session,
    from: &str,
    cap_ms: u32,
) -> CatalogResult<Option<u64>> {
    let sql = capped_count_sql(session.server_info().engine, from, cap_ms)?;
    match crate::rows::query(session, &sql).await {
        Ok(rows) => {
            let row = rows.single("the row count")?;
            Ok(row.int("n")?.and_then(|n| u64::try_from(n).ok()))
        }
        Err(error) if is_timeout(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The exact number of rows of `database.table`, or `None` when counting
/// takes longer than `cap_ms` (the server stops the statement).
///
/// # Errors
///
/// `Unsupported` for a session that is not MySQL or MariaDB; driver errors
/// other than the timeout (a missing table, no privilege).
pub async fn row_count(
    session: &dyn Session,
    database: &str,
    table: &str,
    cap_ms: u32,
) -> CatalogResult<Option<u64>> {
    count_from(session, &util::qname(database, table)?, cap_ms).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use varve_driver::{DriverError, ErrorKind};

    #[test]
    fn caps_the_count_on_the_server() -> CatalogResult<()> {
        assert_eq!(
            capped_count_sql(Engine::Mysql, "`d`.`t`", 2000)?,
            "SELECT /*+ MAX_EXECUTION_TIME(2000) */ COUNT(*) AS `n` FROM `d`.`t`"
        );
        assert_eq!(
            capped_count_sql(Engine::Mariadb, "`d`.`t`", 2000)?,
            "SET STATEMENT max_statement_time=2.000 FOR SELECT COUNT(*) AS `n` FROM `d`.`t`"
        );
        assert_eq!(
            capped_count_sql(Engine::Mariadb, "`d`.`t`", 5)?,
            "SET STATEMENT max_statement_time=0.005 FOR SELECT COUNT(*) AS `n` FROM `d`.`t`"
        );
        assert!(capped_count_sql(Engine::Postgres, "t", 1).is_err());
        Ok(())
    }

    #[test]
    fn a_timeout_is_no_count_other_errors_stay_errors() {
        let driver = |code: &str| CatalogError::Driver {
            error: DriverError::new(ErrorKind::Query, "stopped").with_code(code),
        };
        assert!(is_timeout(&driver("3024")));
        assert!(is_timeout(&driver("1969 (70100)")));
        assert!(is_timeout(&driver("1317 (70100)")));
        assert!(!is_timeout(&driver("1146 (42S02)")));
    }
}
