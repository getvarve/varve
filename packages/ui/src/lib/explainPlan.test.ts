import { describe, expect, it } from 'vitest';
import type { Plan, PlanNode } from '../api/explain';
import {
  CARD_HEIGHT,
  CARD_WIDTH,
  COLUMN_GAP,
  MARGIN,
  explainLabel,
  findNode,
  flattenPlan,
  formatMs,
  heatOf,
  highlightValues,
  layoutPlan,
  misestimateFactor,
  nodeSelfText,
  nodeSubject,
  nodeTitle,
  percentText,
  planOutline,
  planTimeline,
  warningText,
} from './explainPlan';

function node(id: number, operation: string, extra: Partial<PlanNode> = {}): PlanNode {
  return {
    id,
    operation,
    detail: {
      relation: null,
      schema: null,
      alias: null,
      index: null,
      joinType: null,
      strategy: null,
      relationship: null,
      subplan: null,
      cte: null,
      function: null,
      filter: null,
      condition: null,
      sortKey: [],
      groupKey: [],
      parallelAware: false,
      workersPlanned: null,
      workersLaunched: null,
      output: [],
    },
    estimatedRows: null,
    actualRows: null,
    loops: null,
    neverExecuted: false,
    estimatedCost: null,
    actualTime: null,
    buffers: null,
    properties: [],
    children: [],
    share: null,
    warnings: [],
    ...extra,
  };
}

function time(totalMs: number, exclusiveMs: number): PlanNode['actualTime'] {
  return { firstRowMs: 0.01, perLoopMs: totalMs, totalMs, exclusiveMs };
}

function buffers(sharedRead: number, sharedHit = 0): PlanNode['buffers'] {
  return {
    sharedHit,
    sharedRead,
    sharedDirtied: 0,
    sharedWritten: 0,
    localHit: 0,
    localRead: 0,
    localDirtied: 0,
    localWritten: 0,
    tempRead: 0,
    tempWritten: 0,
  };
}

/**
 * The plan of the design: Sort > HashAggregate > Hash Join > (Seq Scan, Hash > Index Scan),
 * with the depth-first ids the backend assigns.
 */
function samplePlan(): Plan {
  const seq = node(3, 'Seq Scan', {
    detail: { ...node(0, '').detail, relation: 'orders', alias: 'o', filter: "(status = 'paid')" },
    estimatedRows: 121_880,
    actualRows: 133_212,
    loops: 1,
    actualTime: time(1511.8, 1511.8),
    buffers: buffers(1_602_330, 214_108),
    estimatedCost: { startup: 0, total: 2_104_883.4, exclusive: 2_104_883.4 },
    share: 0.82,
    warnings: [{ kind: 'seqScanLargeTable', relation: 'orders', rows: 4_100_000 }],
  });
  const idx = node(5, 'Index Scan', {
    detail: { ...node(0, '').detail, relation: 'customers', alias: 'c', index: 'customers_is_test_idx' },
    estimatedRows: 309_200,
    actualRows: 309_688,
    loops: 1,
    actualTime: time(64, 64),
    buffers: buffers(3881, 9412),
    share: 0.03,
  });
  const hash = node(4, 'Hash', { actualTime: time(82.3, 18.3), children: [idx], share: 0.01, estimatedRows: 309_200, actualRows: 309_688, buffers: buffers(3881, 9412) });
  const join = node(2, 'Hash Join', {
    detail: { ...node(0, '').detail, joinType: 'Inner', condition: '(o.customer_id = c.id)' },
    actualTime: time(1790.5, 196.4),
    children: [seq, hash],
    share: 0.11,
    estimatedRows: 118_240,
    actualRows: 131_977,
    buffers: buffers(1_606_211, 223_520),
  });
  const agg = node(1, 'HashAggregate', {
    detail: { ...node(0, '').detail, groupKey: ["date_trunc('day', o.created_at)"] },
    actualTime: time(1838.6, 48.1),
    children: [join],
    share: 0.03,
    estimatedRows: 31,
    actualRows: 30,
    buffers: buffers(1_606_211, 223_520),
  });
  const sort = node(0, 'Sort', {
    detail: { ...node(0, '').detail, sortKey: ['day DESC'] },
    actualTime: time(1838.8, 0.2),
    children: [agg],
    share: 0,
    estimatedRows: 31,
    actualRows: 30,
    loops: 1,
    estimatedCost: { startup: 2_200_000, total: 2_200_001.5, exclusive: 1.5 },
    buffers: buffers(1_606_211, 223_520),
  });
  return {
    format: 'postgresJson',
    analyzed: true,
    root: sort,
    planningMs: 0.41,
    executionMs: 1842.6,
    totalCost: 2_200_001.5,
    shareBasis: 'time',
    planningBuffers: null,
    triggers: [],
    settings: [],
    properties: [],
    warnings: [],
    notices: [],
    raw: '[]',
  };
}

