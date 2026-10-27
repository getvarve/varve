import type { CmdError } from '../api/errors';
import type { Engine } from '../api/records';
import type { BigKey, ClientInfo, ConfigEntry, EditScope, MemoryAnalysis, MemoryScanOptions, PrefixUsage, Process, ServerVariable, SlotRange } from '../api/live';

// Durations

/** Seconds as `42s`, `30m 43s`, `5h 02m`, `3d 4h`. */
/**
 * True when polling a live panel cannot succeed later: the server does not
 * support the panel (`live/unsupported`, for example Galera status on a node
 * without wsrep). The panel stops refreshing; Refresh still asks again.
 */
export function stopsPolling(error: CmdError): boolean {
  return (error.kind === 'live' || error.kind === 'catalog') && error.error.kind === 'unsupported';
}

export function formatSeconds(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, '0')}s`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h ${String(m % 60).padStart(2, '0')}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** Milliseconds as a short duration; sub-second values in ms. */
export function formatAgeMs(ms: number | null): string {
  if (ms === null) return '';
  if (ms < 1000) return `${Math.max(0, Math.round(ms))} ms`;
  return formatSeconds(ms / 1000);
}

// Processes

/** True for PostgreSQL workers and MySQL/MariaDB system threads (not client connections). */
export function isBackground(p: Process, engine: Engine): boolean {
  if (engine === 'postgres') return p.backendType !== null && p.backendType !== 'client backend';
  const user = (p.user ?? '').toLowerCase();
  const command = (p.command ?? '').toLowerCase();
  return user === 'system user' || user === 'event_scheduler' || command === 'daemon' || command.startsWith('binlog dump') || command === 'slave_io' || command === 'slave_sql' || command === 'slave_worker';
}

/** True for a connection that waits for its client: PostgreSQL `idle`, MySQL/MariaDB `Sleep`. */
export function isIdle(p: Process, engine: Engine): boolean {
  if (engine === 'postgres') return p.state === 'idle';
  return (p.command ?? '').toLowerCase() === 'sleep';
}

/** True when a statement (or an open transaction) runs: PostgreSQL non-idle states, MySQL/MariaDB non-Sleep commands. */
export function isRunning(p: Process, engine: Engine): boolean {
  if (engine === 'postgres') return p.state === 'active' || p.state === 'fastpath function call';
  const command = (p.command ?? '').toLowerCase();
  return command === 'query' || command === 'execute';
}

/** PostgreSQL state chips, in the order the panel shows them. */
export const PG_STATES = ['active', 'idle in transaction', 'idle in transaction (aborted)', 'idle', 'other'] as const;
export type PgStateFilter = (typeof PG_STATES)[number];

/** The chip a PostgreSQL backend counts under. */
export function pgStateOf(p: Process): PgStateFilter {
  const s = p.state ?? '';
  return (PG_STATES as readonly string[]).includes(s) ? (s as PgStateFilter) : 'other';
}

export interface ProcessFilter {
  text: string;
  hideIdle: boolean;
  showBackground: boolean;
  includeOwn: boolean;
  /** PostgreSQL state chips to show; empty shows every state. */
  states: ReadonlySet<PgStateFilter>;
}

/** The processes the table shows. */
export function filterProcesses(processes: readonly Process[], engine: Engine, filter: ProcessFilter): Process[] {
  const needle = filter.text.trim().toLowerCase();
  return processes.filter((p) => {
    if (p.own && !filter.includeOwn) return false;
    if (!filter.showBackground && isBackground(p, engine)) return false;
    if (filter.hideIdle && isIdle(p, engine)) return false;
    if (engine === 'postgres' && filter.states.size > 0 && !filter.states.has(pgStateOf(p))) return false;
    if (!needle) return true;
    const hay = [String(p.id), p.user, p.database, p.application, p.clientAddress, p.clientHostname, p.query, p.state, p.command]
      .filter((s): s is string => !!s)
      .join('\n')
      .toLowerCase();
    return hay.includes(needle);
  });
}

/** True when the process has run longer than the threshold in a non-idle state. */
export function isLongRunning(p: Process, engine: Engine, thresholdMs: number): boolean {
  if (p.durationMs === null || thresholdMs <= 0) return false;
  if (isIdle(p, engine) || isBackground(p, engine)) return false;
  return p.durationMs >= thresholdMs;
}

/** Sort keys the table offers. */
export type ProcessSort = 'id' | 'duration' | 'user' | 'state';

export function sortProcesses(processes: readonly Process[], key: ProcessSort, descending: boolean): Process[] {
  const value = (p: Process): number | string => {
    switch (key) {
      case 'id':
        return p.id;
      case 'duration':
        return p.durationMs ?? -1;
      case 'user':
        return (p.user ?? '').toLowerCase();
      case 'state':
        return (p.state ?? p.command ?? '').toLowerCase();
    }
  };
  const out = [...processes].sort((a, b) => {
    const x = value(a);
    const y = value(b);
    const c = x < y ? -1 : x > y ? 1 : a.id - b.id;
    return descending ? -c : c;
  });
  return out;
}

/** One session in a blocking chain. */
export interface ChainLink {
  process: Process;
  depth: number;
  /** The session it waits for; `null` for the root blocker. */
  waitsFor: number | null;
}

/**
 * Blocking chains from `blockedBy` (PostgreSQL `pg_blocking_pids`): each
 * chain starts at a root blocker (blocks others, waits for nobody) and lists
 * the sessions waiting on it, depth first. Sessions in a wait cycle (a
 * deadlock not yet broken) form a chain that starts at the lowest id of the
 * cycle. Every waiting session appears in exactly one chain.
 */
export function blockingChains(processes: readonly Process[]): ChainLink[][] {
  const byId = new Map(processes.map((p) => [p.id, p]));
  const waiters = new Map<number, number[]>();
  for (const p of processes) {
    for (const b of p.blockedBy) {
      if (!byId.has(b)) continue;
      const list = waiters.get(b) ?? [];
      list.push(p.id);
      waiters.set(b, list);
    }
  }
  const blocked = (p: Process): boolean => p.blockedBy.some((b) => byId.has(b));
  const seen = new Set<number>();
  const chains: ChainLink[][] = [];
  const build = (rootId: number): ChainLink[] => {
    const chain: ChainLink[] = [];
    const visit = (id: number, depth: number, waitsFor: number | null): void => {
      if (seen.has(id)) return;
      const process = byId.get(id);
      if (!process) return;
      seen.add(id);
      chain.push({ process, depth, waitsFor });
      for (const w of [...(waiters.get(id) ?? [])].sort((a, b) => a - b)) visit(w, depth + 1, id);
    };
    visit(rootId, 0, null);
    return chain;
  };
  const roots = processes.filter((p) => waiters.has(p.id) && !blocked(p)).sort((a, b) => a.id - b.id);
  for (const r of roots) chains.push(build(r.id));
  // What is left waits in a cycle.
  for (const p of [...processes].sort((a, b) => a.id - b.id)) {
    if (!seen.has(p.id) && blocked(p) && waiters.has(p.id)) chains.push(build(p.id));
  }
  // Waiters whose blockers are not in a chain yet (their blocker is in a cycle already visited) are covered by `seen`.
  return chains.filter((c) => c.length > 1);
}

// Server variables

/** True when the value differs from the server's built-in default (or its source says it was set). */
export function isChanged(v: ServerVariable): boolean {
  if (v.masked) return v.source !== null && !isDefaultSource(v.source);
  if (v.defaultValue !== null && v.value !== null) return normalize(v.value) !== normalize(v.defaultValue);
  if (v.source !== null) return !isDefaultSource(v.source);
  return false;
}

function isDefaultSource(source: string): boolean {
  const s = source.toLowerCase();
  return s === 'default' || s === 'compiled' || s === 'override';
}

function normalize(value: string): string {
  return value.trim().toLowerCase();
}

const PREFIX_CATEGORIES: readonly [RegExp, string][] = [
  [/^innodb_/, 'InnoDB'],
  [/^wsrep_/, 'Galera (wsrep)'],
  [/^(binlog_|log_bin|relay_|rpl_|gtid_|replica_|replicate_|slave_|master_|source_|sync_binlog|server_id)/, 'Replication'],
  [/^(log_|general_log|slow_query|long_query_time|log$)/, 'Logging'],
  [/^performance_schema/, 'Performance Schema'],
  [/^(ssl_|tls_|require_secure)/, 'TLS'],
  [/^(character_set|collation_)/, 'Character sets'],
  [/^(query_cache|table_open_cache|table_definition_cache|thread_cache)/, 'Caches'],
  [/^(optimizer_|join_|sort_|read_rnd|max_seeks)/, 'Optimizer'],
  [/^aria_/, 'Aria'],
  [/^myisam_|^key_/, 'MyISAM'],
];

/** PostgreSQL's own category; for MySQL/MariaDB a group derived from the name. */
export function variableCategory(v: ServerVariable): string {
  if (v.category) return v.category;
  const name = v.name.toLowerCase();
  for (const [pattern, label] of PREFIX_CATEGORIES) if (pattern.test(name)) return label;
  return 'General';
}

export interface VariableFilter {
  text: string;
  /** `''` for all categories. */
  category: string;
  changedOnly: boolean;
  dynamicOnly: boolean;
}

export function filterVariables(vars: readonly ServerVariable[], filter: VariableFilter): ServerVariable[] {
  const needle = filter.text.trim().toLowerCase();
  return vars.filter((v) => {
    if (filter.category && variableCategory(v) !== filter.category) return false;
    if (filter.changedOnly && !isChanged(v)) return false;
    if (filter.dynamicOnly && (v.readOnly || v.dynamic === false || v.restartRequired)) return false;
    if (!needle) return true;
    return v.name.toLowerCase().includes(needle) || (v.description ?? '').toLowerCase().includes(needle) || (!v.masked && (v.value ?? '').toLowerCase().includes(needle));
  });
}

/** How a scope's change is written on this engine. */
export function scopeLabel(scope: EditScope, engine: Engine): string {
  switch (scope) {
    case 'session':
      return 'SET (this session)';
    case 'global':
      return engine === 'postgres' ? 'ALTER SYSTEM' : 'SET GLOBAL';
    case 'persist':
      return 'SET PERSIST';
    case 'persistOnly':
      return 'SET PERSIST_ONLY';
  }
}

/** A short description of what the change kind means (the design's "Change" column). */
export function changeKind(v: ServerVariable): string {
  if (v.readOnly) return 'Read only';
  if (v.restartRequired) return 'Restart';
  if (v.dynamic === false) return 'Restart';
  if (v.context === 'sighup') return 'Reload';
  if (v.settableScopes.length === 0) return 'Not settable';
  return 'Dynamic';
}

const BOOLEAN_WORDS = ['on', 'off', 'true', 'false', 'yes', 'no', '1', '0'];

/**
 * A quick check of a typed value against the variable's type, range and
 * allowed values, so obvious mistakes are caught before planning; the server
 * checks again. `null` when it looks valid.
 */
export function checkVariableValue(v: ServerVariable, input: string): string | null {
  const value = input.trim();
  if (value === '' && v.varType !== 'string' && v.varType !== 'set') return 'Enter a value.';
  switch (v.varType) {
    case 'boolean':
      return BOOLEAN_WORDS.includes(value.toLowerCase()) ? null : 'Enter ON or OFF.';
    case 'integer':
    case 'real': {
      // PostgreSQL accepts a unit after the number (`128MB`, `5min`); the server converts it.
      const match = /^(-?\d+(?:\.\d+)?(?:e[+-]?\d+)?)\s*([a-zA-Z]*)$/i.exec(value);
      if (!match) return v.varType === 'integer' ? 'Enter a whole number.' : 'Enter a number.';
      const [, num = '', unit = ''] = match;
      if (unit && v.unit === null) return 'This variable takes a plain number, without a unit.';
      if (v.varType === 'integer' && !unit && !/^-?\d+$/.test(num)) return 'Enter a whole number.';
      if (!unit) {
        const n = Number(num);
        if (v.minValue !== null && Number.isFinite(Number(v.minValue)) && n < Number(v.minValue)) return `The smallest allowed value is ${v.minValue}.`;
        if (v.maxValue !== null && Number.isFinite(Number(v.maxValue)) && n > Number(v.maxValue)) return `The largest allowed value is ${v.maxValue}.`;
      }
      return null;
    }
    case 'enum':
      if (v.enumValues.length === 0) return null;
      return v.enumValues.some((e) => e.toLowerCase() === value.toLowerCase()) ? null : `Choose one of: ${v.enumValues.join(', ')}.`;
    case 'set': {
      if (v.enumValues.length === 0 || value === '') return null;
      const bad = value
        .split(',')
        .map((s) => s.trim())
        .filter((s) => s !== '' && !v.enumValues.some((e) => e.toLowerCase() === s.toLowerCase().replace(/=(on|off|default)$/, '')));
      return bad.length === 0 ? null : `Not allowed: ${bad.join(', ')}.`;
    }
    case 'string':
      return null;
  }
}

/** The value a scope shows: global or session value where the server reports both. */
export function scopedValue(v: ServerVariable, scope: 'global' | 'session'): string | null {
  if (scope === 'global') return v.globalValue ?? v.value;
  return v.sessionValue ?? v.value;
}

// Redis clients

export type ClientType = 'normal' | 'pubsub' | 'replica' | 'master' | 'monitor';

/** The client type from its `flags` letters. */
export function clientType(c: ClientInfo): ClientType {
  const f = c.flags;
  if (f.includes('S')) return 'replica';
  if (f.includes('M')) return 'master';
  if (f.includes('O')) return 'monitor';
  if (f.includes('P') || (c.sub ?? 0) > 0 || (c.psub ?? 0) > 0 || (c.ssub ?? 0) > 0) return 'pubsub';
  return 'normal';
}

export interface ClientFilter {
  text: string;
  /** `''` for all types. */
  type: ClientType | '';
  /** Only clients idle at least this many seconds; `0` for all. */
  idleOverSeconds: number;
}

export function filterClients(clients: readonly ClientInfo[], filter: ClientFilter): ClientInfo[] {
  const needle = filter.text.trim().toLowerCase();
  return clients.filter((c) => {
    if (filter.type && clientType(c) !== filter.type) return false;
    if (filter.idleOverSeconds > 0 && (c.idle ?? 0) < filter.idleOverSeconds) return false;
    if (!needle) return true;
    return [c.addr, c.name, c.user, c.libName, String(c.id)].some((s) => (s ?? '').toLowerCase().includes(needle));
  });
}

// Redis memory analysis

/** The analysis form as typed: numbers may be empty or out of range. */
export interface MemoryForm {
  pattern: string;
  delimiter: string;
  depth: number | string;
  maxKeys: number | string;
  timeBudgetSeconds: number | string;
  samples: number | string;
}

/** Limits the form enforces; the backend clamps as well. */
export const MEMORY_LIMITS = {
  depth: [1, 16],
  maxKeys: [1, 10_000_000],
  timeBudgetSeconds: [1, 600],
  samples: [0, 1000],
} as const;

/**
 * The scan options for a form, or the problems with it. Fields the form does
 * not show (scan count, top keys, prefixes) keep the backend defaults.
 */
export function memoryOptionsFrom(
  form: MemoryForm,
  defaults: MemoryScanOptions,
): { options: MemoryScanOptions; problems: [] } | { options: null; problems: string[] } {
  const problems: string[] = [];
  const whole = (value: number | string, label: string, [min, max]: readonly [number, number]): number => {
    const n = typeof value === 'number' ? value : Number(value.trim());
    if (typeof value === 'string' && value.trim() === '') problems.push(`${label}: enter a number.`);
    else if (!Number.isInteger(n)) problems.push(`${label}: enter a whole number.`);
    else if (n < min || n > max) problems.push(`${label}: between ${min.toLocaleString('en-US')} and ${max.toLocaleString('en-US')}.`);
    return n;
  };
  const depth = whole(form.depth, 'Prefix depth', MEMORY_LIMITS.depth);
  const maxKeys = whole(form.maxKeys, 'Most keys', MEMORY_LIMITS.maxKeys);
  const seconds = whole(form.timeBudgetSeconds, 'Time budget', MEMORY_LIMITS.timeBudgetSeconds);
  const samples = whole(form.samples, 'Samples', MEMORY_LIMITS.samples);
  if (form.delimiter === '') problems.push('Delimiter: enter at least one character.');
  if (problems.length > 0) return { options: null, problems };
  const pattern = form.pattern.trim();
  return {
    options: {
      ...defaults,
      pattern: pattern === '' || pattern === '*' ? null : pattern,
      delimiter: form.delimiter,
      depth,
      maxKeys,
      timeBudgetMs: seconds * 1000,
      samples,
    },
    problems: [],
  };
}

/** A big key with its type, for the "Big keys" table. */
export interface TypedBigKey extends BigKey {
  keyType: string;
}

/** The biggest keys of the analysis, of one type or all, biggest first. */
export function bigKeys(analysis: MemoryAnalysis, keyType: string): TypedBigKey[] {
  return analysis.types
    .filter((t) => keyType === '' || t.keyType === keyType)
    .flatMap((t) => t.biggest.map((k) => ({ ...k, keyType: t.keyType })))
    .sort((a, b) => b.bytes - a.bytes);
}

/** Bytes of a prefix for the whole database where known, else the measured bytes. */
export function prefixBytes(p: Pick<PrefixUsage, 'bytes' | 'estimatedBytes'>): { bytes: number; estimated: boolean } {
  if (p.estimatedBytes !== null && p.estimatedBytes !== p.bytes) return { bytes: p.estimatedBytes, estimated: true };
  return { bytes: p.bytes, estimated: false };
}

// Redis config

/** Parameters whose name, or (non-secret) value, contains the text; `riskyOnly` keeps risky, protected and startup-only ones. */
export function filterConfig(entries: readonly ConfigEntry[], text: string, riskyOnly: boolean): ConfigEntry[] {
  const needle = text.trim().toLowerCase();
  return entries.filter((e) => {
    if (riskyOnly && !(e.dangerous || e.protected || e.immutable)) return false;
    if (!needle) return true;
    return e.name.includes(needle) || (!e.sensitive && (e.value ?? '').toLowerCase().includes(needle));
  });
}

/** The badges of a parameter, with what each means. */
export function configNote(e: ConfigEntry): { label: string; tone: string; help: string }[] {
  const out: { label: string; tone: string; help: string }[] = [];
  if (e.sensitive) out.push({ label: 'secret', tone: 'tone-neutral', help: 'The value is a secret and is never shown.' });
  if (e.immutable) out.push({ label: 'startup only', tone: 'tone-neutral', help: 'Can only be set in the configuration file or at startup.' });
  if (e.protected) out.push({ label: 'protected', tone: 'tone-warning', help: 'Changes need the server to run with enable-protected-configs.' });
  if (e.dangerous) out.push({ label: 'risky', tone: 'tone-danger', help: 'A wrong value can lose data or lock clients out.' });
  return out;
}

// Redis functions

/** The library name from the `#!lua name=<library>` first line, or `null` when the line is missing. */
export function libraryNameOf(code: string): string | null {
  const first = code.replace(/^\uFEFF/, '').split(/\r?\n/, 1)[0] ?? '';
  const match = /^#!(\w+)\s+(?:.*\s)?name=([^\s]+)/.exec(first.trim());
  return match ? (match[2] ?? null) : null;
}

