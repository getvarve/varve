//! Redis live server panels (design doc 4.2): server info, databases, slow
//! log, latency monitor, clients, configuration, memory analysis, cluster
//! and Sentinel topology, functions and Pub/Sub. They run on the profile's
//! admin session ([`super::metadata`]), never under the user's session
//! lock; configuration and function changes are server-held plans
//! ([`super::plans`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use varve_activity::{
    ActivityEvent, Category, ErrorDetails, Outcome as EventOutcome, RowCounts, SqlDialect,
};
use varve_catalog::{CatalogError, CatalogResult};
use varve_core::Engine;
use varve_driver::{ServerInfo, Session, Value};
use varve_driver_redis::RedisSession;
use varve_live_stats::redis::{
    self as live, ClientInfo, ClusterTopology, CommandPlan, ConfigChange, ConfigEntry, Databases,
    FlushMode, FunctionLibrary, LatencyReport, LatencySample, MemoryAnalysis, MemoryScanOptions,
    PubSubSnapshot, RedisInfo, SentinelTopology, SlowLog,
};

use super::metadata::AdminFuture;
use super::plans::{Planned, StoredPlan, wrong_kind};
use super::{elapsed_ms, error_details};
use crate::commands::{CmdError, Services};
use crate::sessions::{SessionInfo, redis_destructive_commands};

type CmdResult<T> = Result<T, CmdError>;

/// Most channels a Pub/Sub snapshot lists when the UI does not say.
pub const DEFAULT_PUBSUB_CHANNELS: usize = 1000;

/// Slow log entries read when the UI does not say.
pub const DEFAULT_SLOWLOG_COUNT: u32 = 128;

/// A configuration change to plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConfigRequest {
    /// `CONFIG SET` of one or more parameters.
    Set {
        /// The parameters and their new values.
        changes: Vec<ConfigChange>,
    },
    /// `CONFIG REWRITE` of the configuration file.
    Rewrite,
}

/// A functions or scripts change to plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FunctionRequest {
    /// `FUNCTION LOAD` of a library (Redis 7.0 and newer, Valkey).
    Load {
        /// Library code, starting with `#!lua name=...`.
        code: String,
        /// Replace a library of the same name.
        #[serde(default)]
        replace: bool,
    },
    /// `FUNCTION DELETE` of a library.
    Delete {
        /// Library name.
        library: String,
    },
    /// `SCRIPT FLUSH` of the Lua script cache.
    ScriptFlush {
        /// How the cache is freed.
        #[serde(default)]
        mode: FlushMode,
    },
}

/// Cluster and Sentinel topology. Both are `None` on a standalone server.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedisTopology {
    /// Cluster nodes and slot map, when cluster mode is on.
    pub cluster: Option<ClusterTopology>,
    /// Monitored primaries, when the server is a Sentinel.
    pub sentinel: Option<SentinelTopology>,
}

/// A Redis plan as the UI sees it: the command lines with secret values
/// masked. The real commands stay on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedisPlanView {
    /// The commands, secrets replaced by `********`.
    pub display: Vec<String>,
    /// True when the plan can lose data or lock clients out: applying it
    /// needs confirmation in every environment.
    pub destructive: bool,
    /// True when the commands carry a secret value.
    pub sensitive: bool,
    /// Warnings for the user.
    pub warnings: Vec<String>,
}

impl From<&CommandPlan> for RedisPlanView {
    fn from(plan: &CommandPlan) -> Self {
        Self {
            display: plan.display.clone(),
            destructive: plan.destructive,
            sensitive: plan.sensitive,
            warnings: plan.warnings.clone(),
        }
    }
}

/// The command names of a plan, for the confirmation dialog.
pub(crate) fn command_names(plan: &CommandPlan) -> Vec<String> {
    let mut names: Vec<String> = plan
        .display
        .iter()
        .filter_map(|line| line.split_whitespace().next())
        .map(|name| {
            name.trim_matches(|c| c == '"' || c == '\'')
                .to_ascii_uppercase()
        })
        .collect();
    names.dedup();
    names
}