describe('flattenPlan and findNode', () => {
  it('walks depth first in id order with depth and parent', () => {
    const flat = flattenPlan(samplePlan().root);
    expect(flat.map((f) => f.node.id)).toEqual([0, 1, 2, 3, 4, 5]);
    expect(flat.map((f) => f.depth)).toEqual([0, 1, 2, 3, 3, 4]);
    expect(flat.map((f) => f.parent)).toEqual([null, 0, 1, 2, 2, 4]);
  });

  it('finds a node by id and returns null for an unknown id', () => {
    const root = samplePlan().root;
    expect(findNode(root, 4)?.operation).toBe('Hash');
    expect(findNode(root, 99)).toBeNull();
  });
});

describe('hot spots', () => {
  it('classifies shares at the 50% and 10% boundaries', () => {
    expect(heatOf(0.82)).toBe('hot');
    expect(heatOf(0.5)).toBe('hot');
    expect(heatOf(0.4999)).toBe('warm');
    expect(heatOf(0.1)).toBe('warm');
    expect(heatOf(0.09)).toBe('cool');
    expect(heatOf(null)).toBe('cool');
    expect(heatOf(Number.NaN)).toBe('cool');
  });

  it('formats percentages, marking tiny non-zero shares', () => {
    expect(percentText(0.82)).toBe('82%');
    expect(percentText(0)).toBe('0%');
    expect(percentText(0.001)).toBe('<1%');
    expect(percentText(null)).toBe('');
  });

  it('uses the backend share for self time', () => {
    const values = highlightValues(samplePlan().root, 'share');
    expect(values.get(3)).toBe(0.82);
    expect(values.get(0)).toBe(0);
  });

  it('scores misestimates on a log scale against the worst node', () => {
    const plan = samplePlan();
    const worst = findNode(plan.root, 5) as PlanNode;
    worst.estimatedRows = 10;
    worst.actualRows = 10_000;
    const values = highlightValues(plan.root, 'misestimate');
    expect(values.get(5)).toBe(1);
    expect(values.get(3)).toBeGreaterThan(0);
    expect(values.get(3)).toBeLessThan(0.1);
  });

  it('counts each node only for the blocks it read itself (buffers are inclusive)', () => {
    const values = highlightValues(samplePlan().root, 'buffersRead');
    // Everything was read by the two scans; the joins and the sort read nothing themselves.
    expect(values.get(0)).toBe(0);
    expect(values.get(2)).toBe(0);
    expect(values.get(4)).toBe(0);
    expect((values.get(3) ?? 0) + (values.get(5) ?? 0)).toBeCloseTo(1, 10);
    expect(values.get(3)).toBeGreaterThan(0.99);
  });

  it('computes the misestimate factor with a one-row floor', () => {
    expect(misestimateFactor(node(0, 'x', { estimatedRows: 100, actualRows: 1000 }))).toBe(10);
    expect(misestimateFactor(node(0, 'x', { estimatedRows: 1000, actualRows: 100 }))).toBe(10);
    expect(misestimateFactor(node(0, 'x', { estimatedRows: 0, actualRows: 0 }))).toBe(1);
    expect(misestimateFactor(node(0, 'x', { estimatedRows: 5 }))).toBeNull();
    expect(misestimateFactor(node(0, 'x', { estimatedRows: 5, actualRows: 0, neverExecuted: true }))).toBeNull();
  });
});

