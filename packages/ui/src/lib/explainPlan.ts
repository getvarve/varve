import type { Buffers, ExplainOptions, NodeWarning, Plan, PlanNode, ShareBasis } from '../api/explain';

/** A plan node with its place in the tree. */
export interface FlatNode {
  node: PlanNode;
  depth: number;
  parent: number | null;
}

/** Every node in depth-first order (the order of `PlanNode.id`). */
export function flattenPlan(root: PlanNode): FlatNode[] {
  const out: FlatNode[] = [];
  const walk = (node: PlanNode, depth: number, parent: number | null): void => {
    out.push({ node, depth, parent });
    for (const child of node.children) walk(child, depth + 1, node.id);
  };
  walk(root, 0, null);
  return out;
}

/** The node with this id, or `null`. */
export function findNode(root: PlanNode, id: number): PlanNode | null {
  if (root.id === id) return root;
  for (const child of root.children) {
    const hit = findNode(child, id);
    if (hit) return hit;
  }
  return null;
}

// Hot spots

export type Heat = 'hot' | 'warm' | 'cool';

/** Over half of the plan is hot, 10 to 50% warm, the rest cool (design: ExplainPlan legend). */
export function heatOf(fraction: number | null): Heat {
  if (fraction === null || !Number.isFinite(fraction)) return 'cool';
  if (fraction >= 0.5) return 'hot';
  if (fraction >= 0.1) return 'warm';
  return 'cool';
}

/** What the node cards highlight. */
export type HighlightMode = 'share' | 'misestimate' | 'buffersRead';

export const HIGHLIGHT_LABELS: Record<HighlightMode, string> = {
  share: 'Self time',
  misestimate: 'Rows misestimate',
  buffersRead: 'Blocks read',
};

/** The label of `share` for a plan: time when it ran, cost otherwise. */
export function shareLabel(basis: ShareBasis): string {
  if (basis === 'time') return 'Self time';
  if (basis === 'cost') return 'Self cost';
  return 'No time or cost';
}

/** Blocks read from disk (shared, local and temp). */
export function blocksRead(buffers: Buffers | null): number {
  if (!buffers) return 0;
  return buffers.sharedRead + buffers.localRead + buffers.tempRead;
}

/** How many times larger the bigger of estimated and actual rows is; `null` without both. */
export function misestimateFactor(node: PlanNode): number | null {
  const est = node.estimatedRows;
  const act = node.actualRows;
  if (est === null || act === null || node.neverExecuted) return null;
  // One row is the floor: 0 estimated against 0 actual is a perfect estimate.
  const a = Math.max(est, 1);
  const b = Math.max(act, 1);
  return Math.max(a, b) / Math.min(a, b);
}

/**
 * The 0 to 1 value each node shows for a highlight mode: its share of the
 * plan, its misestimate on a log scale against the worst one, or its share of
 * all blocks read. `null` when the plan has nothing to show for that mode.
 */
export function highlightValues(root: PlanNode, mode: HighlightMode): Map<number, number | null> {
  const nodes = flattenPlan(root).map((f) => f.node);
  const out = new Map<number, number | null>();
  if (mode === 'share') {
    for (const n of nodes) out.set(n.id, n.share);
    return out;
  }
  if (mode === 'misestimate') {
    const logs = nodes.map((n) => {
      const f = misestimateFactor(n);
      return f === null ? null : Math.log10(f);
    });
    const worst = Math.max(0, ...logs.filter((l): l is number => l !== null));
    nodes.forEach((n, i) => {
      const l = logs[i] ?? null;
      out.set(n.id, l === null ? null : worst === 0 ? 0 : l / worst);
    });
    return out;
  }
  // Buffers are inclusive of the children in PostgreSQL; the node's own reads are its count minus its children's.
  const own = (n: PlanNode): number => Math.max(0, blocksRead(n.buffers) - n.children.reduce((s, c) => s + blocksRead(c.buffers), 0));
  const total = nodes.reduce((s, n) => s + own(n), 0);
  for (const n of nodes) out.set(n.id, n.buffers === null ? null : total === 0 ? 0 : own(n) / total);
  return out;
}