/// How long a stop for a memory analysis that has not started is kept.
const EARLY_STOP_KEPT: Duration = Duration::from_secs(600);

/// A memory analysis run: its session, stop flag and start (or stop) time.
type MemoryRun = (String, Arc<AtomicBool>, Instant);

/// The stop flags of the memory analyses, by run id (chosen by the UI), so
/// Stop reaches a running analysis without waiting for the admin session
/// it holds.
#[derive(Debug, Default)]
pub struct MemoryRuns {
    runs: Mutex<HashMap<String, MemoryRun>>,
}

impl MemoryRuns {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, MemoryRun>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The stop flag of a run that starts now (already set when a stop
    /// came first).
    fn start(&self, session_id: &str, run_id: &str) -> Arc<AtomicBool> {
        let mut runs = self.lock();
        runs.retain(|_, (_, stop, at)| {
            !stop.load(Ordering::SeqCst) || at.elapsed() < EARLY_STOP_KEPT
        });
        let entry = runs.entry(run_id.to_string()).or_insert_with(|| {
            (
                session_id.to_string(),
                Arc::new(AtomicBool::new(false)),
                Instant::now(),
            )
        });
        if entry.0 == session_id {
            Arc::clone(&entry.1)
        } else {
            Arc::new(AtomicBool::new(false))
        }
    }

    fn finish(&self, session_id: &str, run_id: &str) {
        let mut runs = self.lock();
        if runs
            .get(run_id)
            .is_some_and(|(owner, ..)| owner == session_id)
        {
            runs.remove(run_id);
        }
    }

    fn stop(&self, session_id: &str, run_id: &str) {
        let mut runs = self.lock();
        match runs.get(run_id) {
            Some((owner, stop, _)) if owner == session_id => stop.store(true, Ordering::SeqCst),
            Some(_) => {}
            None => {
                runs.insert(
                    run_id.to_string(),
                    (
                        session_id.to_string(),
                        Arc::new(AtomicBool::new(true)),
                        Instant::now(),
                    ),
                );
            }
        }
    }

    /// Stops and forgets the runs of a closed session.
    pub fn forget_session(&self, session_id: &str) {
        self.lock().retain(|_, (owner, stop, _)| {
            let keep = owner != session_id;
            if !keep {
                stop.store(true, Ordering::SeqCst);
            }
            keep
        });
    }
}

/// A panel call's error as the UI sees it.
fn catalog_err(error: CatalogError) -> CmdError {
    error.into()
}

impl Services {
    /// Runs a Redis panel call on the profile's admin session.
    async fn redis_admin<T, F>(
        &self,
        session_id: &str,
        retry: bool,
        call: F,
    ) -> CmdResult<(SessionInfo, T)>
    where
        F: for<'a> Fn(&'a dyn Session, ServerInfo) -> AdminFuture<'a, T>,
    {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "This Redis panel")?;
        let server = info.server.clone();
        let value = self
            .with_admin(session_id, retry, |session| call(session, server.clone()))
            .await?;
        Ok((info, value))
    }

    /// Records a Redis action under `stats` or `query`.
    fn record_redis(
        &self,
        info: &SessionInfo,
        category: Category,
        action: &str,
        started: Instant,
        result: Result<(), &CmdError>,
    ) {
        let mut event = ActivityEvent::builder(category, action).duration_ms(elapsed_ms(started));
        if let Err(error) = result {
            event = event.outcome(EventOutcome::Failure).error(
                &match error {
                    CmdError::Catalog {
                        error: CatalogError::Driver { error },
                    } => error_details(error),
                    other => ErrorDetails {
                        code: None,
                        message: other.to_string(),
                    },
                },
                Some(SqlDialect::Redis),
            );
        }
        self.record(info, event);
    }

    /// `INFO everything`, parsed.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors (`catalog`).
    pub async fn redis_info(&self, session_id: &str) -> CmdResult<RedisInfo> {
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                Box::pin(async move {
                    live::server_info(session, &server)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await?;
        Ok(value)
    }

    /// Logical databases with key counts.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_databases(&self, session_id: &str) -> CmdResult<Databases> {
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                Box::pin(
                    async move { live::databases(session, &server).await.map_err(catalog_err) },
                )
            })
            .await?;
        Ok(value)
    }

    /// The newest `count` slow log entries (default
    /// [`DEFAULT_SLOWLOG_COUNT`]).
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_slowlog(&self, session_id: &str, count: Option<u32>) -> CmdResult<SlowLog> {
        let count = count.unwrap_or(DEFAULT_SLOWLOG_COUNT);
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                Box::pin(async move {
                    live::slowlog(session, &server, count)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await?;
        Ok(value)
    }

    /// Clears the slow log; logged as `slowlog.reset`.
    ///
    /// # Errors
    /// Unknown, non-Redis or read-only session, driver errors.
    pub async fn redis_slowlog_reset(&self, session_id: &str) -> CmdResult<()> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "This Redis panel")?;
        Self::require_writable(&info, "resetting the slow log")?;
        let started = Instant::now();
        let result = self
            .redis_admin(session_id, false, |session, server| {
                Box::pin(async move {
                    live::reset_slowlog(session, &server)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await
            .map(|_| ());
        self.record_redis(
            &info,
            Category::Stats,
            "slowlog.reset",
            started,
            result.as_ref().map(|_| ()),
        );
        result
    }

    /// The latest latency spike per event.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_latency(&self, session_id: &str) -> CmdResult<LatencyReport> {
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                Box::pin(async move { live::latency(session, &server).await.map_err(catalog_err) })
            })
            .await?;
        Ok(value)
    }

    /// The latency history of one event.
    ///
    /// # Errors
    /// Unknown or non-Redis session, an invalid event name, driver errors.
    pub async fn redis_latency_history(
        &self,
        session_id: &str,
        event: &str,
    ) -> CmdResult<Vec<LatencySample>> {
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                let event = event.to_string();
                Box::pin(async move {
                    live::latency_history(session, &server, &event)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await?;
        Ok(value)
    }

    /// Clears the latency history of `events` (all when empty) and returns
    /// how many were reset; logged as `latency.reset`.
    ///
    /// # Errors
    /// Unknown, non-Redis or read-only session, driver errors.
    pub async fn redis_latency_reset(&self, session_id: &str, events: &[String]) -> CmdResult<i64> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "This Redis panel")?;
        Self::require_writable(&info, "resetting the latency monitor")?;
        let started = Instant::now();
        let result = self
            .redis_admin(session_id, false, |session, server| {
                let events = events.to_vec();
                Box::pin(async move {
                    live::reset_latency(session, &server, &events)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await
            .map(|(_, n)| n);
        self.record_redis(
            &info,
            Category::Stats,
            "latency.reset",
            started,
            result.as_ref().map(|_| ()),
        );
        result
    }

    /// Connected clients, listed from the admin session (`isSelf` marks the
    /// admin connection; the user's session is a normal entry).
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_clients(&self, session_id: &str) -> CmdResult<Vec<ClientInfo>> {
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                Box::pin(async move {
                    live::client_list(session, &server)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await?;
        Ok(value)
    }

    /// Closes a client connection (never the admin session's own); logged
    /// as `process.killed`.
    ///
    /// # Errors
    /// Unknown, non-Redis or read-only session, the own connection
    /// (`catalog` / `invalid`), driver errors.
    pub async fn redis_client_kill(&self, session_id: &str, id: i64) -> CmdResult<()> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "This Redis panel")?;
        Self::require_writable(&info, "closing client connections")?;
        let started = Instant::now();
        let result = self
            .redis_admin(session_id, false, |session, server| {
                Box::pin(async move {
                    live::kill_client(session, &server, id)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await
            .map(|_| ());
        let mut event = ActivityEvent::builder(Category::Query, "process.killed")
            .target(id.to_string())
            .detail("process_id", id)
            .detail("kill", "connection")
            .duration_ms(elapsed_ms(started));
        if let Err(error) = &result {
            event = event.outcome(EventOutcome::Failure).error(
                &ErrorDetails {
                    code: match error {
                        CmdError::Catalog {
                            error: CatalogError::Driver { error },
                        } => error.code.clone(),
                        _ => None,
                    },
                    message: error.to_string(),
                },
                Some(SqlDialect::Redis),
            );
        }
        self.record(&info, event);
        result
    }

    /// `CONFIG GET` for a pattern (default `*`); secret values are masked.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_config(
        &self,
        session_id: &str,
        pattern: Option<&str>,
    ) -> CmdResult<Vec<ConfigEntry>> {
        let pattern = pattern.unwrap_or("*").to_string();
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                let pattern = pattern.clone();
                Box::pin(async move {
                    live::config_get(session, &server, &pattern)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await?;
        Ok(value)
    }

    /// Plans a configuration change and keeps the plan on the server; the
    /// UI gets the masked view.
    ///
    /// # Errors
    /// Unknown or non-Redis session, invalid changes, driver errors.
    pub async fn redis_config_plan(
        &self,
        session_id: &str,
        request: &ConfigRequest,
    ) -> CmdResult<Planned<RedisPlanView>> {
        let (_, plan) = self
            .redis_admin(session_id, true, |session, server| {
                let request = request.clone();
                Box::pin(async move {
                    match request {
                        ConfigRequest::Set { changes } => {
                            live::plan_config_set(session, &server, &changes).await
                        }
                        ConfigRequest::Rewrite => live::plan_config_rewrite(session, &server).await,
                    }
                    .map_err(catalog_err)
                })
            })
            .await?;
        Ok(self.store_redis_plan(session_id, plan))
    }

    /// Plans a functions or script cache change and keeps it on the server.
    ///
    /// # Errors
    /// Unknown or non-Redis session, a server without functions, invalid
    /// code or names.
    pub fn redis_function_plan(
        &self,
        session_id: &str,
        request: &FunctionRequest,
    ) -> CmdResult<Planned<RedisPlanView>> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "This Redis panel")?;
        let plan: CatalogResult<CommandPlan> = match request {
            FunctionRequest::Load { code, replace } => {
                live::function_load_plan(&info.server, code, *replace)
            }
            FunctionRequest::Delete { library } => {
                live::function_delete_plan(&info.server, library)
            }
            FunctionRequest::ScriptFlush { mode } => {
                Ok(live::script_flush_plan(&info.server, *mode))
            }
        };
        Ok(self.store_redis_plan(session_id, plan?))
    }

    fn store_redis_plan(&self, session_id: &str, plan: CommandPlan) -> Planned<RedisPlanView> {
        let view = RedisPlanView::from(&plan);
        let plan_id = self.m2.plans.put(session_id, StoredPlan::Redis(plan));
        Planned {
            plan_id,
            plan: view,
        }
    }

    /// Runs a stored Redis plan on the admin session and returns each
    /// reply.
    ///
    /// Refused on read-only sessions. It needs `confirmed` when the plan is
    /// destructive (always), or when the session confirms destructive
    /// statements and the server flags one of its commands as destructive
    /// (every command is parsed as the server runs it, so quoting a name
    /// does not hide it). Logged as a query with the masked command lines.
    ///
    /// # Errors
    /// As described, an unknown, used or expired plan id, and the failing
    /// command's driver error.
    pub async fn redis_plan_apply(
        &self,
        session_id: &str,
        plan_id: &str,
        confirmed: bool,
    ) -> CmdResult<Vec<Value>> {
        let info = self.session_info(session_id)?;
        Self::require_engine(&info, &[Engine::Redis], "This Redis panel")?;
        Self::require_writable(&info, "changing the server")?;
        let stored = self.m2.plans.peek(session_id, plan_id)?;
        let StoredPlan::Redis(plan) = &stored else {
            return Err(wrong_kind("redis_plan_apply", &stored));
        };
        let script = plan.commands.join("\n");
        let flagged = if info.confirm_destructive {
            let (_, found) = self
                .redis_admin(session_id, true, |session, _| {
                    let script = script.clone();
                    Box::pin(async move {
                        let Some(redis) = session.as_any().downcast_ref::<RedisSession>() else {
                            return Ok(Vec::new());
                        };
                        redis_destructive_commands(redis, &script)
                            .await
                            .map_err(|error| catalog_err(CatalogError::Driver { error }))
                    })
                })
                .await?;
            !found.is_empty()
        } else {
            false
        };
        self.require_confirmation(
            &info,
            plan.destructive || flagged,
            confirmed,
            command_names(plan),
        )?;
        // The plan is spent only once the session that runs it is open.
        self.admin_session(session_id).await?;
        let stored = self.m2.plans.take(session_id, plan_id)?;
        let StoredPlan::Redis(plan) = stored else {
            return Err(wrong_kind("redis_plan_apply", &stored));
        };
        let started = Instant::now();
        let run_plan = plan.clone();
        let result = self
            .redis_admin(session_id, false, |session, server| {
                let plan = run_plan.clone();
                Box::pin(async move {
                    live::apply_plan(session, &server, &plan)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await
            .map(|(_, replies)| replies);
        let duration_ms = elapsed_ms(started);
        let text = plan.display.join("\n");
        let event = match &result {
            Ok(replies) => ActivityEvent::query_executed(
                &text,
                SqlDialect::Redis,
                "plan",
                RowCounts {
                    returned: Some(replies.len() as u64),
                    affected: None,
                },
                duration_ms,
            ),
            Err(error) => ActivityEvent::query_failed(
                &text,
                SqlDialect::Redis,
                "plan",
                &match error {
                    CmdError::Catalog {
                        error: CatalogError::Driver { error },
                    } => error_details(error),
                    other => ErrorDetails {
                        code: None,
                        message: other.to_string(),
                    },
                },
                duration_ms,
            ),
        };
        self.record(&info, event);
        if result.is_ok() && confirmed && (plan.destructive || flagged) {
            self.record(
                &info,
                ActivityEvent::destructive_confirmed(
                    command_names(&plan).join(","),
                    &super::environment_label(&info),
                ),
            );
        }
        result
    }

    /// Memory use per key prefix and type, and the biggest keys, sampled
    /// with `SCAN` and `MEMORY USAGE` within the options' budget, on the
    /// admin session (the profile's Redis database).
    ///
    /// # Errors
    /// Unknown or non-Redis session, invalid options, driver errors.
    pub async fn redis_memory_analysis(
        &self,
        session_id: &str,
        options: Option<&MemoryScanOptions>,
        run_id: Option<&str>,
    ) -> CmdResult<MemoryAnalysis> {
        let options = options.cloned().unwrap_or_default();
        let stop = match run_id {
            Some(id) => self.m2.memory_runs.start(session_id, id),
            None => Arc::new(AtomicBool::new(false)),
        };
        let result = self
            .redis_admin(session_id, true, |session, server| {
                let options = options.clone();
                let stop = Arc::clone(&stop);
                Box::pin(async move {
                    live::analyze_memory_until(session, &server, &options, &stop)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await;
        if let Some(id) = run_id {
            self.m2.memory_runs.finish(session_id, id);
        }
        Ok(result?.1)
    }

    /// Stops the memory analysis `run_id` of the tab: its scan loop ends
    /// before its next step (at most one `SCAN` step or measurement batch
    /// later) and the analysis returns what it measured, marked
    /// `stoppedBy: stopped`. A stop that arrives before the analysis
    /// started stops it at once; unknown ids are fine.
    pub fn redis_memory_analysis_stop(&self, session_id: &str, run_id: &str) {
        self.m2.memory_runs.stop(session_id, run_id);
    }

    /// Cluster and Sentinel topology.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_topology(&self, session_id: &str) -> CmdResult<RedisTopology> {
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                Box::pin(async move {
                    Ok(RedisTopology {
                        cluster: live::cluster_topology(session, &server)
                            .await
                            .map_err(catalog_err)?,
                        sentinel: live::sentinel_topology(session, &server)
                            .await
                            .map_err(catalog_err)?,
                    })
                })
            })
            .await?;
        Ok(value)
    }

    /// Function libraries (Redis 7.0 and newer, Valkey).
    ///
    /// # Errors
    /// Unknown or non-Redis session, a server without functions, driver
    /// errors.
    pub async fn redis_functions(
        &self,
        session_id: &str,
        library_pattern: Option<&str>,
        with_code: bool,
    ) -> CmdResult<Vec<FunctionLibrary>> {
        let pattern = library_pattern.map(str::to_string);
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                let pattern = pattern.clone();
                Box::pin(async move {
                    live::function_list(session, &server, pattern.as_deref(), with_code)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await?;
        Ok(value)
    }

    /// Active Pub/Sub channels with subscriber counts.
    ///
    /// # Errors
    /// Unknown or non-Redis session, driver errors.
    pub async fn redis_pubsub(
        &self,
        session_id: &str,
        pattern: Option<&str>,
        max_channels: Option<usize>,
    ) -> CmdResult<PubSubSnapshot> {
        let pattern = pattern.map(str::to_string);
        let max = max_channels.unwrap_or(DEFAULT_PUBSUB_CHANNELS);
        let (_, value) = self
            .redis_admin(session_id, true, |session, server| {
                let pattern = pattern.clone();
                Box::pin(async move {
                    live::pubsub_snapshot(session, &server, pattern.as_deref(), max)
                        .await
                        .map_err(catalog_err)
                })
            })
            .await?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_deserialize_from_the_ui() -> Result<(), serde_json::Error> {
        let set: ConfigRequest = serde_json::from_value(json!({
            "kind": "set", "changes": [{ "name": "maxmemory", "value": "100mb" }]
        }))?;
        assert!(matches!(set, ConfigRequest::Set { ref changes } if changes.len() == 1));
        let rewrite: ConfigRequest = serde_json::from_value(json!({ "kind": "rewrite" }))?;
        assert_eq!(rewrite, ConfigRequest::Rewrite);
        let load: FunctionRequest =
            serde_json::from_value(json!({ "kind": "load", "code": "#!lua name=l\n" }))?;
        assert!(matches!(load, FunctionRequest::Load { replace: false, .. }));
        let flush: FunctionRequest = serde_json::from_value(json!({ "kind": "scriptFlush" }))?;
        assert_eq!(
            flush,
            FunctionRequest::ScriptFlush {
                mode: FlushMode::Default
            }
        );
        Ok(())
    }

    #[test]
    fn plans_name_their_commands() {
        let plan = CommandPlan {
            commands: vec!["CONFIG SET requirepass x".into(), "config rewrite".into()],
            display: vec![
                "CONFIG SET requirepass ********".into(),
                "config rewrite".into(),
            ],
            destructive: true,
            sensitive: true,
            warnings: vec![],
        };
        assert_eq!(command_names(&plan), ["CONFIG"]);
        assert!(!format!("{plan:?}").contains(" x"));
    }

    #[test]
    fn topology_serializes_in_camel_case() -> Result<(), serde_json::Error> {
        let topology = RedisTopology {
            cluster: None,
            sentinel: None,
        };
        assert_eq!(
            serde_json::to_value(&topology)?,
            json!({ "cluster": null, "sentinel": null })
        );
        Ok(())
    }
}