describe('node text', () => {
  it('names the relation with alias, index, condition or keys', () => {
    const plan = samplePlan();
    expect(nodeSubject(findNode(plan.root, 3) as PlanNode)).toBe('on orders o');
    expect(nodeSubject(findNode(plan.root, 5) as PlanNode)).toBe('on customers c using customers_is_test_idx');
    expect(nodeSubject(findNode(plan.root, 2) as PlanNode)).toBe('(o.customer_id = c.id)');
    expect(nodeSubject(plan.root)).toBe('key: day DESC');
    expect(nodeSubject(findNode(plan.root, 1) as PlanNode)).toBe("group: date_trunc('day', o.created_at)");
  });

  it('adds the join type unless the operation already says it', () => {
    const plan = samplePlan();
    expect(nodeTitle(findNode(plan.root, 2) as PlanNode)).toBe('Hash Join (Inner)');
    const n = node(0, 'Left Hash Join', { detail: { ...node(0, '').detail, joinType: 'Left' } });
    expect(nodeTitle(n)).toBe('Left Hash Join');
  });

  it('shows self time, self cost or never ran', () => {
    const plan = samplePlan();
    expect(nodeSelfText(findNode(plan.root, 3) as PlanNode, 'time')).toBe('1,511.8 ms');
    expect(nodeSelfText(plan.root, 'cost')).toBe('cost 1.5');
    expect(nodeSelfText(node(0, 'x', { neverExecuted: true }), 'time')).toBe('never ran');
    expect(nodeSelfText(plan.root, 'none')).toBe('');
  });

  it('formats milliseconds by size', () => {
    expect(formatMs(0)).toBe('0 ms');
    expect(formatMs(0.031)).toBe('0.031 ms');
    expect(formatMs(2.5)).toBe('2.50 ms');
    expect(formatMs(1842.63)).toBe('1,842.6 ms');
  });

  it('labels the options that were sent', () => {
    expect(explainLabel({ analyze: true, buffers: true, verbose: false, settings: false, wal: false, format: null })).toBe('EXPLAIN (ANALYZE, BUFFERS)');
    expect(explainLabel({ analyze: false, buffers: false, verbose: false, settings: false, wal: false, format: null })).toBe('EXPLAIN');
    expect(explainLabel({ analyze: true, buffers: false, verbose: false, settings: false, wal: false, format: 'tree' })).toBe('EXPLAIN (ANALYZE, FORMAT TREE)');
  });

  it('explains every warning kind', () => {
    expect(warningText({ kind: 'rowMisestimate', estimated: 10, actual: 1000, factor: 100 }).title).toBe('Rows misestimated 100x');
    expect(warningText({ kind: 'externalSort', method: 'external merge', spaceKb: 2048 }).detail).toContain('2,048 kB');
    expect(warningText({ kind: 'diskSpill', batches: 8, diskKb: null }).detail).toContain('8 batches');
    expect(warningText({ kind: 'seqScanLargeTable', relation: 'orders', rows: 4_100_000 }).title).toBe('Full scan of orders');
  });

  it('writes an indented outline like the text format', () => {
    const text = planOutline(samplePlan());
    const lines = text.split('\n');
    expect(lines[0]).toMatch(/^Sort key: day DESC \(cost=2200000\.00\.\.2200001\.50 rows=31\) \(actual time=0\.010\.\.1838\.800 rows=30 loops=1\)$/);
    expect(lines.some((l) => l.startsWith('              ->  Seq Scan on orders o'))).toBe(true);
    expect(lines).toContain("                    Filter: (status = 'paid')");
    expect(lines.at(-2)).toBe('Planning Time: 0.410 ms');
    expect(lines.at(-1)).toBe('Execution Time: 1842.600 ms');
  });
});

