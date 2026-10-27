import type { BulkCount, BulkTtlStatus, KeyEdit, ModuleKind, ModuleQuery, TsLabel, TsMeta } from '../api/redis';
import type { Value } from '../api/sessions';
import { textValue } from './redisEdit';

/** Largest timestamp a time series takes (signed 64-bit milliseconds). */
const MAX_TIMESTAMP = 9223372036854775807n;

/**
 * A timestamp typed by the user as milliseconds (`1718000000000`) or as a
 * date and time (`2026-10-21T13:45:00Z`, `2026-10-21 13:45`, local time when
 * no zone is given). `null` when it is neither; empty input is `undefined`.
 */
export function parseTimestamp(input: string): string | null | undefined {
  const text = input.trim();
  if (text === '') return undefined;
  if (/^\d+$/.test(text)) {
    const n = BigInt(text);
    return n <= MAX_TIMESTAMP ? n.toString() : null;
  }
  const ms = Date.parse(text.includes('T') || !text.includes(' ') ? text : text.replace(' ', 'T'));
  return Number.isFinite(ms) && ms >= 0 ? String(ms) : null;
}

/** A timestamp in milliseconds as ISO text in UTC, or the number when it is out of the date range. */
export function formatTimestamp(ms: string | null): string {
  if (ms === null) return '';
  const n = Number(ms);
  if (!Number.isSafeInteger(n) || n > 8.64e15) return ms;
  return new Date(n).toISOString();
}

/** A retention in milliseconds in words (`0`: keeps samples forever). */
export function formatRetention(ms: string): string {
  if (!/^\d+$/.test(ms) || ms === '0') return 'forever';
  const n = BigInt(ms);
  const units: [bigint, string][] = [
    [86_400_000n, 'd'],
    [3_600_000n, 'h'],
    [60_000n, 'min'],
    [1000n, 's'],
  ];
  for (const [size, unit] of units) {
    if (n % size === 0n) return `${(n / size).toString()} ${unit}`;
  }
  return `${ms} ms`;
}

/** Label rows as the user edits them: names and values are kept exactly (no trimming). */
export function labelProblems(labels: readonly TsLabel[]): string[] {
  const problems: string[] = [];
  const seen = new Set<string>();
  labels.forEach((l, i) => {
    if (l.name === '' || l.value === '') {
      problems.push(`Label ${i + 1}: give a name and a value.`);
      return;
    }
    if (seen.has(l.name)) problems.push(`The label ${l.name} is given twice.`);
    seen.add(l.name);
  });
  if (labels.length > MAX_TS_LABELS) problems.push(`At most ${MAX_TS_LABELS} labels can be set here; use TS.ALTER in the console for more.`);
  return problems;
}

/** Most labels one settings change sets (crates/driver-redis `MAX_TS_LABELS`). */
export const MAX_TS_LABELS = 1000;

function sameLabels(a: readonly TsLabel[], b: readonly TsLabel[]): boolean {
  return a.length === b.length && a.every((l, i) => l.name === b[i]?.name && l.value === b[i]?.value);
}

export const DUPLICATE_POLICIES = ['block', 'first', 'last', 'min', 'max', 'sum'] as const;

/** What the settings form holds. */
export interface TsSettingsForm {
  retentionMs: string;
  duplicatePolicy: string;
  labels: TsLabel[];
}

/** The form for a series' settings as read. */
export function settingsForm(meta: TsMeta): TsSettingsForm {
  return { retentionMs: meta.retentionMs, duplicatePolicy: meta.duplicatePolicy ?? '', labels: meta.labels.map((l) => ({ ...l })) };
}

/**
 * The `tsAlter` edit for what changed in the form (only changed parts are
 * sent), or the problems. `null` edit with no problems: nothing changed.
 */
