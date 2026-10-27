//! Unit tests over plans captured from the M2 database matrix (PostgreSQL
//! 13 and 18, MySQL 8.0 and 9.7, MariaDB 10.6 and 12.3), kept as fixture
//! constants.

use varve_core::Engine;
use varve_driver::{Dialect, ServerInfo, parse_version};

use super::*;
use crate::model::CatalogError;

const PG13_COMPLEX: &str = include_str!("fixtures/pg13_complex.json");
const PG18_COMPLEX: &str = include_str!("fixtures/pg18_complex.json");
const PG13_PARALLEL: &str = include_str!("fixtures/pg13_parallel.json");
const PG18_PARALLEL: &str = include_str!("fixtures/pg18_parallel.json");
const PG13_UPDATE: &str = include_str!("fixtures/pg13_update.json");
const PG18_UPDATE: &str = include_str!("fixtures/pg18_update.json");
const PG13_ESTIMATE: &str = include_str!("fixtures/pg13_estimate.json");
const PG18_ESTIMATE: &str = include_str!("fixtures/pg18_estimate.json");

const MY80_JOIN: &str = include_str!("fixtures/my80_join.json");
const MY80_UNION: &str = include_str!("fixtures/my80_union.json");
const MY80_UPDATE: &str = include_str!("fixtures/my80_update.json");
const MY80_DEPENDENT_SUBQUERY: &str = include_str!("fixtures/my80_dependent_subquery.json");
const MY80_JOIN_TREE: &str = include_str!("fixtures/my80_join_tree.txt");
const MY80_JOIN_ANALYZE: &str = include_str!("fixtures/my80_join_analyze.txt");
const MY80_NEVER_EXECUTED: &str = include_str!("fixtures/my80_never_executed_analyze.txt");
const MY80_UPDATE_ANALYZE: &str = include_str!("fixtures/my80_update_analyze.txt");
const MY80_INSERT_SELECT_ANALYZE: &str = include_str!("fixtures/my80_insert_select_analyze.txt");
const MY97_JOIN_V2: &str = include_str!("fixtures/my97_join_v2.json");
const MY97_JOIN_ANALYZE_V2: &str = include_str!("fixtures/my97_join_analyze_v2.json");
const MY97_UNION_V2: &str = include_str!("fixtures/my97_union_v2.json");
const MY97_JOIN_ANALYZE: &str = include_str!("fixtures/my97_join_analyze.txt");
const MY97_RECURSIVE_CTE_TREE: &str = include_str!("fixtures/my97_recursive_cte_tree.txt");
const MY97_HAVING_ANALYZE: &str = include_str!("fixtures/my97_having_analyze.txt");

const MA106_JOIN: &str = include_str!("fixtures/ma106_join.json");
const MA106_JOIN_ANALYZE: &str = include_str!("fixtures/ma106_join_analyze.json");
const MA106_UNION_ANALYZE: &str = include_str!("fixtures/ma106_union_analyze.json");
const MA106_UPDATE_ANALYZE: &str = include_str!("fixtures/ma106_update_analyze.json");
const MA123_JOIN: &str = include_str!("fixtures/ma123_join.json");
const MA123_JOIN_ANALYZE: &str = include_str!("fixtures/ma123_join_analyze.json");
const MA123_UNION_ANALYZE: &str = include_str!("fixtures/ma123_union_analyze.json");
const MA123_UPDATE_ANALYZE: &str = include_str!("fixtures/ma123_update_analyze.json");

type TestResult = Result<(), CatalogError>;

fn server(engine: Engine, version: &str) -> ServerInfo {
    let (major, minor, patch) = parse_version(version).unwrap_or((0, 0, None));
    ServerInfo {
        engine,
        version: version.to_owned(),
        major,
        minor,
        patch,
    }
}

fn nodes(node: &PlanNode) -> Vec<&PlanNode> {
    let mut out = vec![node];
    for child in &node.children {
        out.extend(nodes(child));
    }
    out
}

fn find<'a>(plan: &'a Plan, operation: &str) -> Result<&'a PlanNode, CatalogError> {
    nodes(&plan.root)
        .into_iter()
        .find(|n| n.operation == operation)
        .ok_or_else(|| CatalogError::NotFound {
            message: format!("no node {operation:?} in {:?}", operations(plan)),
        })
}

fn find_prefix<'a>(plan: &'a Plan, prefix: &str) -> Result<&'a PlanNode, CatalogError> {
    nodes(&plan.root)
        .into_iter()
        .find(|n| n.operation.starts_with(prefix))
        .ok_or_else(|| CatalogError::NotFound {
            message: format!("no node starting with {prefix:?} in {:?}", operations(plan)),
        })
}

fn operations(plan: &Plan) -> Vec<String> {
    nodes(&plan.root)
        .into_iter()
        .map(|n| n.operation.clone())
        .collect()
}

fn property<'a>(node: &'a PlanNode, name: &str) -> Option<&'a str> {
    node.properties
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.value.as_str())
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-6)
}

/// Invariants every parsed plan keeps: ids are a depth-first numbering,
/// exclusive values are never negative, shares add up to 1.
fn check_invariants(plan: &Plan) {
    let all = nodes(&plan.root);
    for (i, node) in all.iter().enumerate() {
        assert_eq!(node.id as usize, i, "ids are depth-first");
        if let Some(t) = node.actual_time.and_then(|t| t.exclusive_ms) {
            assert!(t >= 0.0, "negative exclusive time in {}", node.operation);
        }
        if let Some(c) = node.estimated_cost.and_then(|c| c.exclusive) {
            assert!(c >= 0.0, "negative exclusive cost in {}", node.operation);
        }
    }
    if plan.share_basis != ShareBasis::None {
        let total: f64 = all.iter().filter_map(|n| n.share).sum();
        assert!((total - 1.0).abs() < 1e-6, "shares add up to {total}");
    }
    assert!(!plan.raw.is_empty());
}

// PostgreSQL

