//! Engine-neutral analysis of a parsed plan: exclusive times and costs,
//! hot-spot shares, row misestimates, node ids.

use super::{ActualTime, Cost, NodeWarning, Plan, PlanNode, PlanProperty, ShareBasis};

/// Estimated and actual rows that differ by this factor or more are
/// reported as a [`NodeWarning::RowMisestimate`].
pub const MISESTIMATE_FACTOR: f64 = 10.0;

/// Full scans of tables with at least this many rows are reported as a
/// [`NodeWarning::SeqScanLargeTable`].
pub const LARGE_TABLE_ROWS: f64 = 100_000.0;

/// A property with a name and text.
pub(crate) fn prop(name: &str, value: impl Into<String>) -> PlanProperty {
    PlanProperty {
        name: name.to_owned(),
        value: value.into(),
    }
}

/// The large-table warning when `rows` reaches [`LARGE_TABLE_ROWS`].
pub(crate) fn large_scan(relation: Option<&str>, rows: Option<f64>) -> Option<NodeWarning> {
    let rows = rows?;
    (rows >= LARGE_TABLE_ROWS).then(|| NodeWarning::SeqScanLargeTable {
        relation: relation.map(str::to_owned),
        rows,
    })
}

/// Completes a pair of total and exclusive values from the children's
/// totals: whichever of the two the engine reported, the other follows.
/// Nodes the engine gave neither for take their children's sum as total.
fn complete(
    total: Option<f64>,
    exclusive: Option<f64>,
    children: Option<f64>,
) -> (Option<f64>, Option<f64>) {
    match (total, exclusive) {
        (Some(t), Some(e)) => (Some(t), Some(e)),
        (Some(t), None) => (Some(t), Some((t - children.unwrap_or(0.0)).max(0.0))),
        (None, Some(e)) => (Some(e + children.unwrap_or(0.0)), Some(e)),
        (None, None) => match children {
            Some(sum) => (Some(sum), Some(0.0)),
            None => (None, None),
        },
    }
}

/// Fills in total and exclusive actual times and costs, bottom up.
pub(crate) fn settle(node: &mut PlanNode) {
    for child in &mut node.children {
        settle(child);
    }
    let child_time = sum(node
        .children
        .iter()
        .map(|c| c.actual_time.and_then(|t| t.total_ms)));
    let child_cost = sum(node
        .children
        .iter()
        .map(|c| c.estimated_cost.and_then(|c| c.total)));

    let time = node.actual_time.unwrap_or_default();
    let (total, exclusive) = complete(time.total_ms, time.exclusive_ms, child_time);
    if total.is_some() || node.actual_time.is_some() {
        node.actual_time = Some(ActualTime {
            total_ms: total,
            exclusive_ms: exclusive,
            ..time
        });
    }

    let cost = node.estimated_cost.unwrap_or_default();
    let (total, exclusive) = complete(cost.total, cost.exclusive, child_cost);
    if total.is_some() || node.estimated_cost.is_some() {
        node.estimated_cost = Some(Cost {
            total,
            exclusive,
            ..cost
        });
    }
}

/// Sum of the values present, `None` when none is.
fn sum(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values
        .flatten()
        .fold(None, |acc, v| Some(acc.unwrap_or(0.0) + v))
}

/// Numbers the nodes, adds row misestimate warnings, and computes the
/// hot-spot shares and plan totals. Runs after [`settle`].
pub(crate) fn finish(plan: &mut Plan) {
    let mut next = 0;
    number(&mut plan.root, &mut next);
    misestimates(&mut plan.root);

    let time_sum = sum_exclusive(&plan.root, &|n| n.actual_time.and_then(|t| t.exclusive_ms));
    let cost_sum = sum_exclusive(&plan.root, &|n| n.estimated_cost.and_then(|c| c.exclusive));
    plan.share_basis = if time_sum > 0.0 {
        ShareBasis::Time
    } else if cost_sum > 0.0 {
        ShareBasis::Cost
    } else {
        ShareBasis::None
    };
    match plan.share_basis {
        ShareBasis::Time => share(&mut plan.root, time_sum, &|n| {
            n.actual_time.and_then(|t| t.exclusive_ms)
        }),
        ShareBasis::Cost => share(&mut plan.root, cost_sum, &|n| {
            n.estimated_cost.and_then(|c| c.exclusive)
        }),
        ShareBasis::None => {}
    }

    plan.analyzed = analyzed(&plan.root);
    if plan.total_cost.is_none() {
        plan.total_cost = plan.root.estimated_cost.and_then(|c| c.total);
    }
    if plan.execution_ms.is_none() && plan.analyzed {
        plan.execution_ms = plan.root.actual_time.and_then(|t| t.total_ms);
    }
}

