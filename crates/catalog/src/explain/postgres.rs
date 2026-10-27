//! PostgreSQL `EXPLAIN (FORMAT JSON)`.
//!
//! The output is an array with one object: `Plan` (the node tree, children
//! under `Plans`), `Planning Time`, `Execution Time`, `Triggers`,
//! `Planning` (buffers), `Settings` and `JIT`. Times and rows of a node are
//! averages per loop; nodes below a `Gather` run in every worker and the
//! leader, so their loops count processes.

use super::TriggerTime;
use super::analysis::{self, prop};
use super::json::Json;
use super::{ActualTime, Buffers, Cost, NodeWarning, Plan, PlanFormat, PlanNode, ShareBasis};
use crate::model::{CatalogError, CatalogResult};

/// Parses a PostgreSQL JSON plan.
pub(crate) fn parse(raw: &str) -> CatalogResult<Plan> {
    let json = Json::parse(raw).map_err(|e| unexpected(&format!("the plan is not JSON: {e}")))?;
    let top = match &json {
        Json::Array(items) => items
            .first()
            .ok_or_else(|| unexpected("the plan array is empty"))?,
        Json::Object(_) => &json,
        _ => return Err(unexpected("the plan is not a JSON array")),
    };
    let plan_json = top
        .get("Plan")
        .filter(|p| p.is_object())
        .ok_or_else(|| unexpected("the plan has no \"Plan\" object"))?;
    let mut root = node(plan_json, 1.0);
    attribute_cte_time(&mut root);
    analysis::settle(&mut root);

    let mut plan = Plan {
        format: PlanFormat::PostgresJson,
        analyzed: false,
        root,
        planning_ms: None,
        execution_ms: None,
        total_cost: None,
        share_basis: ShareBasis::None,
        planning_buffers: None,
        triggers: Vec::new(),
        settings: Vec::new(),
        properties: Vec::new(),
        warnings: Vec::new(),
        notices: Vec::new(),
        raw: raw.to_owned(),
    };
    for (key, value) in top.entries() {
        match key.as_str() {
            "Plan" => {}
            "Planning Time" => plan.planning_ms = value.as_f64(),
            "Execution Time" => plan.execution_ms = value.as_f64(),
            "Planning" => plan.planning_buffers = buffers(value),
            "Triggers" => plan.triggers = value.items().iter().map(trigger).collect(),
            "Settings" => {
                plan.settings = value
                    .entries()
                    .iter()
                    .map(|(k, v)| prop(k, v.display()))
                    .collect();
            }
            _ => plan.properties.push(prop(key, value.display())),
        }
    }
    analysis::finish(&mut plan);
    Ok(plan)
}

fn unexpected(message: &str) -> CatalogError {
    CatalogError::Unexpected {
        message: format!("PostgreSQL plan: {message}"),
    }
}

fn trigger(value: &Json) -> TriggerTime {
    let name = value
        .get("Trigger Name")
        .or_else(|| value.get("Constraint Name"))
        .map(Json::display)
        .unwrap_or_default();
    TriggerTime {
        name,
        relation: value.get("Relation").map(Json::display),
        time_ms: value.get("Time").and_then(Json::as_f64),
        calls: value.get("Calls").and_then(Json::as_u64),
    }
}

/// Buffer counters of a node or of `Planning`; `None` without any.
fn buffers(value: &Json) -> Option<Buffers> {
    let mut buffers = Buffers::default();
    let mut any = false;
    for (key, v) in value.entries() {
        let Some(slot) = buffer_slot(&mut buffers, key) else {
            continue;
        };
        *slot = v.as_u64().unwrap_or(0);
        any = true;
    }
    any.then_some(buffers)
}

fn buffer_slot<'a>(buffers: &'a mut Buffers, key: &str) -> Option<&'a mut u64> {
    Some(match key {
        "Shared Hit Blocks" => &mut buffers.shared_hit,
        "Shared Read Blocks" => &mut buffers.shared_read,
        "Shared Dirtied Blocks" => &mut buffers.shared_dirtied,
        "Shared Written Blocks" => &mut buffers.shared_written,
        "Local Hit Blocks" => &mut buffers.local_hit,
        "Local Read Blocks" => &mut buffers.local_read,
        "Local Dirtied Blocks" => &mut buffers.local_dirtied,
        "Local Written Blocks" => &mut buffers.local_written,
        "Temp Read Blocks" => &mut buffers.temp_read,
        "Temp Written Blocks" => &mut buffers.temp_written,
        _ => return None,
    })
}

