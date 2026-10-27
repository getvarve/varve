//! MySQL `EXPLAIN FORMAT=JSON` (format versions 1 and 2) and MariaDB
//! `EXPLAIN FORMAT=JSON` / `ANALYZE FORMAT=JSON`.
//!
//! Format version 1 (MySQL 8.0, and 8.3+ with
//! `explain_json_format_version = 1`) and MariaDB nest operations by name:
//! a `query_block` holds a `table`, a `nested_loop` of tables, or wrappers
//! such as `ordering_operation` / `filesort` around them, and subqueries
//! hang under `*_subqueries` lists or `materialized` objects. MariaDB lists
//! the tables of a join as repeated `table` members of one object in older
//! releases. Format version 2 (the 9.x default, and `EXPLAIN ANALYZE
//! FORMAT=JSON`) is a plain tree of `operation` nodes with `inputs`.

use super::analysis::{self, prop};
use super::json::Json;
use super::{ActualTime, Cost, NodeWarning, Plan, PlanFormat, PlanNode, ShareBasis};
use crate::model::{CatalogError, CatalogResult};

fn unexpected(engine: &str, message: &str) -> CatalogError {
    CatalogError::Unexpected {
        message: format!("{engine} plan: {message}"),
    }
}

fn empty_plan(format: PlanFormat, root: PlanNode, raw: &str) -> Plan {
    Plan {
        format,
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
    }
}

/// Parses MySQL JSON, either format version.
pub(crate) fn parse_mysql(raw: &str) -> CatalogResult<Plan> {
    let json =
        Json::parse(raw).map_err(|e| unexpected("MySQL", &format!("the plan is not JSON: {e}")))?;
    if let Some(query_plan) = json.get("query_plan").filter(|p| p.is_object()) {
        let mut root = v2_node(query_plan);
        analysis::settle(&mut root);
        let mut plan = empty_plan(PlanFormat::MysqlJson, root, raw);
        for (key, value) in json.entries() {
            if key != "query_plan" {
                plan.properties.push(prop(key, value.display()));
            }
        }
        analysis::finish(&mut plan);
        return Ok(plan);
    }
    nested_plan(PlanFormat::MysqlJson, "MySQL", &json, raw)
}

/// Parses MariaDB JSON.
pub(crate) fn parse_mariadb(raw: &str) -> CatalogResult<Plan> {
    let json = Json::parse(raw)
        .map_err(|e| unexpected("MariaDB", &format!("the plan is not JSON: {e}")))?;
    nested_plan(PlanFormat::MariadbJson, "MariaDB", &json, raw)
}

/// Format version 1 and MariaDB: the document holds one `query_block`.
fn nested_plan(format: PlanFormat, engine: &str, json: &Json, raw: &str) -> CatalogResult<Plan> {
    let block = json
        .get("query_block")
        .filter(|b| b.is_object())
        .ok_or_else(|| unexpected(engine, "the plan has no \"query_block\""))?;
    let mut root = build("query_block", block);
    analysis::settle(&mut root);
    let mut plan = empty_plan(format, root, raw);
    for (key, value) in json.entries() {
        match key.as_str() {
            "query_block" => {}
            "query_optimization" => {
                plan.planning_ms = value.get("r_total_time_ms").and_then(Json::as_f64);
                for (k, v) in value.entries() {
                    if k != "r_total_time_ms" {
                        plan.properties
                            .push(prop(&format!("query_optimization.{k}"), v.display()));
                    }
                }
            }
            _ => plan.properties.push(prop(key, value.display())),
        }
    }
    analysis::finish(&mut plan);
    Ok(plan)
}

/// Label of a member that is an operation of its own.
fn op_label(key: &str) -> Option<&'static str> {
    Some(match key {
        "query_block" => "Query block",
        "table" => "Table",
        "nested_loop" => "Nested loop",
        "ordering_operation" => "Sort",
        "grouping_operation" => "Group",
        "duplicates_removal" => "Remove duplicates",
        "windowing" | "window_functions_computation" => "Window functions",
        "buffer_result" => "Buffer result",
        "union_result" => "Union",
        "filesort" => "Filesort",
        "temporary_table" => "Temporary table",
        "read_sorted_file" => "Read sorted file",
        "materialized_from_subquery" | "materialized" => "Materialize",
        "block-nl-join" => "Block nested loop join",
        "bka-join" => "Batched key access join",
        "range-checked-for-each-record" => "Range checked for each record",
        "expression_cache" => "Expression cache",
        _ => return None,
    })
}

