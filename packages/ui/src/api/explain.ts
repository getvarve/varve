import { call } from './invoke';

/** `ExplainFormat` in crates/catalog/src/explain/mod.rs. */
export type ExplainFormat = 'json' | 'tree';

/** `ExplainOptions` in crates/catalog/src/explain/mod.rs. `confirmed` is ignored by the app; the call's own `confirmed` counts. */
export interface ExplainOptions {
  analyze: boolean;
  buffers: boolean;
  verbose: boolean;
  settings: boolean;
  wal: boolean;
  /** `null` takes the engine default (JSON, except MySQL `EXPLAIN ANALYZE`: tree). */
  format: ExplainFormat | null;
  confirmed: boolean;
}

/** `ExplainCapabilities`: the options a server offers. */
export interface ExplainCapabilities {
  /** Formats for a plain `EXPLAIN`, default first. */
  formats: ExplainFormat[];
  analyze: boolean;
  /** Formats for `ANALYZE`, default first. */
  analyzeFormats: ExplainFormat[];
  buffers: boolean;
  verbose: boolean;
  settings: boolean;
  /** Only together with `analyze`. */
  wal: boolean;
}

/** `PlanFormat`: which parser read the server's output. */
export type PlanFormat = 'postgresJson' | 'mysqlJson' | 'mysqlTree' | 'mariadbJson';

/** `ShareBasis`: what `PlanNode.share` is measured in. */
export type ShareBasis = 'time' | 'cost' | 'none';

/** `NodeDetail`: fields every engine has in some form. */
export interface NodeDetail {
  relation: string | null;
  schema: string | null;
  alias: string | null;
  index: string | null;
  joinType: string | null;
  strategy: string | null;
  relationship: string | null;
  subplan: string | null;
  cte: string | null;
  function: string | null;
  filter: string | null;
  condition: string | null;
  sortKey: string[];
  groupKey: string[];
  parallelAware: boolean;
  workersPlanned: number | null;
  workersLaunched: number | null;
  output: string[];
}

/** `Cost`, in the engine's own units. */
export interface Cost {
  startup: number | null;
  /** Including the children. */
  total: number | null;
  /** The node itself, without its children. */
  exclusive: number | null;
}

/** `ActualTime`, in milliseconds. */
export interface ActualTime {
  firstRowMs: number | null;
  perLoopMs: number | null;
  /** Over all loops including the children (wall time for parallel nodes). */
  totalMs: number | null;
  /** `totalMs` without the children. */
  exclusiveMs: number | null;
}

/** `Buffers`: PostgreSQL block counts. */
export interface Buffers {
  sharedHit: number;
  sharedRead: number;
  sharedDirtied: number;
  sharedWritten: number;
  localHit: number;
  localRead: number;
  localDirtied: number;
  localWritten: number;
  tempRead: number;
  tempWritten: number;
}

export interface TriggerTime {
  name: string;
  relation: string | null;
  timeMs: number | null;
  calls: number | null;
}

export interface PlanProperty {
  name: string;
  value: string;
}

/** `NodeWarning`: a problem the catalog found in a node. */
export type NodeWarning =
  | { kind: 'rowMisestimate'; estimated: number; actual: number; factor: number }
  | { kind: 'externalSort'; method: string; spaceKb: number | null }
  | { kind: 'diskSpill'; batches: number | null; diskKb: number | null }
  | { kind: 'seqScanLargeTable'; relation: string | null; rows: number };

/** `PlanNode`: one operation of a plan. */
export interface PlanNode {
  /** Position in a depth-first walk, 0 for the root. */
  id: number;
  operation: string;
  detail: NodeDetail;
  /** Per loop. */
  estimatedRows: number | null;
  /** Per loop (an average over the loops). */
  actualRows: number | null;
  loops: number | null;
  neverExecuted: boolean;
  estimatedCost: Cost | null;
  actualTime: ActualTime | null;
  buffers: Buffers | null;
  properties: PlanProperty[];
  children: PlanNode[];
  /** Share of the plan's exclusive time or cost (see `Plan.shareBasis`), 0 to 1. */
  share: number | null;
  warnings: NodeWarning[];
}

/** `Plan`: a parsed execution plan. */
export interface Plan {
  format: PlanFormat;
  /** True when the statement ran (actual values present). */
  analyzed: boolean;
  root: PlanNode;
  planningMs: number | null;
  executionMs: number | null;
  totalCost: number | null;
  shareBasis: ShareBasis;
  planningBuffers: Buffers | null;
  triggers: TriggerTime[];
  settings: PlanProperty[];
  properties: PlanProperty[];
  warnings: string[];
  notices: string[];
  /** The server's output, as received. */
  raw: string;
}

export function explainCapabilities(sessionId: string): Promise<ExplainCapabilities> {
  return call<ExplainCapabilities>('explain_capabilities', { sessionId });
}

/**
 * Explains one statement on the user's session. With `options.analyze`, call
 * with `confirmed: false` first; a statement that cannot be proven free of side
 * effects rejects with `confirmationRequired` (every reason in `statements`).
 * The tab's `cancel` stops a running call.
 */
export function explain(sessionId: string, statement: string, options: ExplainOptions, confirmed: boolean): Promise<Plan> {
  return call<Plan>('explain', { sessionId, statement, options: { ...options, confirmed: false }, confirmed });
}