/// The label PostgreSQL's text format shows for a node.
fn label(value: &Json) -> String {
    let node_type = value.get("Node Type").and_then(Json::as_str).unwrap_or("?");
    let strategy = value.get("Strategy").and_then(Json::as_str);
    let mut name = match (node_type, strategy) {
        ("Aggregate", Some("Sorted")) => "GroupAggregate".to_owned(),
        ("Aggregate", Some("Hashed")) => "HashAggregate".to_owned(),
        ("Aggregate", Some("Mixed")) => "MixedAggregate".to_owned(),
        ("SetOp", Some("Hashed")) => "HashSetOp".to_owned(),
        ("ModifyTable", _) => value
            .get("Operation")
            .and_then(Json::as_str)
            .unwrap_or(node_type)
            .to_owned(),
        _ => node_type.to_owned(),
    };
    match value.get("Partial Mode").and_then(Json::as_str) {
        Some("Partial") => name = format!("Partial {name}"),
        Some("Finalize") => name = format!("Finalize {name}"),
        _ => {}
    }
    if value
        .get("Parallel Aware")
        .and_then(Json::as_bool)
        .unwrap_or(false)
    {
        name = format!("Parallel {name}");
    }
    if value.get("Async Capable").and_then(Json::as_bool) == Some(true) && node_type != "Append" {
        name = format!("Async {name}");
    }
    name
}

/// Reads a node. `processes` is the number of processes running it
/// (workers plus leader below a `Gather`), used to turn summed loop times
/// into wall time.
fn node(value: &Json, processes: f64) -> PlanNode {
    let mut node = PlanNode {
        operation: label(value),
        ..PlanNode::default()
    };
    let node_type = value.get("Node Type").and_then(Json::as_str).unwrap_or("");
    let gather = matches!(node_type, "Gather" | "Gather Merge");
    let workers = value
        .get("Workers Launched")
        .or_else(|| value.get("Workers Planned"))
        .and_then(Json::as_f64);
    let child_processes = if gather {
        // The leader takes part unless parallel_leader_participation is
        // off; PostgreSQL does not report that, so it is assumed on.
        workers.map_or(processes, |w| w + 1.0)
    } else {
        processes
    };

    let mut cost = Cost::default();
    let mut time = ActualTime::default();
    let mut timed = false;
    let mut node_buffers = Buffers::default();
    let mut has_buffers = false;
    let mut rows_removed = 0.0;
    let mut sort_method = None;
    let mut sort_space = None;
    let mut sort_disk = false;
    let mut hash_batches = None;
    let mut disk_usage = None;

    for (key, v) in value.entries() {
        if let Some(slot) = buffer_slot(&mut node_buffers, key) {
            *slot = v.as_u64().unwrap_or(0);
            has_buffers = true;
            continue;
        }
        let d = &mut node.detail;
        match key.as_str() {
            "Plans" => {
                node.children = v
                    .items()
                    .iter()
                    .map(|c| self::node(c, child_processes))
                    .collect();
            }
            "Node Type" | "Strategy" | "Partial Mode" | "Operation" | "Async Capable" => {
                node.properties.push(prop(key, v.display()));
            }
            "Parallel Aware" => d.parallel_aware = v.as_bool().unwrap_or(false),
            "Parent Relationship" => d.relationship = Some(v.display()),
            "Subplan Name" => d.subplan = Some(v.display()),
            "CTE Name" => d.cte = Some(v.display()),
            "Relation Name" => d.relation = Some(v.display()),
            "Schema" => d.schema = Some(v.display()),
            "Alias" => d.alias = Some(v.display()),
            "Index Name" => d.index = Some(v.display()),
            "Join Type" => d.join_type = Some(v.display()),
            "Function Name" => d.function = Some(v.display()),
            "Filter" => d.filter = Some(v.display()),
            "Hash Cond" | "Merge Cond" | "Index Cond" | "TID Cond" => {
                d.condition = Some(v.display());
            }
            "Sort Key" => d.sort_key = v.strings(),
            "Group Key" => d.group_key = v.strings(),
            "Output" => d.output = v.strings(),
            "Workers Planned" => d.workers_planned = v.as_u64(),
            "Workers Launched" => d.workers_launched = v.as_u64(),
            "Startup Cost" => cost.startup = v.as_f64(),
            "Total Cost" => cost.total = v.as_f64(),
            "Plan Rows" => node.estimated_rows = v.as_f64(),
            "Actual Startup Time" => {
                time.first_row_ms = v.as_f64();
                timed = true;
            }
            "Actual Total Time" => {
                time.per_loop_ms = v.as_f64();
                timed = true;
            }
            "Actual Rows" => node.actual_rows = v.as_f64(),
            "Actual Loops" => node.loops = v.as_u64(),
            _ => {
                match key.as_str() {
                    "Rows Removed by Filter" => rows_removed = v.as_f64().unwrap_or(0.0),
                    "Sort Method" => sort_method = v.as_str().map(str::to_owned),
                    "Sort Space Used" => sort_space = v.as_u64(),
                    "Sort Space Type" => sort_disk = v.as_str() == Some("Disk"),
                    "Hash Batches" | "HashAgg Batches" => hash_batches = v.as_u64(),
                    "Disk Usage" => disk_usage = v.as_u64(),
                    _ => {}
                }
                node.properties.push(prop(key, v.display()));
            }
        }
    }

    if cost.startup.is_some() || cost.total.is_some() {
        node.estimated_cost = Some(cost);
    }
    if has_buffers {
        node.buffers = Some(node_buffers);
    }
    let loops = node.loops.map(|l| l as f64);
    if node.loops == Some(0) {
        node.never_executed = true;
    }
    if timed {
        // Per-loop time times loops is the summed time of every process;
        // spread over the processes it is wall time.
        time.total_ms = match (time.per_loop_ms, loops) {
            (Some(per_loop), Some(loops)) => Some(per_loop * loops / processes),
            (Some(per_loop), None) => Some(per_loop),
            _ => None,
        };
        node.actual_time = Some(time);
    }

    // Warnings this engine can tell.
    if let Some(method) = sort_method
        && (method.contains("external") || sort_disk)
    {
        node.warnings.push(NodeWarning::ExternalSort {
            method,
            space_kb: sort_space,
        });
    }
    if hash_batches.is_some_and(|b| b > 1) || disk_usage.is_some_and(|d| d > 0) {
        node.warnings.push(NodeWarning::DiskSpill {
            batches: hash_batches,
            disk_kb: disk_usage,
        });
    }
    if node_type == "Seq Scan" {
        // Rows read per scan: returned plus filtered out. A parallel scan
        // splits one table over its processes.
        let per_scan = match node.actual_rows {
            Some(actual) => Some(actual + rows_removed),
            None => node.estimated_rows,
        };
        let spread = if node.detail.parallel_aware {
            loops.unwrap_or(processes)
        } else {
            1.0
        };
        if let Some(warning) = analysis::large_scan(
            node.detail.relation.as_deref(),
            per_scan.map(|r| r * spread),
        ) {
            node.warnings.push(warning);
        }
    }
    node
}