/// The relationship of the members of a subquery list:
/// `attached_subqueries` becomes `attached subquery`.
fn subquery_relationship(key: &str) -> Option<String> {
    key.strip_suffix("subqueries")
        .map(|prefix| format!("{}subquery", prefix.replace('_', " ")))
}

/// True when a value can hold operations (an object, or an array of
/// objects).
fn holds_operations(value: &Json) -> bool {
    match value {
        Json::Object(_) => true,
        Json::Array(items) => !items.is_empty() && items.iter().all(Json::is_object),
        _ => false,
    }
}

/// Nodes for an operation member. An array (`nested_loop`, MariaDB
/// `duplicates_removal`) becomes one node over its elements.
fn operation(key: &str, value: &Json) -> PlanNode {
    match value {
        Json::Array(items) => PlanNode {
            operation: op_label(key).unwrap_or(key).to_owned(),
            children: items.iter().flat_map(|e| expand(e, None)).collect(),
            ..PlanNode::default()
        },
        _ => build(key, value),
    }
}

/// Nodes for one element of a list: the operations it holds, with its
/// scalar members (`dependent`, `cacheable`, ...) as properties of the
/// first one.
fn expand(element: &Json, relationship: Option<&str>) -> Vec<PlanNode> {
    let mut nodes = Vec::new();
    let mut properties = Vec::new();
    for (key, value) in element.entries() {
        if op_label(key).is_some() && holds_operations(value) {
            nodes.push(operation(key, value));
        } else {
            properties.push(prop(key, value.display()));
        }
    }
    if nodes.is_empty() {
        nodes.push(PlanNode {
            operation: relationship.map_or("Operation", |_| "Subquery").to_owned(),
            ..PlanNode::default()
        });
    }
    if let Some(first) = nodes.first_mut() {
        first.properties.splice(0..0, properties);
    }
    if let Some(relationship) = relationship {
        for node in &mut nodes {
            node.detail.relationship = Some(relationship.to_owned());
        }
    }
    nodes
}

/// Human name of a MySQL / MariaDB access type.
fn access_label(access: &str) -> String {
    match access {
        "ALL" => "Full table scan".into(),
        "index" => "Full index scan".into(),
        "range" => "Index range scan".into(),
        "ref" => "Index lookup".into(),
        "eq_ref" => "Unique index lookup".into(),
        "ref_or_null" => "Index lookup with NULL".into(),
        "fulltext" => "Full-text index lookup".into(),
        "const" => "Constant row".into(),
        "system" => "System row".into(),
        "index_merge" => "Index merge".into(),
        "unique_subquery" => "Unique subquery lookup".into(),
        "index_subquery" => "Index subquery lookup".into(),
        "hash_ALL" => "Hash join over full table scan".into(),
        "hash_index" => "Hash join over full index scan".into(),
        "hash_range" => "Hash join over index range scan".into(),
        other => other.replace('_', " "),
    }
}

