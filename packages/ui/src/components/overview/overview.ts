import { displayName, kindLabel, objectIdentity } from '../../api/catalog';
import type { KindCount, ObjectKind, ObjectSummary } from '../../api/catalog';
import type { ObjNode } from '../../lib/tree';

/** The order kinds are listed in: tables first, then views and materialized views, then the rest. */
const KIND_ORDER: readonly ObjectKind[] = [
  'table',
  'partitionedTable',
  'foreignTable',
  'view',
  'materializedView',
  'sequence',
  'function',
  'procedure',
  'aggregate',
  'trigger',
  'event',
  'enumType',
  'compositeType',
  'domain',
  'rangeType',
  'collation',
  'operator',
  'package',
  'packageBody',
  'extension',
  'eventTrigger',
  'publication',
  'subscription',
  'foreignDataWrapper',
  'foreignServer',
  'cast',
];

/** Sort rank of a kind; kinds not listed come last, in a fixed order. */
export function kindRank(kind: ObjectKind): number {
  const at = KIND_ORDER.indexOf(kind);
  return at >= 0 ? at : KIND_ORDER.length;
}

/** Kinds the overview never lists itself (they are the containers it shows). */
const CONTAINER_KINDS: readonly ObjectKind[] = ['database', 'schema'];

/** The kinds of a container worth one list call each, in display order. */
export function overviewKinds(kinds: readonly KindCount[]): ObjectKind[] {
  return kinds
    .filter((k) => k.count > 0 && !CONTAINER_KINDS.includes(k.kind))
    .map((k) => k.kind)
    .sort((a, b) => kindRank(a) - kindRank(b) || a.localeCompare(b));
}

/** The schema a PostgreSQL overview starts on: `public`, else the first user schema, else the first one. */
export function defaultSchema(schemas: readonly ObjectSummary[]): string | null {
  const user = schemas.filter((s) => !s.system);
  return (user.find((s) => s.object.name === 'public') ?? user[0] ?? schemas[0])?.object.name ?? null;
}

/** The database a MySQL/MariaDB overview starts on: the session's default schema, else the first user database. */
export function defaultDatabase(databases: readonly ObjectSummary[], sessionDatabase: string | null): string | null {
  if (sessionDatabase !== null && databases.some((d) => d.object.name === sessionDatabase)) return sessionDatabase;
  const user = databases.filter((d) => !d.system);
  return (user[0] ?? databases[0])?.object.name ?? null;
}

export interface OverviewRow {
  key: string;
  summary: ObjectSummary;
  name: string;
  kind: ObjectKind;
  rows: number | null;
  size: number | null;
  comment: string | null;
}

export function overviewRows(summaries: readonly ObjectSummary[]): OverviewRow[] {
  return summaries.map((summary) => ({
    key: objectIdentity(summary.object),
    summary,
    name: displayName(summary.object, false),
    kind: summary.object.kind,
    rows: summary.rowEstimate,
    size: summary.sizeBytes,
    comment: summary.comment,
  }));
}

export type SortColumn = 'name' | 'kind' | 'rows' | 'size' | 'comment';
export interface SortState {
  column: SortColumn;
  descending: boolean;
}

/** The default order: by kind (tables first), then by name. */
export const DEFAULT_SORT: SortState = { column: 'kind', descending: false };

function compareNullable(a: number | string | null, b: number | string | null): number | null {
  if (a === null && b === null) return 0;
  // Unknown values sort last in both directions.
  if (a === null) return null;
  if (b === null) return null;
  return typeof a === 'number' && typeof b === 'number' ? a - b : String(a).localeCompare(String(b));
}

const byName = (a: OverviewRow, b: OverviewRow): number => a.name.localeCompare(b.name) || a.key.localeCompare(b.key);

/**
 * Sorts rows by a column, with ties broken by kind and name. Empty values
 * (unknown row counts, no comment) stay at the end in both directions.
 */
export function sortRows(rows: readonly OverviewRow[], sort: SortState): OverviewRow[] {
  const sign = sort.descending ? -1 : 1;
  const value = (row: OverviewRow): number | string | null => {
    switch (sort.column) {
      case 'name':
        return row.name;
      case 'kind':
        return kindRank(row.kind);
      case 'rows':
        return row.rows;
      case 'size':
        return row.size;
      case 'comment':
        return row.comment && row.comment !== '' ? row.comment : null;
    }
  };
  return [...rows].sort((a, b) => {
    const va = value(a);
    const vb = value(b);
    const compared = compareNullable(va, vb);
    if (compared === null) return va === null ? 1 : -1;
    if (compared !== 0) return sign * compared;
    return kindRank(a.kind) - kindRank(b.kind) || byName(a, b);
  });
}

export interface OverviewFilter {
  text: string;
  /** Kinds to show; empty shows every kind. */
  kinds: ReadonlySet<ObjectKind>;
}

/** Rows whose name or comment contains the text (case-insensitive), of the chosen kinds. */
export function filterRows(rows: readonly OverviewRow[], filter: OverviewFilter): OverviewRow[] {
  const needle = filter.text.trim().toLocaleLowerCase();
  return rows.filter(
    (row) =>
      (filter.kinds.size === 0 || filter.kinds.has(row.kind)) &&
      (needle === '' || row.name.toLocaleLowerCase().includes(needle) || (row.comment ?? '').toLocaleLowerCase().includes(needle)),
  );
}

/** One filter chip per kind present, in display order, with counts. */
export function kindChips(rows: readonly OverviewRow[]): { kind: ObjectKind; label: string; count: number }[] {
  const counts = new Map<ObjectKind, number>();
  for (const row of rows) counts.set(row.kind, (counts.get(row.kind) ?? 0) + 1);
  return [...counts.entries()]
    .sort(([a], [b]) => kindRank(a) - kindRank(b) || a.localeCompare(b))
    .map(([kind, count]) => ({ kind, label: kindLabel(kind, true), count }));
}

/** An estimated row count for display: grouped digits, `null` as an empty cell. */
export function formatRowCount(rows: number | null): string {
  return rows === null ? '' : rows.toLocaleString();
}

const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];

/** A byte count in binary units with one decimal from KiB up (`16.0 KiB`). */
export function formatBytes(bytes: number | null): string {
  if (bytes === null) return '';
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${value} B` : `${value.toFixed(1)} ${UNITS[unit] ?? ''}`;
}

/** The tree node of a row, so the overview shares the tree's menus and actions. */
export function rowNode(connectionId: string, row: OverviewRow): Extract<ObjNode, { type: 'object' }> {
  return { type: 'object', key: `overview:${connectionId}:${row.key}`, connectionId, name: row.name, summary: row.summary };
}

/**
 * The kind filter that applies: the chosen kinds that still have rows. A
 * filter whose kinds all left the list (another schema, a refresh) applies
 * as no filter, so it can never hide everything without a chip to undo it.
 */
export function effectiveKinds(chosen: ReadonlySet<ObjectKind>, rows: readonly OverviewRow[]): Set<ObjectKind> {
  const present = new Set(rows.map((r) => r.kind));
  return new Set([...chosen].filter((kind) => present.has(kind)));
}

/** The first row whose top is in view, for keyboard navigation that starts where the user scrolled to. */
export function firstRowInView(scrollTop: number, rowHeight: number, count: number): number {
  if (count === 0) return -1;
  return Math.min(count - 1, Math.max(0, Math.ceil(scrollTop / rowHeight)));
}
