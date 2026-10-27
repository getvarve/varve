//! Redis modules and console help (design doc 4.2): which module features
//! a server has, the Search indexes, TimeSeries ranges and the read-only
//! queries of probabilistic keys, and `COMMAND DOCS` help for the console.
//!
//! Module detection and the Search indexes are server panels: they run on
//! the profile's admin session ([`super::metadata`]). Key reads (ranges,
//! queries) and console help run on the tab's own session, like the value
//! viewer, so they see the tab's database. Module key changes are value
//! edits ([`super::redis_edit`]).

use varve_core::Engine;
use varve_driver_redis::{
    CommandHelp, ModuleQuery, ModuleReport, QueryResult, RedisSession, SearchIndex, TsRangePage,
    TsRangeQuery,
};

use super::redis_edit::{driver_err, name_bytes};
use crate::commands::{CmdError, Services};

type CmdResult<T> = Result<T, CmdError>;

/// A driver error of an admin call.
fn admin_err(error: varve_driver::DriverError) -> CmdError {
    driver_err(error)
}

fn not_redis() -> CmdError {
    super::unsupported("this session is not a Redis connection")
}

impl Services {
    /// Runs `work` on the profile's admin session as a [`RedisSession`].
    async fn redis_admin_session<T, F>(&self, session_id: &str, work: F) -> CmdResult<T>
    where
        F: for<'a> Fn(&'a RedisSession) -> super::metadata::AdminFuture<'a, T>,
    {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "This Redis panel")?;
        self.with_admin(session_id, true, |session| {
            match session.as_any().downcast_ref::<RedisSession>() {
                Some(redis) => work(redis),
                None => Box::pin(async { Err(not_redis()) }),
            }
        })
        .await
    }

    /// Which module features (JSON, Search, TimeSeries, Bloom, Cuckoo,
    /// Count-Min, Top-K, t-digest) the server has, with the reason for each
    /// missing one, and `MODULE LIST` when the user may run it.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_modules(&self, session_id: &str) -> CmdResult<ModuleReport> {
        self.redis_admin_session(session_id, |redis| {
            Box::pin(async move { redis.modules().await.map_err(admin_err) })
        })
        .await
    }

    /// The names of the Search indexes (`FT._LIST`).
    ///
    /// # Errors
    /// Unknown or non-Redis session; `unsupported` (code
    /// `SEARCH_UNAVAILABLE`) without Search; driver errors.
    pub async fn redis_search_indexes(&self, session_id: &str) -> CmdResult<Vec<String>> {
        self.redis_admin_session(session_id, |redis| {
            Box::pin(async move { redis.search_indexes().await.map_err(admin_err) })
        })
        .await
    }

    /// One Search index (`FT.INFO`) with its definition rebuilt as an
    /// `FT.CREATE` command; `None` when it does not exist.
    ///
    /// # Errors
    /// As [`Self::redis_search_indexes`].
    pub async fn redis_search_index(
        &self,
        session_id: &str,
        name: &str,
    ) -> CmdResult<Option<SearchIndex>> {
        let name = name.to_string();
        self.redis_admin_session(session_id, move |redis| {
            let name = name.clone();
            Box::pin(async move { redis.search_index(&name).await.map_err(admin_err) })
        })
        .await
    }

    /// A page of samples of a time series on the tab's session; `None`
    /// when the key does not exist.
    ///
    /// # Errors
    /// Unknown or non-Redis session, a malformed key, an invalid range or
    /// cursor (`invalid`), another type (`WRONGTYPE`), driver errors.
    pub async fn redis_ts_range(
        &self,
        session_id: &str,
        key: &str,
        query: &TsRangeQuery,
    ) -> CmdResult<Option<TsRangePage>> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .ts_range(&key, query)
            .await
            .map_err(driver_err)
    }

    /// A read-only query of a Bloom or Cuckoo filter, Count-Min sketch,
    /// Top-K or t-digest on the tab's session (read-only sessions too);
    /// `None` when the key does not exist.
    ///
    /// # Errors
    /// Unknown or non-Redis session, invalid inputs (`invalid`), a key of
    /// another kind (`WRONGTYPE`), driver errors.
    pub async fn redis_module_query(
        &self,
        session_id: &str,
        key: &str,
        query: &ModuleQuery,
    ) -> CmdResult<Option<QueryResult>> {
        let key = name_bytes(key)?;
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .module_query(&key, query)
            .await
            .map_err(driver_err)
    }

    /// Console help for a command (`COMMAND DOCS`, or the arity and flags
    /// of `COMMAND INFO` with a note on servers without it), on the tab's
    /// session; never queued into an open console `MULTI` block.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_command_help(&self, session_id: &str, name: &str) -> CmdResult<CommandHelp> {
        let (_, lock) = self.redis_session_lock(session_id).await?;
        Self::redis_of(&lock, session_id)?
            .command_help(name)
            .await
            .map_err(driver_err)
    }
}