/// Reads a format version 1 or MariaDB operation object.
fn build(key: &str, value: &Json) -> PlanNode {
    let mut node = PlanNode {
        operation: op_label(key).unwrap_or(key).to_owned(),
        ..PlanNode::default()
    };
    let mut cost = Cost::default();
    let mut own_cost = None::<f64>;
    let mut time = ActualTime::default();
    let mut own_time = None::<f64>;
    let mut timed = false;
    let mut access = None;
    let mut table_children = Vec::new();
    let mut select_id = None;
    let mut block_operation = None;
    let mut message = None;
    let mut verb = None;
    let mut sort_passes = None;

    for (k, v) in value.entries() {
        if op_label(k).is_some() && holds_operations(v) {
            if k == "table" {
                table_children.push(node.children.len());
            }
            node.children.push(operation(k, v));
            continue;
        }
        if k == "query_specifications" || k == "sorts" {
            node.children
                .extend(v.items().iter().flat_map(|e| expand(e, None)));
            continue;
        }
        if let Some(relationship) = subquery_relationship(k)
            && holds_operations(v)
        {
            node.children.extend(
                v.items()
                    .iter()
                    .flat_map(|e| expand(e, Some(&relationship))),
            );
            continue;
        }
        let d = &mut node.detail;
        match k.as_str() {
            "select_id" => select_id = Some(v.display()),
            "operation" => block_operation = Some(v.display()),
            "message" => message = Some(v.display()),
            "table_name" => d.relation = Some(v.display()),
            "access_type" => access = v.as_str().map(str::to_owned),
            "key" => d.index = Some(v.display()),
            "attached_condition" => d.filter = Some(v.display()),
            "index_condition" => d.condition = Some(v.display()),
            "join_type" => d.join_type = Some(v.display()),
            "sort_key" => d.sort_key = vec![v.display()],
            "insert" | "update" | "delete" if v.as_bool() == Some(true) => {
                verb = Some(k.clone());
            }
            "cost_info" => {
                for (ck, cv) in v.entries() {
                    match ck.as_str() {
                        "query_cost" => cost.total = cv.as_f64(),
                        "read_cost" | "eval_cost" | "sort_cost" => {
                            if let Some(c) = cv.as_f64() {
                                own_cost = Some(own_cost.unwrap_or(0.0) + c);
                            }
                        }
                        _ => {}
                    }
                    node.properties.push(prop(ck, cv.display()));
                }
            }
            // MariaDB 11+: the cost of the node including what it reads.
            "cost" => cost.total = v.as_f64(),
            "rows_examined_per_scan" | "rows" => node.estimated_rows = v.as_f64(),
            "r_rows" => node.actual_rows = v.as_f64(),
            "r_loops" => node.loops = v.as_u64(),
            "r_total_time_ms" => {
                time.total_ms = v.as_f64();
                timed = true;
            }
            "r_table_time_ms" | "r_other_time_ms" => {
                if let Some(t) = v.as_f64() {
                    own_time = Some(own_time.unwrap_or(0.0) + t);
                }
                timed = true;
            }
            _ => {
                if k == "r_sort_passes" {
                    sort_passes = v.as_u64();
                }
                node.properties.push(prop(k, v.display()));
            }
        }
    }

    // MariaDB before 11 lists the tables of a join side by side.
    if table_children.len() > 1 {
        let mut joined = Vec::new();
        for index in table_children.iter().rev() {
            joined.push(node.children.remove(*index));
        }
        joined.reverse();
        let at = table_children[0];
        node.children.insert(
            at,
            PlanNode {
                operation: "Nested loop".into(),
                children: joined,
                ..PlanNode::default()
            },
        );
    }

    match key {
        "query_block" => {
            let mut label = match &select_id {
                Some(id) => format!("Query block #{id}"),
                None => "Query block".to_owned(),
            };
            if let Some(operation) = block_operation {
                label = format!("{label} ({operation})");
            }
            if let Some(message) = message {
                label = format!("{label}: {message}");
            }
            node.operation = label;
        }
        "table" => {
            let table = node.detail.relation.clone().unwrap_or_else(|| "?".into());
            let access_text = access.as_deref().map(access_label);
            node.operation = match (verb.as_deref(), access_text) {
                (Some("insert"), _) => format!("Insert into {table}"),
                (Some(verb), Some(access)) => {
                    let verb = if verb == "update" {
                        "Update"
                    } else {
                        "Delete from"
                    };
                    format!("{verb} {table} ({access})")
                }
                (Some(verb), None) => {
                    let verb = if verb == "update" {
                        "Update"
                    } else {
                        "Delete from"
                    };
                    format!("{verb} {table}")
                }
                (None, Some(access)) => format!("{access} on {table}"),
                (None, None) => format!("Table {table}"),
            };
        }
        _ => {
            if let Some(message) = message {
                node.properties.push(prop("message", message));
            }
            if let Some(id) = select_id {
                node.properties.push(prop("select_id", id));
            }
        }
    }
    if let Some(access) = &access {
        node.properties
            .insert(0, prop("access_type", access.clone()));
    }

    // Costs: the node's own cost (version 1 tables and sorts) or its total.
    if own_cost.is_some() {
        cost.exclusive = own_cost;
    }
    if cost.total.is_some() || cost.exclusive.is_some() {
        node.estimated_cost = Some(cost);
    }
    // Times: MariaDB tables and filesorts report their own time, query
    // blocks and others their total.
    if timed {
        let own = matches!(key, "table" | "filesort");
        if own {
            time.exclusive_ms = own_time.or(time.total_ms);
            time.total_ms = None;
        }
        if let (Some(total), Some(loops)) = (time.total_ms.or(time.exclusive_ms), node.loops)
            && loops > 0
        {
            time.per_loop_ms = Some(total / loops as f64);
        }
        node.actual_time = Some(time);
    }
    if node.loops == Some(0) {
        node.never_executed = true;
    }

    if sort_passes.is_some_and(|p| p > 0) {
        node.warnings.push(NodeWarning::ExternalSort {
            method: "filesort with merge passes".into(),
            space_kb: None,
        });
    }
    if access.as_deref() == Some("ALL")
        && !node
            .detail
            .relation
            .as_deref()
            .is_some_and(|r| r.starts_with('<'))
    {
        let rows = max_of(node.estimated_rows, node.actual_rows);
        if let Some(warning) = analysis::large_scan(node.detail.relation.as_deref(), rows) {
            node.warnings.push(warning);
        }
    }
    node
}