#[test]
fn postgres_complex_plans() -> TestResult {
    for raw in [PG13_COMPLEX, PG18_COMPLEX] {
        let plan = parse_plan(PlanFormat::PostgresJson, raw)?;
        check_invariants(&plan);
        assert!(plan.analyzed);
        assert_eq!(plan.share_basis, ShareBasis::Time);
        assert!(plan.planning_ms.is_some_and(|t| t > 0.0));
        assert!(plan.execution_ms.is_some_and(|t| t > 0.0));
        assert!(plan.total_cost.is_some());
        assert!(!plan.settings.is_empty(), "SETTINGS were asked for");
        assert!(plan.planning_buffers.is_some());
        assert!(plan.properties.iter().any(|p| p.name == "JIT"));
        assert_eq!(plan.root.operation, "Sort");

        // The materialized CTE, its scan, the init plan and the correlated
        // subplan.
        let all = nodes(&plan.root);
        let cte = all
            .iter()
            .find(|n| n.detail.subplan.as_deref() == Some("CTE big"))
            .ok_or_else(|| CatalogError::NotFound {
                message: "CTE init plan".into(),
            })?;
        assert_eq!(cte.detail.relationship.as_deref(), Some("InitPlan"));
        let scan = find(&plan, "CTE Scan")?;
        assert_eq!(scan.detail.cte.as_deref(), Some("big"));
        let scan_time = scan.actual_time.unwrap_or_default();
        assert!(scan_time.exclusive_ms <= scan_time.total_ms);
        assert!(
            all.iter()
                .any(|n| n.detail.relationship.as_deref() == Some("SubPlan"))
        );
        assert!(all.iter().any(|n| {
            n.detail.relationship.as_deref() == Some("InitPlan")
                && n.detail
                    .subplan
                    .as_deref()
                    .is_some_and(|s| s.starts_with("InitPlan"))
        }));

        // Parallel workers below a Gather.
        let gather = find(&plan, "Gather")?;
        assert!(gather.detail.workers_planned.is_some());
        assert!(gather.detail.workers_launched.is_some());

        // VERBOSE output and buffers on nodes.
        assert!(!plan.root.detail.output.is_empty());
        assert!(plan.root.buffers.is_some_and(|b| b.shared_hit > 0));
        let index_scan = find(&plan, "Index Scan")?;
        assert_eq!(index_scan.detail.schema.as_deref(), Some("m2_explain_cap"));
        assert!(index_scan.detail.index.is_some());
        assert!(
            all.iter()
                .any(|n| n.operation == "Index Scan" && n.detail.condition.is_some())
        );

        let sort = &plan.root;
        assert_eq!(sort.detail.sort_key.len(), 1);
        assert!(sort.estimated_cost.is_some_and(|c| c.startup.is_some()));
    }
    Ok(())
}

#[test]
fn postgres_parallel_external_sort() -> TestResult {
    for raw in [PG13_PARALLEL, PG18_PARALLEL] {
        let plan = parse_plan(PlanFormat::PostgresJson, raw)?;
        check_invariants(&plan);
        assert_eq!(plan.root.operation, "Gather Merge");
        let launched = plan.root.detail.workers_launched.unwrap_or(0);
        assert!(launched >= 1);

        let sort = find(&plan, "Sort")?;
        assert!(sort.warnings.iter().any(|w| matches!(
            w,
            NodeWarning::ExternalSort { method, space_kb: Some(_) } if method.contains("external")
        )));
        assert!(sort.buffers.is_some_and(|b| b.temp_written > 0));
        assert!(property(sort, "Workers").is_some());
        assert!(sort.detail.sort_key.contains(&"created".to_owned()));

        // The parallel scan reads the 300 000 row table across processes.
        let scan = find(&plan, "Parallel Seq Scan")?;
        assert!(scan.detail.parallel_aware);
        assert!(scan.warnings.iter().any(|w| matches!(
            w,
            NodeWarning::SeqScanLargeTable { relation: Some(r), rows } if r == "orders" && *rows >= 299_000.0
        )));
        // Wall time below the Gather: per-loop time times loops divided by
        // the processes.
        let time = scan.actual_time.unwrap_or_default();
        let loops = scan.loops.unwrap_or(0) as f64;
        let processes = launched as f64 + 1.0;
        assert!(close(
            time.total_ms,
            time.per_loop_ms.unwrap_or(0.0) * loops / processes
        ));
    }
    Ok(())
}

#[test]
fn postgres_update_with_trigger_and_wal() -> TestResult {
    for raw in [PG13_UPDATE, PG18_UPDATE] {
        let plan = parse_plan(PlanFormat::PostgresJson, raw)?;
        check_invariants(&plan);
        assert_eq!(plan.root.operation, "Update");
        assert_eq!(plan.root.detail.relation.as_deref(), Some("orders"));
        assert_eq!(plan.triggers.len(), 1);
        let trigger = &plan.triggers[0];
        assert_eq!(trigger.name, "orders_audit");
        assert_eq!(trigger.relation.as_deref(), Some("orders"));
        assert_eq!(trigger.calls, Some(150));
        assert!(trigger.time_ms.is_some());
        assert!(property(&plan.root, "WAL Records").is_some());
        assert!(plan.root.buffers.is_some_and(|b| b.shared_dirtied > 0));
    }
    Ok(())
}

#[test]
fn postgres_estimates_use_cost_shares() -> TestResult {
    for raw in [PG13_ESTIMATE, PG18_ESTIMATE] {
        let plan = parse_plan(PlanFormat::PostgresJson, raw)?;
        check_invariants(&plan);
        assert!(!plan.analyzed);
        assert_eq!(plan.share_basis, ShareBasis::Cost);
        assert_eq!(plan.root.operation, "Append");
        assert!(plan.execution_ms.is_none());
        assert!(nodes(&plan.root).iter().all(|n| n.actual_time.is_none()));
        let join = find_prefix(&plan, "Hash")?;
        assert!(join.estimated_rows.is_some());
        assert!(
            nodes(&plan.root)
                .iter()
                .any(|n| n.detail.join_type.as_deref() == Some("Right")
                    || n.detail.join_type.as_deref() == Some("Left"))
        );
    }
    Ok(())
}