/** Percent text for a 0 to 1 fraction: `82%`, `<1%` for small non-zero values. */
export function percentText(fraction: number | null): string {
  if (fraction === null || !Number.isFinite(fraction)) return '';
  if (fraction > 0 && fraction < 0.005) return '<1%';
  return `${Math.round(fraction * 100)}%`;
}

// Formatting

/** A number with thousands separators and at most `digits` decimals. */
export function formatNumber(n: number, digits = 0): string {
  return n.toLocaleString('en-US', { maximumFractionDigits: digits, minimumFractionDigits: 0 });
}

/** Milliseconds as `0.031 ms`, `48.1 ms`, `1,842.6 ms`. */
export function formatMs(ms: number): string {
  if (ms === 0) return '0 ms';
  if (Math.abs(ms) < 1) return `${ms.toFixed(3)} ms`;
  if (Math.abs(ms) < 10) return `${ms.toFixed(2)} ms`;
  return `${formatNumber(ms, 1)} ms`;
}

/** Rows as a whole number where it is one (averages over loops can be fractional). */
export function formatRows(rows: number): string {
  return Number.isInteger(rows) ? formatNumber(rows) : formatNumber(rows, 2);
}

/** PostgreSQL blocks are 8 kB unless the server was built otherwise. */
export const PG_BLOCK_BYTES = 8192;

/** The relation, index or other object a node works on, for the card's second column. */
export function nodeSubject(node: PlanNode): string {
  const d = node.detail;
  const parts: string[] = [];
  if (d.relation) {
    const name = d.schema ? `${d.schema}.${d.relation}` : d.relation;
    parts.push(d.alias && d.alias !== d.relation ? `on ${name} ${d.alias}` : `on ${name}`);
  } else if (d.cte) parts.push(`CTE ${d.cte}`);
  else if (d.function) parts.push(`function ${d.function}`);
  if (d.index) parts.push(`using ${d.index}`);
  if (parts.length > 0) return parts.join(' ');
  if (d.condition) return d.condition;
  if (d.sortKey.length > 0) return `key: ${d.sortKey.join(', ')}`;
  if (d.groupKey.length > 0) return `group: ${d.groupKey.join(', ')}`;
  if (d.subplan) return d.subplan;
  return '';
}

/** The operation name with its join type or strategy when the operation does not already say it. */
export function nodeTitle(node: PlanNode): string {
  const d = node.detail;
  const extra = [d.joinType, d.strategy].filter((s): s is string => !!s && !node.operation.toLowerCase().includes(s.toLowerCase()));
  return extra.length > 0 ? `${node.operation} (${extra.join(', ')})` : node.operation;
}

/** The self time (analyzed) or self cost of a node, as card text. */
export function nodeSelfText(node: PlanNode, basis: ShareBasis): string {
  if (node.neverExecuted) return 'never ran';
  if (basis === 'time') {
    const ms = node.actualTime?.exclusiveMs;
    return ms === null || ms === undefined ? '' : formatMs(ms);
  }
  if (basis === 'cost') {
    const c = node.estimatedCost?.exclusive;
    return c === null || c === undefined ? '' : `cost ${formatNumber(c, 2)}`;
  }
  return '';
}

/** A short title and explanation of a node warning. */
export function warningText(w: NodeWarning): { title: string; detail: string } {
  switch (w.kind) {
    case 'rowMisestimate':
      return {
        title: `Rows misestimated ${formatNumber(w.factor, 1)}x`,
        detail: `The planner expected ${formatRows(w.estimated)} rows per loop and got ${formatRows(w.actual)}. Stale statistics (run ANALYZE on the table) or correlated columns are the usual cause; the plan above this node may be a poor choice.`,
      };
    case 'externalSort':
      return {
        title: 'Sort spilled to disk',
        detail: `Sort method ${w.method}${w.spaceKb === null ? '' : `, ${formatNumber(w.spaceKb)} kB on disk`}. More sort memory (work_mem, sort_buffer_size) or an index that delivers the order avoids it.`,
      };
    case 'diskSpill':
      return {
        title: 'Hash spilled to disk',
        detail: `The hash table was split${w.batches === null ? '' : ` into ${formatNumber(w.batches)} batches`}${w.diskKb === null ? '' : ` using ${formatNumber(w.diskKb)} kB on disk`}. More hash memory (work_mem, hash_mem_multiplier) keeps it in memory.`,
      };
    case 'seqScanLargeTable':
      return {
        title: `Full scan of ${w.relation ?? 'a large table'}`,
        detail: `${formatRows(w.rows)} rows are read. When the filter keeps only a few of them, an index on the filtered columns lets the server read just those.`,
      };
  }
}