fn max_of(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Reads a format version 2 node.
fn v2_node(value: &Json) -> PlanNode {
    let mut node = PlanNode::default();
    let mut cost = Cost::default();
    let mut time = ActualTime::default();
    let mut timed = false;
    let mut access = None;
    for (k, v) in value.entries() {
        let d = &mut node.detail;
        match k.as_str() {
            "operation" => node.operation = v.display(),
            "inputs" => node.children = v.items().iter().map(v2_node).collect(),
            "table_name" => d.relation = Some(v.display()),
            "schema_name" => d.schema = Some(v.display()),
            "alias" => d.alias = Some(v.display()),
            "index_name" => d.index = Some(v.display()),
            "join_type" => d.join_type = Some(v.display()),
            "join_algorithm" => d.strategy = Some(v.display()),
            "condition" => d.filter = Some(v.display()),
            "lookup_condition" | "hash_condition" => d.condition = Some(v.display()),
            "sort_fields" => d.sort_key = v.strings(),
            "subquery_location" => d.relationship = Some(format!("subquery in {}", v.display())),
            "estimated_rows" => node.estimated_rows = v.as_f64(),
            "estimated_total_cost" => cost.total = v.as_f64(),
            "estimated_first_row_cost" => cost.startup = v.as_f64(),
            "actual_rows" => node.actual_rows = v.as_f64(),
            "actual_loops" => node.loops = v.as_u64(),
            "actual_first_row_ms" => {
                time.first_row_ms = v.as_f64();
                timed = true;
            }
            "actual_last_row_ms" => {
                time.per_loop_ms = v.as_f64();
                timed = true;
            }
            _ => {
                if k == "access_type" {
                    access = v.as_str().map(str::to_owned);
                }
                node.properties.push(prop(k, v.display()));
            }
        }
    }
    if node.operation.is_empty() {
        node.operation = access.clone().unwrap_or_else(|| "Operation".into());
    }
    if cost.total.is_some() || cost.startup.is_some() {
        node.estimated_cost = Some(cost);
    }
    if node.loops == Some(0) {
        node.never_executed = true;
    }
    if timed {
        time.total_ms = time
            .per_loop_ms
            .map(|t| t * node.loops.map_or(1.0, |l| l as f64));
        node.actual_time = Some(time);
    }
    if access.as_deref() == Some("table")
        && node.operation.starts_with("Table scan")
        && !node
            .detail
            .relation
            .as_deref()
            .is_some_and(|r| r.starts_with('<'))
    {
        let rows = max_of(node.estimated_rows, node.actual_rows);
        if let Some(warning) = analysis::large_scan(node.detail.relation.as_deref(), rows) {
            node.warnings.push(warning);
        }
    }
    node
}