#[test]
fn postgres_rejects_garbage() {
    assert!(parse_plan(PlanFormat::PostgresJson, "not json").is_err());
    assert!(parse_plan(PlanFormat::PostgresJson, "[]").is_err());
    assert!(parse_plan(PlanFormat::PostgresJson, r#"[{"Planning Time": 1}]"#).is_err());
}

// MySQL format version 1

#[test]
fn mysql_v1_join_with_materialized_subquery() -> TestResult {
    let plan = parse_plan(PlanFormat::MysqlJson, MY80_JOIN)?;
    check_invariants(&plan);
    assert!(!plan.analyzed);
    assert_eq!(plan.share_basis, ShareBasis::Cost);
    assert!(close(plan.total_cost, 1155.44));
    assert_eq!(plan.root.operation, "Query block #1");
    assert_eq!(
        operations(&plan),
        vec![
            "Query block #1",
            "Sort",
            "Group",
            "Nested loop",
            "Full table scan on o",
            "Unique index lookup on c",
            "Unique index lookup on <subquery2>",
            "Materialize",
            "Query block",
            "Full table scan on orders",
        ]
    );
    let scan = find(&plan, "Full table scan on o")?;
    assert_eq!(scan.estimated_rows, Some(3000.0));
    assert!(scan.detail.filter.is_some());
    assert!(close(
        scan.estimated_cost.and_then(|c| c.exclusive),
        202.26 + 99.99
    ));
    let lookup = find(&plan, "Unique index lookup on c")?;
    assert_eq!(lookup.detail.index.as_deref(), Some("PRIMARY"));
    assert_eq!(property(lookup, "prefix_cost"), Some("652.21"));
    Ok(())
}

#[test]
fn mysql_v1_union_and_subqueries() -> TestResult {
    let plan = parse_plan(PlanFormat::MysqlJson, MY80_UNION)?;
    check_invariants(&plan);
    let union = find(&plan, "Union")?;
    assert_eq!(union.detail.relation.as_deref(), Some("<union1,2>"));
    let blocks: Vec<_> = union
        .children
        .iter()
        .map(|c| c.operation.as_str())
        .collect();
    assert_eq!(blocks, vec!["Query block #1", "Query block #2"]);
    assert_eq!(property(&union.children[0], "cacheable"), Some("true"));

    let plan = parse_plan(PlanFormat::MysqlJson, MY80_DEPENDENT_SUBQUERY)?;
    check_invariants(&plan);
    let sub = find(&plan, "Query block #2")?;
    assert_eq!(
        sub.detail.relationship.as_deref(),
        Some("select list subquery")
    );
    assert_eq!(property(sub, "dependent"), Some("true"));

    let plan = parse_plan(PlanFormat::MysqlJson, MY80_UPDATE)?;
    check_invariants(&plan);
    find(&plan, "Update orders (Index range scan)")?;
    Ok(())
}

// MySQL format version 2

#[test]
fn mysql_v2_estimate_and_analyze() -> TestResult {
    let plan = parse_plan(PlanFormat::MysqlJson, MY97_JOIN_V2)?;
    check_invariants(&plan);
    assert!(!plan.analyzed);
    assert_eq!(plan.share_basis, ShareBasis::Cost);
    assert_eq!(plan.root.operation, "Sort: total DESC");
    assert!(plan.properties.iter().any(|p| p.name == "query"));
    let scan = find(&plan, "Table scan on o")?;
    assert_eq!(scan.detail.relation.as_deref(), Some("orders"));
    assert_eq!(scan.detail.schema.as_deref(), Some("m2_explain_cap"));
    assert_eq!(scan.detail.alias.as_deref(), Some("o"));

    let plan = parse_plan(PlanFormat::MysqlJson, MY97_JOIN_ANALYZE_V2)?;
    check_invariants(&plan);
    assert!(plan.analyzed);
    assert_eq!(plan.share_basis, ShareBasis::Time);
    let lookup = find_prefix(&plan, "Single-row index lookup on c")?;
    assert_eq!(lookup.loops, Some(2400));
    let time = lookup.actual_time.unwrap_or_default();
    assert!(close(
        time.total_ms,
        time.per_loop_ms.unwrap_or(0.0) * 2400.0
    ));
    let join = find_prefix(&plan, "Nested loop inner join")?;
    assert_eq!(join.detail.join_type.as_deref(), Some("inner join"));
    assert!(plan.execution_ms.is_some());
    // The outer join estimated almost a million rows and got 2155.
    assert!(nodes(&plan.root).iter().any(|n| {
        n.warnings
            .iter()
            .any(|w| matches!(w, NodeWarning::RowMisestimate { .. }))
    }));

    let plan = parse_plan(PlanFormat::MysqlJson, MY97_UNION_V2)?;
    check_invariants(&plan);
    assert!(nodes(&plan.root).len() > 4);
    Ok(())
}

// MySQL tree

#[test]
fn mysql_tree_estimate() -> TestResult {
    let plan = parse_plan(PlanFormat::MysqlTree, MY80_JOIN_TREE)?;
    check_invariants(&plan);
    assert!(!plan.analyzed);
    assert_eq!(plan.share_basis, ShareBasis::Cost);
    assert_eq!(plan.root.operation, "Sort: total DESC");
    assert_eq!(plan.root.detail.sort_key, vec!["total DESC"]);
    let outer = find(&plan, "Nested loop inner join")?;
    assert_eq!(outer.children.len(), 2);
    assert_eq!(outer.estimated_rows, Some(999_800.0));
    assert_eq!(outer.detail.join_type.as_deref(), Some("Inner"));
    assert_eq!(outer.detail.strategy.as_deref(), Some("nested loop"));
    let lookup = find_prefix(&plan, "Single-row index lookup on <subquery2>")?;
    assert_eq!(lookup.detail.relation.as_deref(), Some("<subquery2>"));
    assert_eq!(lookup.detail.index.as_deref(), Some("<auto_distinct_key>"));
    assert_eq!(lookup.estimated_cost.and_then(|c| c.startup), Some(402.0));
    let filter = find(&plan, "Filter: (o.created > DATE'2026-03-01')")?;
    assert_eq!(
        filter.detail.filter.as_deref(),
        Some("(o.created > DATE'2026-03-01')")
    );

    let plan = parse_plan(PlanFormat::MysqlTree, MY97_RECURSIVE_CTE_TREE)?;
    check_invariants(&plan);
    let materialize = find(&plan, "Materialize recursive CTE s")?;
    assert_eq!(materialize.children.len(), 2);
    assert_eq!(
        materialize.children[1].operation,
        "Repeat until convergence"
    );
    find(&plan, "Nested loop left join")
        .map(|n| assert_eq!(n.detail.join_type.as_deref(), Some("Left")))?;
    Ok(())
}

#[test]
fn mysql_tree_analyze() -> TestResult {
    for raw in [MY80_JOIN_ANALYZE, MY97_JOIN_ANALYZE] {
        let plan = parse_plan(PlanFormat::MysqlTree, raw)?;
        check_invariants(&plan);
        assert!(plan.analyzed);
        assert_eq!(plan.share_basis, ShareBasis::Time);
        let lookup = find_prefix(&plan, "Single-row index lookup on c")?;
        assert_eq!(lookup.loops, Some(2400));
        assert_eq!(lookup.actual_rows, Some(1.0));
        let time = lookup.actual_time.unwrap_or_default();
        assert!(time.first_row_ms.is_some_and(|t| t < 0.001));
        assert!(close(
            time.total_ms,
            time.per_loop_ms.unwrap_or(0.0) * 2400.0
        ));
        assert!(close(
            plan.execution_ms,
            plan.root
                .actual_time
                .and_then(|t| t.per_loop_ms)
                .unwrap_or(-1.0)
        ));
        let outer = find(&plan, "Nested loop inner join")?;
        assert!(outer.warnings.iter().any(|w| matches!(
            w,
            NodeWarning::RowMisestimate { estimated, actual, .. }
                if *estimated == 999_800.0 && *actual == 2155.0
        )));
        let fractional = find_prefix(&plan, "Single-row index lookup on <subquery2>")?;
        assert_eq!(fractional.actual_rows, Some(0.898));
    }

    let plan = parse_plan(PlanFormat::MysqlTree, MY80_NEVER_EXECUTED)?;
    check_invariants(&plan);
    let skipped = find(&plan, "Limit: 1 row(s)")?;
    assert!(nodes(&plan.root).iter().any(|n| n.never_executed));
    assert!(skipped.loops.is_some());
    let select = find(&plan, "Select #3 (subquery in condition; dependent)")?;
    assert_eq!(select.detail.relationship.as_deref(), Some("subquery"));
    assert!(select.children.iter().all(|c| c.never_executed));
    let lookup = find_prefix(&plan, "Index lookup on <materialized_subquery>")?;
    // No cost of its own in the text; the total comes from its child.
    assert!(lookup.estimated_cost.is_some_and(|c| c.startup.is_none()));
    assert_eq!(lookup.actual_rows, Some(0.0));

    let raw = "-> Sort: total DESC\n    -> Table scan on c  (cost=20.2 rows=200)\n-> Select #2 (subquery in projection; run only once)\n    -> Aggregate: max(orders.amount)  (cost=602 rows=1)\n";
    let plan = parse_plan(PlanFormat::MysqlTree, raw)?;
    check_invariants(&plan);
    assert_eq!(plan.root.operation, "Sort: total DESC");
    assert_eq!(plan.root.children.len(), 2);
    assert_eq!(
        plan.root.children[1].detail.relationship.as_deref(),
        Some("subquery")
    );

    let plan = parse_plan(PlanFormat::MysqlTree, MY97_HAVING_ANALYZE)?;
    check_invariants(&plan);
    assert_eq!(plan.root.operation, "Limit: 1 row(s)");
    find(&plan, "Sort: `COUNT(*)` DESC")?;
    Ok(())
}

#[test]
fn mysql_tree_dml() -> TestResult {
    let plan = parse_plan(PlanFormat::MysqlTree, MY80_UPDATE_ANALYZE)?;
    assert_eq!(plan.root.operation, "<not executable by iterator executor>");
    assert!(!plan.analyzed);
    assert_eq!(plan.warnings.len(), 1);

    let plan = parse_plan(PlanFormat::MysqlTree, MY80_INSERT_SELECT_ANALYZE)?;
    check_invariants(&plan);
    assert!(plan.analyzed);
    assert_eq!(plan.root.operation, "Insert into orders");
    let range = find_prefix(&plan, "Index range scan on orders")?;
    assert_eq!(range.detail.index.as_deref(), Some("PRIMARY"));
    assert_eq!(range.detail.condition.as_deref(), Some("id < 5"));
    Ok(())
}

#[test]
fn mysql_tree_continuation_lines_and_garbage() -> TestResult {
    let raw = "-> Filter: (t.a = 'x\ny')  (cost=1.5 rows=2) (actual time=0.1..0.2 rows=1 loops=1)\n    -> Table scan on t  (cost=1.5 rows=200000)\n";
    let plan = parse_plan(PlanFormat::MysqlTree, raw)?;
    assert_eq!(plan.root.operation, "Filter: (t.a = 'x\ny')");
    assert_eq!(plan.root.actual_rows, Some(1.0));
    let scan = &plan.root.children[0];
    assert!(matches!(
        scan.warnings.as_slice(),
        [NodeWarning::SeqScanLargeTable { rows, .. }] if *rows == 200_000.0
    ));
    assert!(parse_plan(PlanFormat::MysqlTree, "  \n").is_err());
    Ok(())
}

// MariaDB

#[test]
fn mariadb_join_estimates() -> TestResult {
    for raw in [MA106_JOIN, MA123_JOIN] {
        let plan = parse_plan(PlanFormat::MariadbJson, raw)?;
        check_invariants(&plan);
        assert!(!plan.analyzed);
        assert_eq!(plan.root.operation, "Query block #1");
        // 10.6 writes the join as three `table` members of one object; both
        // versions end up with one nested loop over the three tables.
        let joins: Vec<_> = nodes(&plan.root)
            .into_iter()
            .filter(|n| n.operation == "Nested loop" && n.children.len() == 3)
            .collect();
        assert_eq!(joins.len(), 1, "{:?}", operations(&plan));
        let derived = find(&plan, "Index lookup on <derived2>")?;
        assert_eq!(derived.children[0].operation, "Materialize");
        let subquery = find_prefix(&plan, "Query block #3")?;
        assert_eq!(subquery.detail.relationship.as_deref(), Some("subquery"));
    }
    let plan = parse_plan(PlanFormat::MariadbJson, MA106_JOIN)?;
    assert_eq!(plan.share_basis, ShareBasis::None);
    let plan = parse_plan(PlanFormat::MariadbJson, MA123_JOIN)?;
    assert_eq!(plan.share_basis, ShareBasis::Cost);
    assert!(close(plan.total_cost, 3.031389023));
    Ok(())
}

#[test]
fn mariadb_join_analyze() -> TestResult {
    for raw in [MA106_JOIN_ANALYZE, MA123_JOIN_ANALYZE] {
        let plan = parse_plan(PlanFormat::MariadbJson, raw)?;
        check_invariants(&plan);
        assert!(plan.analyzed);
        assert_eq!(plan.share_basis, ShareBasis::Time);
        assert!(plan.execution_ms.is_some_and(|t| t > 1.0));
        let filesort = find(&plan, "Filesort")?;
        assert_eq!(filesort.detail.sort_key, vec!["sum(b.total) desc"]);
        let customers = find(&plan, "Full index scan on c")?;
        assert_eq!(customers.estimated_rows, Some(200.0));
        assert_eq!(customers.actual_rows, Some(200.0));
        assert_eq!(customers.loops, Some(1));
        let time = customers.actual_time.unwrap_or_default();
        assert!(time.exclusive_ms.is_some_and(|t| t > 0.0));
        let lookup = find(&plan, "Unique index lookup on <subquery4>")?;
        assert_eq!(lookup.loops, Some(200));
        assert_eq!(lookup.actual_rows, Some(0.9));
    }
    let plan = parse_plan(PlanFormat::MariadbJson, MA123_JOIN_ANALYZE)?;
    assert!(plan.planning_ms.is_some());
    Ok(())
}

#[test]
fn mariadb_union_and_update_analyze() -> TestResult {
    for raw in [MA106_UNION_ANALYZE, MA123_UNION_ANALYZE] {
        let plan = parse_plan(PlanFormat::MariadbJson, raw)?;
        check_invariants(&plan);
        let union = find(&plan, "Union")?;
        assert_eq!(union.children.len(), 2);
        assert_eq!(union.children[1].operation, "Query block #2 (UNION)");
        assert_eq!(union.actual_rows, Some(5.0));
    }
    for raw in [MA106_UPDATE_ANALYZE, MA123_UPDATE_ANALYZE] {
        let plan = parse_plan(PlanFormat::MariadbJson, raw)?;
        check_invariants(&plan);
        assert!(plan.analyzed);
        let update = find(&plan, "Update orders (Index range scan)")?;
        assert_eq!(update.actual_rows, Some(15.0));
        assert!(update.actual_time.and_then(|t| t.exclusive_ms).is_some());
    }
    Ok(())
}

// Statement building

#[test]
fn postgres_statements() -> TestResult {
    let pg13 = server(Engine::Postgres, "13.22");
    let pg18 = server(Engine::Postgres, "18.1");
    let plain = ExplainOptions::default();
    assert_eq!(
        explain_sql(&pg13, "SELECT 1;", &plain)?,
        "EXPLAIN (FORMAT JSON)\nSELECT 1"
    );
    let all = ExplainOptions {
        analyze: true,
        buffers: true,
        verbose: true,
        settings: true,
        wal: true,
        format: None,
        confirmed: false,
    };
    assert_eq!(
        explain_sql(&pg13, "select * from t", &all)?,
        "EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL)\nselect * from t"
    );
    let analyze = ExplainOptions {
        analyze: true,
        ..ExplainOptions::default()
    };
    assert_eq!(
        explain_sql(&pg18, "select 1", &analyze)?,
        "EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS FALSE)\nselect 1"
    );
    assert_eq!(
        explain_sql(&pg13, "select 1", &analyze)?,
        "EXPLAIN (FORMAT JSON, ANALYZE)\nselect 1"
    );
    // A leading comment stays on its own line.
    assert_eq!(
        explain_sql(&pg13, "-- note\nselect 1", &plain)?,
        "EXPLAIN (FORMAT JSON)\n-- note\nselect 1"
    );
    let wal_only = ExplainOptions {
        wal: true,
        ..ExplainOptions::default()
    };
    assert!(matches!(
        explain_sql(&pg13, "select 1", &wal_only),
        Err(CatalogError::Invalid { .. })
    ));
    let tree = ExplainOptions {
        format: Some(ExplainFormat::Tree),
        ..ExplainOptions::default()
    };
    assert!(matches!(
        explain_sql(&pg13, "select 1", &tree),
        Err(CatalogError::Unsupported { .. })
    ));
    assert_eq!(plan_format(&pg13, &plain)?, PlanFormat::PostgresJson);
    Ok(())
}

#[test]
fn mysql_and_mariadb_statements() -> TestResult {
    let my80 = server(Engine::Mysql, "8.0.46");
    let my8017 = server(Engine::Mysql, "8.0.17");
    let my8020 = server(Engine::Mysql, "8.0.20");
    let my97 = server(Engine::Mysql, "9.7.2");
    let maria = server(Engine::Mariadb, "10.6.24-MariaDB");
    let plain = ExplainOptions::default();
    let analyze = ExplainOptions {
        analyze: true,
        ..ExplainOptions::default()
    };
    let tree = ExplainOptions {
        format: Some(ExplainFormat::Tree),
        ..ExplainOptions::default()
    };
    let analyze_json = ExplainOptions {
        analyze: true,
        format: Some(ExplainFormat::Json),
        ..ExplainOptions::default()
    };
    assert_eq!(
        explain_sql(&my80, "SELECT 1", &plain)?,
        "EXPLAIN FORMAT=JSON\nSELECT 1"
    );
    assert_eq!(
        explain_sql(&my80, "SELECT 1", &tree)?,
        "EXPLAIN FORMAT=TREE\nSELECT 1"
    );
    assert_eq!(
        explain_sql(&my80, "SELECT 1", &analyze)?,
        "EXPLAIN ANALYZE FORMAT=TREE\nSELECT 1"
    );
    assert_eq!(
        explain_sql(&my8020, "SELECT 1", &analyze)?,
        "EXPLAIN ANALYZE\nSELECT 1"
    );
    assert!(matches!(
        explain_sql(&my8017, "SELECT 1", &analyze),
        Err(CatalogError::Unsupported { .. })
    ));
    assert!(matches!(
        explain_sql(&my80, "SELECT 1", &analyze_json),
        Err(CatalogError::Unsupported { .. })
    ));
    assert_eq!(
        explain_sql(&my97, "SELECT 1", &analyze_json)?,
        "EXPLAIN ANALYZE FORMAT=JSON\nSELECT 1"
    );
    assert_eq!(plan_format(&my80, &analyze)?, PlanFormat::MysqlTree);
    assert_eq!(plan_format(&my97, &plain)?, PlanFormat::MysqlJson);

    assert_eq!(
        explain_sql(&maria, "SELECT 1", &plain)?,
        "EXPLAIN FORMAT=JSON\nSELECT 1"
    );
    assert_eq!(
        explain_sql(&maria, "SELECT 1", &analyze)?,
        "ANALYZE FORMAT=JSON\nSELECT 1"
    );
    assert!(matches!(
        explain_sql(&maria, "SELECT 1", &tree),
        Err(CatalogError::Unsupported { .. })
    ));
    assert_eq!(plan_format(&maria, &analyze)?, PlanFormat::MariadbJson);

    for option in [
        ExplainOptions {
            buffers: true,
            ..ExplainOptions::default()
        },
        ExplainOptions {
            verbose: true,
            ..ExplainOptions::default()
        },
        ExplainOptions {
            settings: true,
            ..ExplainOptions::default()
        },
    ] {
        assert!(matches!(
            explain_sql(&my80, "SELECT 1", &option),
            Err(CatalogError::Unsupported { .. })
        ));
        assert!(matches!(
            explain_sql(&maria, "SELECT 1", &option),
            Err(CatalogError::Unsupported { .. })
        ));
    }
    Ok(())
}

#[test]
fn scripts_and_non_statements_are_refused() {
    let pg = server(Engine::Postgres, "17.0");
    let options = ExplainOptions::default();
    for text in [
        "select 1; select 2",
        "",
        "  -- only a comment\n",
        "explain select 1",
    ] {
        assert!(
            matches!(
                explain_sql(&pg, text, &options),
                Err(CatalogError::Invalid { .. })
            ),
            "{text:?}"
        );
    }
    assert!(matches!(
        explain_sql(&server(Engine::Redis, "7.4.0"), "GET k", &options),
        Err(CatalogError::Unsupported { .. })
    ));
}

#[test]
fn capabilities_follow_versions() {
    let pg12 = explain_capabilities(&server(Engine::Postgres, "12.0"));
    assert!(pg12.settings && !pg12.wal);
    let pg13 = explain_capabilities(&server(Engine::Postgres, "13.0"));
    assert!(pg13.settings && pg13.wal && pg13.buffers && pg13.verbose);
    let my = explain_capabilities(&server(Engine::Mysql, "8.0.46"));
    assert_eq!(my.formats, vec![ExplainFormat::Json, ExplainFormat::Tree]);
    assert_eq!(my.analyze_formats, vec![ExplainFormat::Tree]);
    let my97 = explain_capabilities(&server(Engine::Mysql, "9.7.2"));
    assert_eq!(
        my97.analyze_formats,
        vec![ExplainFormat::Tree, ExplainFormat::Json]
    );
    let maria = explain_capabilities(&server(Engine::Mariadb, "12.3.1-MariaDB"));
    assert_eq!(maria.formats, vec![ExplainFormat::Json]);
    assert!(maria.analyze && !maria.buffers);
}

// ANALYZE safety

#[test]
fn analyze_safety_by_statement_kind() {
    let pg = server(Engine::Postgres, "18.0");
    let my = server(Engine::Mysql, "8.0.46");
    let maria = server(Engine::Mariadb, "10.6.24-MariaDB");
    for s in [&pg, &my, &maria] {
        assert_eq!(
            analyze_safety(s, "SELECT * FROM t"),
            AnalyzeSafety::ReadOnly
        );
        assert_eq!(
            analyze_safety(s, "WITH x AS (SELECT 1) SELECT * FROM x"),
            AnalyzeSafety::ReadOnly
        );
        assert_eq!(
            analyze_safety(s, "select 'insert into t' as s"),
            AnalyzeSafety::ReadOnly
        );
        for dml in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1 WHERE id = 2",
            "DELETE FROM t WHERE id = 1",
        ] {
            assert!(
                matches!(analyze_safety(s, dml), AnalyzeSafety::RolledBack { .. }),
                "{dml}"
            );
        }
        for refused in [
            "CREATE TABLE t (a int)",
            "DROP TABLE t",
            "TRUNCATE t",
            "ALTER TABLE t ADD COLUMN b int",
            "BEGIN",
            "COMMIT",
            "GRANT SELECT ON t TO u",
            "CALL p()",
            "SET x = 1",
            "SELECT 1; SELECT 2",
            "",
        ] {
            assert!(
                matches!(analyze_safety(s, refused), AnalyzeSafety::Refused { .. }),
                "{refused}"
            );
        }
    }
    // Updates without WHERE get an extra warning.
    let AnalyzeSafety::RolledBack { warnings } = analyze_safety(&pg, "UPDATE t SET a = 1") else {
        panic!("UPDATE without WHERE is rolled back");
    };
    assert_eq!(warnings.len(), 3);
    // Non-transactional engines are mentioned on MySQL and MariaDB.
    let AnalyzeSafety::RolledBack { warnings } = analyze_safety(&maria, "DELETE FROM t WHERE a=1")
    else {
        panic!("DELETE is rolled back");
    };
    assert!(warnings.iter().any(|w| w.contains("non-transactional")));
    // PostgreSQL specials.
    assert!(matches!(
        analyze_safety(
            &pg,
            "WITH gone AS (DELETE FROM t RETURNING *) SELECT count(*) FROM gone"
        ),
        AnalyzeSafety::RolledBack { .. }
    ));
    assert!(matches!(
        analyze_safety(&pg, "SELECT nextval('s')"),
        AnalyzeSafety::NeedsConfirmation { .. }
    ));
    assert!(matches!(
        analyze_safety(&pg, "SELECT * INTO copy FROM t"),
        AnalyzeSafety::RolledBack { .. }
    ));
    assert_eq!(
        analyze_safety(&pg, "SELECT $$ delete from t $$, 'update' /* insert */"),
        AnalyzeSafety::ReadOnly
    );
    // MySQL writes files with INTO OUTFILE.
    assert!(matches!(
        analyze_safety(&my, "SELECT * FROM t INTO OUTFILE '/tmp/x'"),
        AnalyzeSafety::Refused { .. }
    ));
    assert!(matches!(
        analyze_safety(&server(Engine::Redis, "7.4.0"), "GET k"),
        AnalyzeSafety::Refused { .. }
    ));
}

#[test]
fn words_skip_literals_and_comments() {
    assert_eq!(
        safety::words(
            "select 'a''b', \"Update\", $t$ delete $t$, E'x\\'y' -- insert\n /* merge /* nested */ */ from t",
            Dialect::Postgres,
            false
        ),
        vec!["SELECT", "FROM", "T"]
    );
    assert_eq!(
        safety::words(
            "select `delete`, 'a\\'b' # update\n from t",
            Dialect::Mysql,
            true
        ),
        vec!["SELECT", "FROM", "T"]
    );
    // Without backslash escapes the same text reads differently.
    assert!(
        safety::words("select 'a\\'b' from t", Dialect::Mysql, false).contains(&"B".to_owned())
    );
    // MySQL `--` needs whitespace after it to start a comment.
    assert_eq!(
        safety::words("select 1 --1, x", Dialect::Mysql, true),
        vec!["SELECT", "X"]
    );
    assert_eq!(
        safety::words("select 1 --1, x", Dialect::Postgres, false),
        vec!["SELECT"]
    );
    // PostgreSQL identifiers may contain `$`: `a$b$` is one word, not the
    // start of a dollar quote hiding what follows.
    assert_eq!(
        safety::words(
            "WITH a$b$ AS (DELETE FROM t RETURNING 1) SELECT 1",
            Dialect::Postgres,
            false
        ),
        vec![
            "WITH",
            "A$B$",
            "AS",
            "DELETE",
            "FROM",
            "T",
            "RETURNING",
            "SELECT"
        ]
    );
}

#[test]
fn words_follow_the_driver_lexer() {
    // A line comment ends at `\r` as well, so what follows is code.
    assert_eq!(
        safety::words("select 1 -- note\rdelete from t", Dialect::Postgres, false),
        vec!["SELECT", "DELETE", "FROM", "T"]
    );
    // Non-ASCII identifiers with `$` do not open a dollar quote.
    assert_eq!(
        safety::words(
            "WITH \u{e9}$x$ AS (DELETE FROM t) SELECT 1",
            Dialect::Postgres,
            false
        ),
        vec!["WITH", "\u{e9}$X$", "AS", "DELETE", "FROM", "T", "SELECT"]
    );
    // The contents of a MySQL executable comment are code.
    assert_eq!(
        safety::words("select 1 /*! , sleep(1) */", Dialect::Mysql, true),
        vec!["SELECT", "SLEEP"]
    );
}

#[test]
fn function_calls_are_found() {
    assert_eq!(
        safety::calls_in(
            "WITH x (a) AS (SELECT count(*) FROM t) SELECT lower(name), CAST(a AS varchar(10)), \
             b::numeric(10,2), s.f(1), \"Odd\"(2) FROM x AS y(a) JOIN t USING (id) \
             WHERE id IN (1) AND EXISTS (SELECT 1) OVER (PARTITION BY a)",
            Dialect::Postgres
        ),
        vec!["count", "lower", "cast", "s.f", "Odd"]
    );
    assert_eq!(
        safety::calls_in(
            "INSERT INTO t (a, b) SELECT get_lock('x', 1), ifnull(a, 0) FROM s.u",
            Dialect::Mysql
        ),
        vec!["get_lock", "ifnull"]
    );
}

#[test]
fn side_effecting_functions_need_confirmation() {
    let pg = server(Engine::Postgres, "18.0");
    let my = server(Engine::Mysql, "9.7.2");
    let maria = server(Engine::Mariadb, "12.3.1-MariaDB");
    let needs = |s: &ServerInfo, sql: &str, function: &str| match analyze_safety(s, sql) {
        AnalyzeSafety::NeedsConfirmation { warnings } => {
            assert!(
                warnings
                    .iter()
                    .any(|w| w.starts_with(&format!("{function}()"))),
                "{sql}: {warnings:?}"
            );
            warnings
        }
        other => panic!("{sql}: {other:?}"),
    };
    for (sql, function) in [
        ("SELECT pg_advisory_lock(1)", "pg_advisory_lock"),
        ("SELECT pg_try_advisory_lock(1)", "pg_try_advisory_lock"),
        ("SELECT lo_export(1, '/tmp/x')", "lo_export"),
        ("SELECT lo_import('/etc/passwd')", "lo_import"),
        ("SELECT dblink_exec('c', 'DELETE FROM t')", "dblink_exec"),
        ("SELECT pg_notify('c', 'x')", "pg_notify"),
        ("SELECT nextval('s')", "nextval"),
        ("SELECT setval('s', 1)", "setval"),
        ("SELECT pg_terminate_backend(42)", "pg_terminate_backend"),
        ("SELECT pg_cancel_backend(42)", "pg_cancel_backend"),
        ("SELECT pg_reload_conf()", "pg_reload_conf"),
        ("SELECT pg_file_write('x', 'y', false)", "pg_file_write"),
        ("SELECT my_function(id) FROM t", "my_function"),
        ("SELECT * FROM app.report(1)", "app.report"),
        (
            "SELECT pg_catalog.pg_advisory_lock(1)",
            "pg_catalog.pg_advisory_lock",
        ),
        ("UPDATE t SET a = audit(a) WHERE id = 1", "audit"),
    ] {
        needs(&pg, sql, function);
    }
    for s in [&my, &maria] {
        for (sql, function) in [
            ("SELECT GET_LOCK('x', 10)", "get_lock"),
            ("SELECT RELEASE_LOCK('x')", "release_lock"),
            ("SELECT SLEEP(1)", "sleep"),
            ("SELECT BENCHMARK(1000000, MD5('x'))", "benchmark"),
            ("SELECT my_udf(a) FROM t", "my_udf"),
        ] {
            let warnings = needs(s, sql, function);
            // The non-transactional warning shows for SELECTs as well.
            assert!(
                warnings.iter().any(|w| w.contains("non-transactional")),
                "{sql}"
            );
        }
        // A backslash hides a call in one reading of the text, not in the
        // other; both count.
        needs(s, "SELECT 'a\\', GET_LOCK('x', 1), 'b' FROM t", "get_lock");
    }
    assert!(matches!(
        analyze_safety(&maria, "SELECT NEXT VALUE FOR s"),
        AnalyzeSafety::NeedsConfirmation { .. }
    ));
    // Built-ins without side effects stay read-only.
    for sql in [
        "SELECT lower(name), count(*), coalesce(a, 0), date_trunc('day', now()) FROM t GROUP BY 1",
        "SELECT * FROM generate_series(1, 10)",
        "SELECT pg_catalog.lower('X')",
    ] {
        assert_eq!(analyze_safety(&pg, sql), AnalyzeSafety::ReadOnly, "{sql}");
    }
    assert_eq!(
        analyze_safety(
            &my,
            "SELECT IFNULL(a, 0), DATE_FORMAT(d, '%Y'), COUNT(*) FROM t"
        ),
        AnalyzeSafety::ReadOnly
    );
}

#[test]
fn hidden_statements_are_refused() {
    let my = server(Engine::Mysql, "8.0.46");
    let maria = server(Engine::Mariadb, "10.6.24-MariaDB");
    let pg = server(Engine::Postgres, "13.22");
    let plain = ExplainOptions::default();
    let analyze = ExplainOptions {
        analyze: true,
        ..ExplainOptions::default()
    };
    for s in [&my, &maria] {
        for text in [
            "SELECT 1 --1; DELETE FROM t",
            "SELECT 1 /*! ; DELETE FROM t */",
            "SELECT 1 /*!80000 ; DELETE FROM t */",
            "SELECT 1 /*M! ; DELETE FROM t */",
            "SELECT 'a\\'; DELETE FROM t; SELECT '",
        ] {
            for options in [&plain, &analyze] {
                assert!(
                    matches!(
                        explain_sql(s, text, options),
                        Err(CatalogError::Invalid { .. })
                    ),
                    "{text}"
                );
            }
            assert!(
                matches!(analyze_safety(s, text), AnalyzeSafety::Refused { .. }),
                "{text}"
            );
        }
    }
    // PostgreSQL: `--1` is a comment there, so the DELETE is a second
    // statement and the script is refused too.
    assert!(matches!(
        explain_sql(&pg, "SELECT 1 --1\n; DELETE FROM t", &plain),
        Err(CatalogError::Invalid { .. })
    ));
}

#[test]
fn plans_serialize_for_the_ui() -> Result<(), Box<dyn std::error::Error>> {
    let plan = parse_plan(PlanFormat::PostgresJson, PG13_PARALLEL)?;
    let json = serde_json::to_value(&plan)?;
    assert_eq!(json["format"], "postgresJson");
    assert_eq!(json["shareBasis"], "time");
    assert_eq!(json["root"]["operation"], "Gather Merge");
    let sort = &json["root"]["children"][0];
    assert_eq!(sort["warnings"][0]["kind"], "externalSort");
    assert!(sort["actualTime"]["exclusiveMs"].is_number());
    assert!(sort["buffers"]["tempWritten"].is_number());
    let back: Plan = serde_json::from_value(json)?;
    assert_eq!(back, plan);
    Ok(())
}

#[test]
fn a_call_with_a_column_alias_is_a_call() {
    let pg = ServerInfo {
        engine: Engine::Postgres,
        version: "18.0".into(),
        major: 18,
        minor: 0,
        patch: None,
    };
    let my = ServerInfo {
        engine: Engine::Mysql,
        version: "9.7.0".into(),
        major: 9,
        minor: 7,
        patch: Some(0),
    };
    for (server, statement) in [
        (&pg, "SELECT pg_sleep(10) AS slept"),
        (&my, "SELECT SLEEP(10) AS slept"),
        (&pg, "SELECT nextval('s') AS n FROM t"),
    ] {
        assert!(
            matches!(
                analyze_safety(server, statement),
                AnalyzeSafety::NeedsConfirmation { .. }
            ),
            "{statement}"
        );
    }
    // A CTE column list is no call.
    assert_eq!(
        safety::calls_in(
            "WITH x (a) AS (SELECT 1), y (b) AS MATERIALIZED (SELECT 2), \
             z (c) AS NOT MATERIALIZED (SELECT 3) SELECT lower(a) AS l FROM x",
            Dialect::Postgres
        ),
        vec!["lower".to_owned()]
    );
    // Nor is the alias column list after WITH ORDINALITY.
    let ordinality = "SELECT * FROM unnest(ARRAY[1]) WITH ORDINALITY u(x, n)";
    assert_eq!(
        safety::calls_in(ordinality, Dialect::Postgres),
        vec!["unnest".to_owned()]
    );
}

#[test]
fn a_column_definition_list_is_a_call() {
    let pg = ServerInfo {
        engine: Engine::Postgres,
        version: "18.0".into(),
        major: 18,
        minor: 0,
        patch: None,
    };
    for statement in [
        "SELECT * FROM dblink('dbname=x', 'INSERT INTO t VALUES (1) RETURNING x') AS (x int)",
        "SELECT * FROM ROWS FROM (evilrec(1) AS (a bigint))",
        "SELECT * FROM t, f(1) AS (a int)",
    ] {
        assert!(
            matches!(
                analyze_safety(&pg, statement),
                AnalyzeSafety::NeedsConfirmation { .. }
            ),
            "{statement}"
        );
    }
    assert_eq!(
        safety::calls_in(
            "WITH RECURSIVE a (x) AS (SELECT 1), b (y) AS (SELECT 2) SELECT * FROM a, b",
            Dialect::Postgres
        ),
        Vec::<String>::new()
    );
}

#[test]
fn locking_clauses_are_text_reasons() {
    for statement in [
        "SELECT * FROM t FOR UPDATE",
        "SELECT * FROM t FOR NO KEY UPDATE",
        "SELECT * FROM t FOR SHARE NOWAIT",
        "SELECT * FROM t FOR KEY SHARE SKIP LOCKED",
    ] {
        let reasons = probe::pg_text_reasons(statement);
        assert!(
            reasons.iter().any(|r| r.contains("locks rows")),
            "{statement}: {reasons:?}"
        );
    }
    // A literal or a column named like the clause is no lock.
    for statement in [
        "SELECT 'for update' FROM t",
        "SELECT t.\"for\", t.update FROM t",
        "SELECT * FROM t WHERE t.a = 1",
    ] {
        assert!(probe::pg_text_reasons(statement).is_empty(), "{statement}");
    }
}

#[test]
fn rewritten_statements_name_tables_and_stored_functions() {
    let names = |note: &str, ansi_quotes: bool| {
        let tokens = safety::tokens_quoting(note, Dialect::Mysql, true, ansi_quotes);
        let mut names = probe::RewrittenNames::default();
        probe::rewritten_names(&tokens, &mut names);
        names
    };
    let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
    // MySQL 8.0: a join with a temporary table and a semi join, aliases,
    // columns qualified three ways, a stored function of the default
    // database (quoted) and a qualified one, a built-in (bare).
    let note = "/* select#1 */ select `f`(`db`.`x`.`a`) AS `f(a)`,`db`.`f2`(1) AS `x`,\
                upper(`db`.`x`.`b`) AS `b` from `db`.`t` `x` join `db`.`tmp` \
                semi join (`db`.`trg`) where ((`db`.`tmp`.`a` = `db`.`x`.`a`) \
                and (`db`.`trg`.`a` = `db`.`x`.`a`))";
    let found = names(note, false);
    assert!(found.tables.contains(&pair("db", "t")), "{found:?}");
    assert!(found.tables.contains(&pair("db", "tmp")), "{found:?}");
    assert!(found.tables.contains(&pair("db", "trg")), "{found:?}");
    assert_eq!(
        found.functions,
        [
            (None, "f".to_owned()),
            (Some("db".to_owned()), "f2".to_owned())
        ]
        .into_iter()
        .collect()
    );
    assert!(found.calls.contains("upper"), "{found:?}");
    assert!(!found.calls.contains("f"), "{found:?}");
    // A CTE with a column list (MySQL prints `c`(`n`), like a call), a
    // window name and a derived table are no stored functions; the tables
    // inside are found; names keep their case.
    let note = "with recursive `c`(`n`) as (select 1 AS `n` union all select `c`.`n` + 1 \
                AS `n + 1` from `c` where (`c`.`n` < 3)) select `x`, \
                row_number() OVER `w` AS `r` from (select 1 AS `n` from `db`.`U`) `d` \
                window `w` AS (ORDER BY `d`.`n`)";
    let found = names(note, false);
    assert!(found.functions.is_empty(), "{found:?}");
    assert!(found.tables.contains(&pair("db", "U")), "{found:?}");
    // Columns of an aliased table (`db`.`x`.`a`), of a derived table
    // (`d`.`n`) and of a plain table (`db`.`t`.`a`) name no table besides
    // the ones in FROM.
    let note = "select `db`.`x`.`a` AS `a`,`d`.`n` AS `n`,`db`.`t2`.`b` AS `b` \
                from `db`.`t` `x` join (select 1 AS `n` from `db`.`u`) `d` join `db`.`t2` \
                where `db`.`x`.`a` = `d`.`n`";
    let found = names(note, false);
    assert_eq!(
        found.tables,
        [pair("db", "t"), pair("db", "u"), pair("db", "t2")]
            .into_iter()
            .collect()
    );
    // An aliased call is still a call.
    let found = names("select `f`(`db`.`t`.`a`) AS `x` from `db`.`t`", false);
    assert_eq!(
        found.functions,
        [(None, "f".to_owned())].into_iter().collect()
    );
    // ANSI_QUOTES: the server prints double quotes; a string stays a
    // string in the default mode.
    let note =
        "select \"db\".\"t\".\"a\" AS \"a\" from \"db\".\"t\" where (\"db\".\"t\".\"b\" = 'x')";
    assert!(names(note, true).tables.contains(&pair("db", "t")));
    assert!(names(note, false).tables.is_empty());
}

#[test]
fn pg_blocks_create_the_view_and_raise_the_statements_own_error() -> TestResult {
    let block = probe::pg_block(
        Some("EXPLAIN (FORMAT JSON) SELECT 1"),
        Some((
            "CREATE TEMP VIEW varve_explain_check AS SELECT NULL::pg_catalog.text FROM (\nSELECT 1\n) AS varve_explain_sub",
            "EXECUTE varve_explain_check_v9",
        )),
        true,
    )?;
    assert!(block.starts_with("DO $"));
    assert!(block.contains("CREATE TEMP VIEW varve_explain_check"));
    // The statement's own error is raised again; a read-only transaction,
    // a duplicate view and a missing TEMP privilege are check failures.
    assert!(block.contains("RAISE;"));
    assert!(block.contains("'25006'"));
    assert!(block.contains("'42P07'"));
    assert!(block.contains("'42501'"));
    // The lookup runs after the view exists and before the run, which is
    // read-only.
    let view = block
        .find("CREATE TEMP VIEW")
        .ok_or(CatalogError::Unexpected {
            message: "no view".into(),
        })?;
    let lookup = block
        .find("EXECUTE varve_explain_check_v9")
        .ok_or(CatalogError::Unexpected {
            message: "no lookup".into(),
        })?;
    let run = block
        .find("transaction_read_only")
        .ok_or(CatalogError::Unexpected {
            message: "no run".into(),
        })?;
    assert!(view < lookup && lookup < run);
    // Without a check or a run the block only answers.
    let plain = probe::pg_block(None, None, false)?;
    assert!(!plain.contains("CREATE TEMP VIEW") && plain.contains("'R[]'"));
    Ok(())
}

#[test]
fn engines_are_read_from_definitions() {
    assert_eq!(
        probe::engine_of(
            "CREATE TEMPORARY TABLE `tmp` (\n  `a` int(11) DEFAULT NULL\n) ENGINE=FEDERATED DEFAULT CHARSET=utf8mb4 CONNECTION='mysql://x'"
        ),
        Some("FEDERATED".to_owned())
    );
    assert_eq!(
        probe::engine_of("CREATE TABLE `t` (`a` int) engine=InnoDB"),
        Some("INNODB".to_owned())
    );
    assert_eq!(probe::engine_of("CREATE VIEW v AS SELECT 1"), None);
    // `ENGINE=` inside the column list (a comment, a default, a quoted
    // name, a comment block) is not the table's engine.
    assert_eq!(
        probe::engine_of(
            "CREATE TEMPORARY TABLE `tfc` (\n  `n` int COMMENT 'ENGINE=InnoDB',\n  \
             `v` varchar(5) DEFAULT 'ENGINE=MyISAM' /* ENGINE=Aria */\n) ENGINE=FEDERATED \
             DEFAULT CHARSET=utf8mb4 CONNECTION='mysql://x'"
        ),
        Some("FEDERATED".to_owned())
    );
    assert_eq!(
        probe::engine_of(
            "CREATE TEMPORARY TABLE `ENGINE=InnoDB` (\n  `n` int\n) ENGINE=FEDERATED \
             CONNECTION='mysql://x'"
        ),
        Some("FEDERATED".to_owned())
    );
    assert_eq!(
        probe::engine_of("CREATE TABLE `t` (`a` int) COMMENT='x' engine = MyISAM"),
        Some("MYISAM".to_owned())
    );
    assert_eq!(
        probe::engine_of("CREATE TABLE `t` (`a` int COMMENT 'ENGINE=InnoDB')"),
        None
    );
}
