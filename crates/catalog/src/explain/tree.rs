//! MySQL `EXPLAIN FORMAT=TREE` and `EXPLAIN ANALYZE` text.
//!
//! Every node is a line `-> operation  (cost=a..b rows=n) (actual
//! time=f..l rows=r loops=k)`, indented four spaces per level. Estimates
//! are per loop, actual times and rows are averages per loop, and a node
//! that never ran ends in `(never executed)`. A condition with a line break
//! in a string literal continues on lines without `->`.

use super::analysis::{self, prop};
use super::{ActualTime, Cost, Plan, PlanFormat, PlanNode, ShareBasis};
use crate::model::{CatalogError, CatalogResult};

/// What MySQL prints when it cannot measure a statement (single-table
/// `UPDATE` and `DELETE` before the iterator executor handled them).
const NOT_EXECUTABLE: &str = "<not executable by iterator executor>";

/// Parses tree text.
pub(crate) fn parse(raw: &str) -> CatalogResult<Plan> {
    // Group lines into entries: a `->` line plus its continuation lines.
    let mut entries: Vec<(usize, String)> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if let Some(text) = trimmed.strip_prefix("->") {
            entries.push((indent, text.trim().to_owned()));
        } else if let Some((_, text)) = entries.last_mut() {
            text.push('\n');
            text.push_str(trimmed);
        } else {
            entries.push((indent, trimmed.to_owned()));
        }
    }
    if entries.is_empty() {
        return Err(CatalogError::Unexpected {
            message: "MySQL plan: the tree output is empty".into(),
        });
    }

    // Build the tree with a stack of (indent, node).
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    for (indent, text) in entries {
        let node = node(&text);
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            pop(&mut stack, &mut roots);
        }
        stack.push((indent, node));
    }
    while !stack.is_empty() {
        pop(&mut stack, &mut roots);
    }
    // Subqueries evaluated in the projection (`-> Select #2 (subquery in
    // projection; ...)`) are printed at the top level after the main plan;
    // they belong to it.
    let mut rest = roots.into_iter();
    let Some(mut root) = rest.next() else {
        return Err(CatalogError::Unexpected {
            message: "MySQL plan: the tree output has no node".into(),
        });
    };
    root.children.extend(rest);
    analysis::settle(&mut root);

    let mut warnings = Vec::new();
    if contains_operation(&root, NOT_EXECUTABLE) {
        warnings.push(
            "MySQL could not measure this statement (single-table UPDATE and DELETE are not \
             run by EXPLAIN ANALYZE); the plan has no actual values"
                .to_owned(),
        );
    }
    let mut plan = Plan {
        format: PlanFormat::MysqlTree,
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
        warnings,
        notices: Vec::new(),
        raw: raw.to_owned(),
    };
    analysis::finish(&mut plan);
    Ok(plan)
}

fn contains_operation(node: &PlanNode, operation: &str) -> bool {
    node.operation == operation
        || node
            .children
            .iter()
            .any(|c| contains_operation(c, operation))
}

fn pop(stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>) {
    if let Some((_, node)) = stack.pop() {
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(node),
            None => roots.push(node),
        }
    }
}

/// The trailing `( ... )` group of `text` and the text before it, when the
/// group is one of the measurement groups.
fn trailing_group(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_end();
    if !text.ends_with(')') {
        return None;
    }
    let mut depth = 0usize;
    for (i, c) in text.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    let inner = &text[i + 1..text.len() - 1];
                    let measured = ["cost=", "rows=", "actual ", "never executed"]
                        .iter()
                        .any(|p| inner.starts_with(p));
                    return measured.then_some((&text[..i], inner));
                }
            }
            _ => {}
        }
    }
    None
}

/// `a..b` or `b`.
fn range(text: &str) -> (Option<f64>, Option<f64>) {
    match text.split_once("..") {
        Some((a, b)) => (a.parse().ok(), b.parse().ok()),
        None => (None, text.parse().ok()),
    }
}