export function tsAlterEdit(meta: TsMeta, form: TsSettingsForm): { edit: KeyEdit | null; problems: string[] } {
  const problems: string[] = [];
  const retention = form.retentionMs.trim();
  if (!/^\d+$/.test(retention) || BigInt(retention) > MAX_TIMESTAMP) problems.push('The retention is a whole number of milliseconds (0 keeps samples forever).');
  const policy = form.duplicatePolicy.trim().toLowerCase();
  if (policy !== '' && !(DUPLICATE_POLICIES as readonly string[]).includes(policy)) problems.push(`The duplicate policy is one of ${DUPLICATE_POLICIES.join(', ')}.`);
  problems.push(...labelProblems(form.labels));
  if (problems.length > 0) return { edit: null, problems };
  const retentionChanged = BigInt(retention).toString() !== meta.retentionMs;
  const policyChanged = policy !== '' && policy !== (meta.duplicatePolicy ?? '');
  const labelsChanged = !sameLabels(form.labels, meta.labels);
  if (!retentionChanged && !policyChanged && !labelsChanged) return { edit: null, problems };
  return {
    edit: {
      op: 'tsAlter',
      retentionMs: retentionChanged ? BigInt(retention).toString() : null,
      labels: labelsChanged ? form.labels.map((l) => ({ ...l })) : null,
      duplicatePolicy: policyChanged ? policy : null,
      original: meta,
    },
    problems,
  };
}

/** True when lowering the retention to `next` drops samples (from `current`, both in ms; 0 is forever). */
export function retentionShrinks(current: string, next: string): boolean {
  if (!/^\d+$/.test(next) || !/^\d+$/.test(current)) return false;
  const n = BigInt(next);
  return n > 0n && (current === '0' || n < BigInt(current));
}

/** One input per non-empty line, as text values. */
export function lines(text: string): Value[] {
  return text
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l !== '')
    .map(textValue);
}

/** A query a probabilistic key offers. */
export interface QueryChoice {
  op: ModuleQuery['op'];
  label: string;
  /** What one input line is. */
  input: string;
}

/** The read-only queries of a kind. */
export function queriesFor(kind: ModuleKind): QueryChoice[] {
  switch (kind) {
    case 'bloom':
      return [{ op: 'bloomExists', label: 'May contain (BF.MEXISTS)', input: 'item' }];
    case 'cuckoo':
      return [
        { op: 'cuckooExists', label: 'May contain (CF.MEXISTS)', input: 'item' },
        { op: 'cuckooCount', label: 'Count (CF.COUNT)', input: 'item' },
      ];
    case 'countMin':
      return [{ op: 'cmsQuery', label: 'Estimated count (CMS.QUERY)', input: 'item' }];
    case 'topK':
      return [{ op: 'topkQuery', label: 'In the top list (TOPK.QUERY)', input: 'item' }];
    case 'tDigest':
      return [
        { op: 'tdigestQuantile', label: 'Quantile (TDIGEST.QUANTILE)', input: 'fraction from 0 to 1' },
        { op: 'tdigestCdf', label: 'Fraction at or below (TDIGEST.CDF)', input: 'value' },
        { op: 'tdigestRank', label: 'Rank (TDIGEST.RANK)', input: 'value' },
        { op: 'tdigestRevRank', label: 'Reverse rank (TDIGEST.REVRANK)', input: 'value' },
        { op: 'tdigestByRank', label: 'Value at rank (TDIGEST.BYRANK)', input: 'rank' },
        { op: 'tdigestTrimmedMean', label: 'Trimmed mean (TDIGEST.TRIMMED_MEAN)', input: 'low and high fraction' },
      ];
    case 'timeSeries':
      return [];
  }
}