/** `EXPLAIN (ANALYZE, BUFFERS)` style label of what was asked for. */
export function explainLabel(options: Pick<ExplainOptions, 'analyze' | 'buffers' | 'verbose' | 'settings' | 'wal' | 'format'>): string {
  const flags: string[] = [];
  if (options.analyze) flags.push('ANALYZE');
  if (options.buffers) flags.push('BUFFERS');
  if (options.verbose) flags.push('VERBOSE');
  if (options.settings) flags.push('SETTINGS');
  if (options.wal) flags.push('WAL');
  if (options.format) flags.push(`FORMAT ${options.format.toUpperCase()}`);
  return flags.length > 0 ? `EXPLAIN (${flags.join(', ')})` : 'EXPLAIN';
}

/** One line per node, indented like PostgreSQL's text format. */
export function planOutline(plan: Plan): string {
  const lines: string[] = [];
  for (const { node, depth } of flattenPlan(plan.root)) {
    const parts = [nodeTitle(node)];
    const subject = nodeSubject(node);
    if (subject) parts.push(subject);
    const c = node.estimatedCost;
    const est: string[] = [];
    if (c?.startup !== null && c?.startup !== undefined && c.total !== null) est.push(`cost=${c.startup.toFixed(2)}..${c.total.toFixed(2)}`);
    else if (c?.total !== null && c?.total !== undefined) est.push(`cost=${c.total.toFixed(2)}`);
    if (node.estimatedRows !== null) est.push(`rows=${formatRows(node.estimatedRows)}`);
    if (est.length > 0) parts.push(`(${est.join(' ')})`);
    if (node.neverExecuted) parts.push('(never executed)');
    else {
      const t = node.actualTime;
      const act: string[] = [];
      if (t?.firstRowMs !== null && t?.firstRowMs !== undefined && t.perLoopMs !== null) {
        act.push(`time=${t.firstRowMs.toFixed(3)}..${t.perLoopMs.toFixed(3)}`);
      } else if (t?.totalMs !== null && t?.totalMs !== undefined) act.push(`time=${t.totalMs.toFixed(3)}`);
      if (node.actualRows !== null) act.push(`rows=${formatRows(node.actualRows)}`);
      if (node.loops !== null) act.push(`loops=${node.loops}`);
      if (act.length > 0) parts.push(`(actual ${act.join(' ')})`);
    }
    const indent = depth === 0 ? '' : `${'      '.repeat(depth - 1)}  ->  `;
    lines.push(indent + parts.join(' '));
    const pad = depth === 0 ? '  ' : `${'      '.repeat(depth)}  `;
    if (node.detail.condition && node.detail.condition !== subject) lines.push(`${pad}Cond: ${node.detail.condition}`);
    if (node.detail.filter) lines.push(`${pad}Filter: ${node.detail.filter}`);
  }
  if (plan.planningMs !== null) lines.push(`Planning Time: ${plan.planningMs.toFixed(3)} ms`);
  if (plan.executionMs !== null) lines.push(`Execution Time: ${plan.executionMs.toFixed(3)} ms`);
  return lines.join('\n');
}

// Tree layout (Visual view)

export const CARD_WIDTH = 340;
export const CARD_HEIGHT = 72;
export const COLUMN_GAP = 20;
export const ROW_GAP = 12;
export const MARGIN = 24;

export interface PlacedNode {
  id: number;
  x: number;
  y: number;
}

export interface PlanLayout {
  nodes: PlacedNode[];
  /** SVG paths from each parent to its children. */
  edges: { from: number; to: number; d: string }[];
  width: number;
  height: number;
}

/**
 * Lays the plan out top-down: a node's first child sits below it in the same
 * column, every further child opens a column to the right of its earlier
 * siblings' subtrees (the layout of the ExplainPlan design).
 */