describe('layoutPlan', () => {
  it('keeps the first child in the parent column and opens a column for later children', () => {
    const layout = layoutPlan(samplePlan().root);
    const at = new Map(layout.nodes.map((n) => [n.id, n]));
    const col = (id: number): number => ((at.get(id)?.x ?? 0) - MARGIN) / (CARD_WIDTH + COLUMN_GAP);
    expect([0, 1, 2, 3].map(col)).toEqual([0, 0, 0, 0]);
    expect(col(4)).toBe(1);
    expect(col(5)).toBe(1);
    expect(at.get(3)?.y).toBe(at.get(4)?.y);
    expect(layout.width).toBe(MARGIN + 1 * (CARD_WIDTH + COLUMN_GAP) + CARD_WIDTH + MARGIN);
    expect(layout.height).toBeGreaterThan((at.get(5)?.y ?? 0) + CARD_HEIGHT);
  });

  it('opens a column after the whole subtree of an earlier sibling', () => {
    const wide = node(0, 'Append', {
      children: [node(1, 'A', { children: [node(2, 'A1'), node(3, 'A2')] }), node(4, 'B')],
    });
    const at = new Map(layoutPlan(wide).nodes.map((n) => [n.id, n.x]));
    expect(at.get(3)).toBeGreaterThan(at.get(2) ?? 0);
    // B sits right of A2, the last column of A's subtree.
    expect(at.get(4)).toBeGreaterThan(at.get(3) ?? 0);
  });

  it('draws straight edges in a column and elbows across columns', () => {
    const layout = layoutPlan(samplePlan().root);
    const straight = layout.edges.find((e) => e.from === 2 && e.to === 3);
    const elbow = layout.edges.find((e) => e.from === 2 && e.to === 4);
    expect(straight?.d).toMatch(/^M\d+ \d+ V\d+$/);
    expect(elbow?.d).toMatch(/^M\d+ \d+ V[\d.]+ H\d+ V\d+$/);
    expect(layout.edges).toHaveLength(5);
  });
});

describe('planTimeline', () => {
  it('nests children by inclusive time inside their parent', () => {
    const t = planTimeline(samplePlan());
    expect(t?.basis).toBe('time');
    expect(t?.total).toBe(1838.8);
    const bar = new Map(t?.bars.map((b) => [b.id, b]));
    expect(bar.get(0)).toMatchObject({ start: 0, width: 1, depth: 0 });
    const join = bar.get(2);
    const seq = bar.get(3);
    const hash = bar.get(4);
    expect(seq?.start).toBeCloseTo(join?.start ?? -1, 10);
    expect((hash?.start ?? 0) + (hash?.width ?? 0)).toBeLessThanOrEqual((join?.start ?? 0) + (join?.width ?? 0) + 1e-9);
    expect(hash?.start).toBeCloseTo((seq?.start ?? 0) + (seq?.width ?? 0), 10);
    expect(t?.depth).toBe(5);
  });

  it('scales children that add up to more than their parent (parallel workers)', () => {
    const plan = samplePlan();
    plan.root = node(0, 'Gather', {
      actualTime: time(100, 10),
      children: [node(1, 'A', { actualTime: time(90, 90) }), node(2, 'B', { actualTime: time(90, 90) })],
    });
    const t = planTimeline(plan);
    const bars = t?.bars.filter((b) => b.depth === 1) ?? [];
    expect(bars.reduce((s, b) => s + b.width, 0)).toBeCloseTo(1, 10);
    expect(bars[0]?.width).toBeCloseTo(0.5, 10);
  });

  it('falls back to cost without times and to equal shares without either below', () => {
    const plan = samplePlan();
    plan.analyzed = false;
    plan.root = node(0, 'Nested Loop', {
      estimatedCost: { startup: 0, total: 100, exclusive: 20 },
      children: [node(1, 'A', { estimatedCost: { startup: 0, total: 60, exclusive: 60 } }), node(2, 'B', { estimatedCost: { startup: 0, total: 20, exclusive: 20 } })],
    });
    const t = planTimeline(plan);
    expect(t?.basis).toBe('cost');
    expect(t?.bars.find((b) => b.id === 1)?.width).toBeCloseTo(0.6, 10);

    plan.root = node(0, 'X', { estimatedCost: { startup: 0, total: 10, exclusive: 10 }, children: [node(1, 'A'), node(2, 'B')] });
    const even = planTimeline(plan);
    expect(even?.bars.find((b) => b.id === 2)).toMatchObject({ start: 0.5, width: 0.5 });
  });

  it('returns null for a plan with neither times nor costs (MariaDB without ANALYZE)', () => {
    const plan = samplePlan();
    plan.analyzed = false;
    plan.root = node(0, 'ALL', { children: [node(1, 'ref')] });
    expect(planTimeline(plan)).toBeNull();
  });
});