/** The query for `op` with one input per line; `null` when the input does not fit. */
export function buildQuery(op: ModuleQuery['op'], text: string): ModuleQuery | null {
  const inputs = lines(text);
  if (op === 'tdigestTrimmedMean') {
    const [low, high] = text.split(/[\s,]+/).filter((t) => t !== '');
    return low !== undefined && high !== undefined ? { op, low: textValue(low), high: textValue(high) } : null;
  }
  if (inputs.length === 0) return null;
  switch (op) {
    case 'bloomExists':
    case 'cuckooExists':
    case 'cuckooCount':
    case 'cmsQuery':
    case 'topkQuery':
      return { op, items: inputs };
    case 'tdigestQuantile':
      return { op, quantiles: inputs };
    case 'tdigestCdf':
    case 'tdigestRank':
    case 'tdigestRevRank':
      return { op, values: inputs };
    case 'tdigestByRank':
      return { op, ranks: inputs };
  }
}

/** `item increment` lines as Count-Min increments (increment 1 when left out). */
export function cmsIncrements(text: string): { items: { item: Value; increment: string }[]; problems: string[] } {
  const items: { item: Value; increment: string }[] = [];
  const problems: string[] = [];
  text.split('\n').forEach((raw, i) => {
    const line = raw.trim();
    if (line === '') return;
    const match = /^(.*?)(?:\s+(\d+))?$/.exec(line);
    const item = match?.[1]?.trim() ?? '';
    const increment = match?.[2] ?? '1';
    if (item === '' || increment === '0') {
      problems.push(`Line ${i + 1}: write an item, optionally followed by an increment of at least 1.`);
      return;
    }
    items.push({ item: textValue(item), increment });
  });
  return { items, problems };
}

/** The add edit a kind offers for one item per line; `null` when there is nothing to add. */
export function addEdit(kind: ModuleKind, text: string, nx = false): KeyEdit | null {
  const inputs = lines(text);
  if (kind === 'countMin') {
    const { items, problems } = cmsIncrements(text);
    return problems.length === 0 && items.length > 0 ? { op: 'cmsIncrBy', items } : null;
  }
  if (inputs.length === 0) return null;
  switch (kind) {
    case 'bloom':
      return { op: 'bloomAdd', items: inputs };
    case 'cuckoo':
      return { op: 'cuckooAdd', items: inputs, nx };
    case 'topK':
      return { op: 'topkAdd', items: inputs };
    case 'tDigest':
      return inputs.every((v) => v.t === 'text' && Number.isFinite(Number(v.v))) ? { op: 'tdigestAdd', values: inputs } : null;
    case 'timeSeries':
      return null;
  }
}

/** A bulk preview in words. */
export function previewText(count: BulkCount): string {
  const n = Number(count.keys).toLocaleString('en-US');
  const keys = count.keys === '1' ? 'key' : 'keys';
  return count.complete ? `${n} ${keys} match on database ${count.db}.` : `At least ${n} ${keys} match on database ${count.db} (counting stopped early).`;
}

/** A bulk job's progress in words. */
export function bulkProgressText(status: BulkTtlStatus): string {
  const c = status.counts;
  const changed = Number(c.changed).toLocaleString('en-US');
  const matched = Number(c.matched).toLocaleString('en-US');
  const skipped = c.skipped === '0' ? '' : `, ${Number(c.skipped).toLocaleString('en-US')} skipped (gone, or without a TTL)`;
  switch (status.state) {
    case 'running':
      return `${changed} of ${matched} keys changed so far${skipped}.`;
    case 'complete':
      return `Done: ${changed} keys changed${skipped}.`;
    case 'stopped':
      return `Stopped: ${changed} keys were changed before the stop and keep their new TTL${skipped}.`;
    case 'failed':
      return `Failed after ${changed} changed keys${status.error ? `: ${status.error.message}` : ''}.`;
  }
}

/** A TTL typed in seconds as milliseconds text; `null` when it is not a whole number of at least 1. */
export function ttlFromSeconds(input: string): string | null {
  const text = input.trim();
  if (!/^\d+$/.test(text)) return null;
  const ms = BigInt(text) * 1000n;
  return ms >= 1000n && ms <= MAX_TIMESTAMP ? ms.toString() : null;
}