export function layoutPlan(root: PlanNode): PlanLayout {
  const nodes: PlacedNode[] = [];
  const edges: PlanLayout['edges'] = [];
  const pos = new Map<number, PlacedNode>();
  let maxCol = 0;
  let maxDepth = 0;
  const colX = (col: number): number => MARGIN + col * (CARD_WIDTH + COLUMN_GAP);
  const rowY = (depth: number): number => MARGIN / 2 + depth * (CARD_HEIGHT + ROW_GAP);

  /** Places `node` at `col` and returns the last column its subtree uses. */
  const place = (node: PlanNode, depth: number, col: number): number => {
    const placed = { id: node.id, x: colX(col), y: rowY(depth) };
    nodes.push(placed);
    pos.set(node.id, placed);
    maxCol = Math.max(maxCol, col);
    maxDepth = Math.max(maxDepth, depth);
    let last = col;
    node.children.forEach((child, i) => {
      last = place(child, depth + 1, i === 0 ? col : last + 1);
    });
    return last;
  };
  place(root, 0, 0);

  const stub = 36;
  for (const { node } of flattenPlan(root)) {
    const p = pos.get(node.id);
    if (!p) continue;
    for (const child of node.children) {
      const c = pos.get(child.id);
      if (!c) continue;
      const x1 = p.x + stub;
      const y1 = p.y + CARD_HEIGHT;
      const x2 = c.x + stub;
      const y2 = c.y;
      const mid = y1 + (y2 - y1) / 2;
      const d = x1 === x2 ? `M${x1} ${y1} V${y2}` : `M${x1} ${y1} V${mid} H${x2} V${y2}`;
      edges.push({ from: node.id, to: child.id, d });
    }
  }
  return {
    nodes,
    edges,
    width: colX(maxCol) + CARD_WIDTH + MARGIN,
    height: rowY(maxDepth) + CARD_HEIGHT + MARGIN,
  };
}

// Timeline (icicle) view

export interface TimelineBar {
  id: number;
  depth: number;
  /** 0 to 1 within the whole plan. */
  start: number;
  /** 0 to 1 within the whole plan. */
  width: number;
  /** The inclusive time (ms) or cost the width stands for. */
  value: number;
}

export interface Timeline {
  basis: 'time' | 'cost';
  total: number;
  bars: TimelineBar[];
  depth: number;
}

/** Inclusive time (analyzed) or cost of a node; `null` when unknown. */
function inclusive(node: PlanNode, basis: 'time' | 'cost'): number | null {
  if (basis === 'time') return node.neverExecuted ? 0 : (node.actualTime?.totalMs ?? null);
  return node.estimatedCost?.total ?? null;
}

/**
 * The plan as nested bars: the root spans the whole width, each child sits
 * inside its parent with a width proportional to its inclusive time (or cost,
 * when the plan did not run). Children that together exceed their parent
 * (parallel workers, init plans counted twice) are scaled to fit. `null` when
 * the plan has neither times nor costs.
 */
export function planTimeline(plan: Plan): Timeline | null {
  const basis: 'time' | 'cost' | null = plan.analyzed && inclusive(plan.root, 'time') !== null ? 'time' : inclusive(plan.root, 'cost') !== null ? 'cost' : null;
  if (basis === null) return null;
  const total = inclusive(plan.root, basis) ?? 0;
  const bars: TimelineBar[] = [];
  let maxDepth = 0;
  const walk = (node: PlanNode, depth: number, start: number, width: number, value: number): void => {
    bars.push({ id: node.id, depth, start, width, value });
    maxDepth = Math.max(maxDepth, depth);
    const values = node.children.map((c) => Math.max(0, inclusive(c, basis) ?? 0));
    const sum = values.reduce((s, v) => s + v, 0);
    if (sum === 0) {
      // Nothing measured below: children share the parent equally so they stay visible.
      const each = node.children.length > 0 ? width / node.children.length : 0;
      node.children.forEach((c, i) => walk(c, depth + 1, start + i * each, each, values[i] ?? 0));
      return;
    }
    const scale = value > 0 && sum <= value ? width / value : width / sum;
    let at = start;
    node.children.forEach((c, i) => {
      const v = values[i] ?? 0;
      walk(c, depth + 1, at, v * scale, v);
      at += v * scale;
    });
  };
  walk(plan.root, 0, 0, 1, total);
  return { basis, total, bars, depth: maxDepth + 1 };
}