fn node(text: &str) -> PlanNode {
    let mut node = PlanNode::default();
    let mut rest = text;
    let mut cost = None;
    let mut time = None;
    while let Some((before, group)) = trailing_group(rest) {
        rest = before;
        if group == "never executed" {
            node.never_executed = true;
            node.loops = Some(0);
            continue;
        }
        let (actual, fields) = match group.strip_prefix("actual ") {
            Some(fields) => (true, fields),
            None => (false, group),
        };
        for field in fields.split_whitespace() {
            let Some((name, value)) = field.split_once('=') else {
                continue;
            };
            match (actual, name) {
                (false, "cost") => {
                    let (startup, total) = range(value);
                    cost = Some(Cost {
                        startup,
                        total,
                        exclusive: None,
                    });
                }
                (false, "rows") => node.estimated_rows = value.parse().ok(),
                (true, "time") => {
                    let (first, last) = range(value);
                    time = Some(ActualTime {
                        first_row_ms: first,
                        per_loop_ms: last,
                        ..ActualTime::default()
                    });
                }
                (true, "rows") => node.actual_rows = value.parse().ok(),
                (true, "loops") => node.loops = value.parse().ok(),
                _ => node.properties.push(prop(name, value)),
            }
        }
    }
    node.operation = rest.trim().to_owned();
    node.estimated_cost = cost;
    if let Some(mut time) = time {
        time.total_ms = time
            .per_loop_ms
            .map(|t| t * node.loops.map_or(1.0, |l| l as f64));
        node.actual_time = Some(time);
    }
    describe(&mut node);
    node
}

/// Fills the detail fields the operation text names.
fn describe(node: &mut PlanNode) {
    let op = node.operation.clone();
    let lower = op.to_ascii_lowercase();
    let d = &mut node.detail;
    if let Some(filter) = op.strip_prefix("Filter: ") {
        d.filter = Some(filter.to_owned());
    }
    if let Some(keys) = op.strip_prefix("Sort: ") {
        d.sort_key = split_top_level(keys);
    }
    if let Some(keys) = op.strip_prefix("Sort row IDs: ") {
        d.sort_key = split_top_level(keys);
    }
    if let Some(at) = op.find(" on ")
        && (lower.contains("scan") || lower.contains("lookup") || lower.contains("search"))
    {
        let after = &op[at + 4..];
        let name = after.split_whitespace().next().unwrap_or("");
        if !name.is_empty() {
            d.relation = Some(name.to_owned());
        }
        if let Some(using) = after.find(" using ") {
            let index = after[using + 7..].split_whitespace().next().unwrap_or("");
            if !index.is_empty() {
                d.index = Some(index.to_owned());
            }
        }
        if let Some(open) = after.find(" (")
            && after.ends_with(')')
        {
            d.condition = Some(after[open + 2..after.len() - 1].to_owned());
        }
    }
    if lower.contains("join") {
        if lower.starts_with("nested loop") {
            d.strategy = Some("nested loop".into());
        } else if lower.contains("hash") {
            d.strategy = Some("hash".into());
        }
        for (word, join_type) in [
            ("left", "Left"),
            ("right", "Right"),
            ("outer", "Outer"),
            ("antijoin", "Anti"),
            ("anti", "Anti"),
            ("semijoin", "Semi"),
            ("semi", "Semi"),
            ("inner", "Inner"),
        ] {
            if lower
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == word)
            {
                d.join_type = Some(join_type.to_owned());
                break;
            }
        }
        if let Some(open) = op.find(" (")
            && op.ends_with(')')
        {
            d.condition = Some(op[open + 2..op.len() - 1].to_owned());
        }
    }
    if op.starts_with("Select #") {
        d.relationship = Some("subquery".into());
    }
    if lower.starts_with("table scan") && !d.relation.as_deref().is_some_and(|r| r.starts_with('<'))
    {
        let rows = match (node.estimated_rows, node.actual_rows) {
            (Some(e), Some(a)) => Some(e.max(a)),
            (e, a) => e.or(a),
        };
        if let Some(warning) = analysis::large_scan(d.relation.as_deref(), rows) {
            node.warnings.push(warning);
        }
    }
}

/// Splits on commas outside parentheses and quotes.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote = None;
    let mut current = String::new();
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                current.push(c);
            }
            (Some(_), c) => current.push(c),
            (None, '\'' | '"' | '`') => {
                quote = Some(c);
                current.push(c);
            }
            (None, '(') => {
                depth += 1;
                current.push(c);
            }
            (None, ')') => {
                depth -= 1;
                current.push(c);
            }
            (None, ',') if depth == 0 => {
                parts.push(current.trim().to_owned());
                current.clear();
            }
            (None, c) => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_owned());
    }
    parts
}