fn number(node: &mut PlanNode, next: &mut u32) {
    node.id = *next;
    *next += 1;
    for child in &mut node.children {
        number(child, next);
    }
}

fn analyzed(node: &PlanNode) -> bool {
    node.actual_rows.is_some()
        || node.loops.is_some()
        || node.actual_time.is_some_and(|t| t.total_ms.is_some())
        || node.children.iter().any(analyzed)
}

fn misestimates(node: &mut PlanNode) {
    if let (Some(estimated), Some(actual)) = (node.estimated_rows, node.actual_rows)
        && !node.never_executed
        && node.loops != Some(0)
    {
        // Below one row the ratio says nothing, so both count as at least 1.
        let (e, a) = (estimated.max(1.0), actual.max(1.0));
        let factor = e.max(a) / e.min(a);
        if factor >= MISESTIMATE_FACTOR {
            node.warnings.push(NodeWarning::RowMisestimate {
                estimated,
                actual,
                factor,
            });
        }
    }
    for child in &mut node.children {
        misestimates(child);
    }
}

fn sum_exclusive(node: &PlanNode, value: &dyn Fn(&PlanNode) -> Option<f64>) -> f64 {
    value(node).unwrap_or(0.0)
        + node
            .children
            .iter()
            .map(|c| sum_exclusive(c, value))
            .sum::<f64>()
}

fn share(node: &mut PlanNode, total: f64, value: &dyn Fn(&PlanNode) -> Option<f64>) {
    node.share = value(node).map(|v| (v / total).clamp(0.0, 1.0));
    for child in &mut node.children {
        share(child, total, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timed(total: Option<f64>, exclusive: Option<f64>, children: Vec<PlanNode>) -> PlanNode {
        PlanNode {
            actual_time: Some(ActualTime {
                total_ms: total,
                exclusive_ms: exclusive,
                ..ActualTime::default()
            }),
            children,
            ..PlanNode::default()
        }
    }

    #[test]
    fn exclusive_follows_from_total_and_back() {
        // Parent reports a total, one child an exclusive time only (as
        // MariaDB tables do), its child a total.
        let mut root = timed(
            Some(10.0),
            None,
            vec![timed(None, Some(2.0), vec![timed(Some(3.0), None, vec![])])],
        );
        settle(&mut root);
        let time = |n: &PlanNode| n.actual_time.unwrap_or_default();
        assert_eq!(time(&root).exclusive_ms, Some(5.0));
        assert_eq!(time(&root.children[0]).total_ms, Some(5.0));
        assert_eq!(time(&root.children[0].children[0]).exclusive_ms, Some(3.0));
    }

    #[test]
    fn exclusive_never_goes_negative() {
        let mut root = timed(Some(1.0), None, vec![timed(Some(4.0), None, vec![])]);
        settle(&mut root);
        assert_eq!(root.actual_time.and_then(|t| t.exclusive_ms), Some(0.0));
    }

    #[test]
    fn misestimates_need_a_factor_of_ten() {
        let mut node = PlanNode {
            estimated_rows: Some(10.0),
            actual_rows: Some(99.0),
            loops: Some(1),
            children: vec![PlanNode {
                estimated_rows: Some(1.0),
                actual_rows: Some(0.0),
                loops: Some(3),
                children: vec![PlanNode {
                    estimated_rows: Some(5000.0),
                    actual_rows: Some(2.0),
                    loops: Some(1),
                    ..PlanNode::default()
                }],
                ..PlanNode::default()
            }],
            ..PlanNode::default()
        };
        misestimates(&mut node);
        assert!(node.warnings.is_empty());
        assert!(node.children[0].warnings.is_empty());
        assert_eq!(
            node.children[0].children[0].warnings,
            vec![NodeWarning::RowMisestimate {
                estimated: 5000.0,
                actual: 2.0,
                factor: 2500.0
            }]
        );
    }
}