/// A materialized CTE runs inside the `CTE Scan` that first reads it, so
/// its time shows both in its `InitPlan` and in that scan and every node
/// above the scan. The `InitPlan` keeps it; the scans and the nodes between
/// them and the node holding the `InitPlan` lose it. Runs before
/// [`analysis::settle`] derives exclusive times from the totals.
fn attribute_cte_time(node: &mut PlanNode) {
    let ctes: Vec<(String, f64)> = node
        .children
        .iter()
        .filter_map(|child| {
            let name = child.detail.subplan.as_deref()?.strip_prefix("CTE ")?;
            let total = child.actual_time.and_then(|t| t.total_ms)?;
            Some((name.to_owned(), total))
        })
        .collect();
    for (name, total) in ctes {
        let mut remaining = total;
        let own_subplan = format!("CTE {name}");
        for child in &mut node.children {
            if child.detail.subplan.as_deref() != Some(own_subplan.as_str()) {
                drain_cte(child, &name, &mut remaining);
            }
        }
    }
    for child in &mut node.children {
        attribute_cte_time(child);
    }
}

/// Takes up to `remaining` of the CTE's time out of the scans of `name` in
/// this subtree and out of the totals above them; returns what was taken.
fn drain_cte(node: &mut PlanNode, name: &str, remaining: &mut f64) -> f64 {
    let mut taken = 0.0;
    for child in &mut node.children {
        taken += drain_cte(child, name, remaining);
    }
    let total = node.actual_time.and_then(|t| t.total_ms);
    if node.detail.cte.as_deref() == Some(name)
        && let Some(total) = total
    {
        // The scan's own total is what it can give, less what its
        // children (none for a CTE Scan) already gave.
        let take = remaining.min((total - taken).max(0.0));
        *remaining -= take;
        taken += take;
    }
    if taken > 0.0
        && let Some(time) = node.actual_time.as_mut()
        && let Some(total) = time.total_ms
    {
        time.total_ms = Some((total - taken).max(0.0));
    }
    taken
}