// Redis cluster

export const CLUSTER_SLOTS = 16384;

/** Slot ranges as fractions of the slot space, for the slot map bar; gaps are unassigned. */
export function slotSegments(
  map: readonly { range: SlotRange; nodeId: string }[],
  unassigned: readonly SlotRange[],
): { start: number; width: number; nodeId: string | null; range: SlotRange }[] {
  const all = [
    ...map.map((m) => ({ range: m.range, nodeId: m.nodeId as string | null })),
    ...unassigned.map((range) => ({ range, nodeId: null as string | null })),
  ].sort((a, b) => a.range.start - b.range.start);
  return all.map((s) => ({
    start: s.range.start / CLUSTER_SLOTS,
    width: (s.range.end - s.range.start + 1) / CLUSTER_SLOTS,
    nodeId: s.nodeId,
    range: s.range,
  }));
}

/** Total slots of some ranges. */
export function slotCount(ranges: readonly SlotRange[]): number {
  return ranges.reduce((s, r) => s + (r.end - r.start + 1), 0);
}

/** `0-5460, 10923` style text of slot ranges. */
export function slotText(ranges: readonly SlotRange[]): string {
  return ranges.map((r) => (r.start === r.end ? String(r.start) : `${r.start}-${r.end}`)).join(', ');
}
