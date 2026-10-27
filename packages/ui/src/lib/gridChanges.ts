/**
 * Change tracking for the table data view.
 *
 * Pending edits are kept by row key (the values of the key columns as the
 * row was read), not by position, so they survive paging, sorting and a
 * reload: a reload refreshes the row's snapshot of original values, which
 * is what "re-read the rows and plan again" after a conflict means. Each
 * entry also keeps its own snapshot, so edits on several pages are applied
 * together. Everything here is plain data and pure functions.
 */
import type { AppliedChange, ApplyOutcome, CellValue, ColumnInfo, KeyInfo, RowChange, RowKey } from '../api/grid';
import { openTransactionOf } from '../api/errors';
import type { CmdError } from '../api/errors';
import type { Value } from '../api/sessions';
import { cellRestoresOriginal, columnAccess } from './gridValues';
import { plainText } from './values';
import type { GridDialect } from './gridSql';

/** A row as read, values in `columns` order. */
export interface GridRowData {
  /** `keyId` of the row's key. */
  id: string;
  values: Value[];
}

/** Pending changes to an existing row. */
export interface RowUpdate {
  key: RowKey;
  /** The row as last read, in `columns` order. */
  original: Value[];
  /** Changed cells by column name. */
  set: Record<string, CellValue>;
}

/** A pending delete. */
export interface RowDelete {
  key: RowKey;
  original: Value[];
}

/** A pending new row. */
export interface RowInsert {
  id: string;
  /** Cells the user filled in by column name; the others get their default. */
  cells: Record<string, CellValue>;
}

/** Every pending change of a data view. */
export interface ChangeSet {
  updates: Record<string, RowUpdate>;
  deletes: Record<string, RowDelete>;
  inserts: RowInsert[];
  /** Counter for insert ids. */
  nextInsert: number;
}

/** Which pending entry a planned change came from. */
export type ChangeRef = { kind: 'update' | 'delete'; id: string } | { kind: 'insert'; id: string };

/** A copy of `record` without `key`. */
function without<T>(record: Record<string, T>, key: string): Record<string, T> {
  return Object.fromEntries(Object.entries(record).filter(([k]) => k !== key));
}

/** An empty change set. */
export function emptyChanges(): ChangeSet {
  return { updates: {}, deletes: {}, inserts: [], nextInsert: 1 };
}

/** Counts for the pending changes bar. */
export function changeCounts(cs: ChangeSet): { updated: number; inserted: number; deleted: number; total: number } {
  const updated = Object.keys(cs.updates).filter((id) => !(id in cs.deletes)).length;
  const inserted = cs.inserts.length;
  const deleted = Object.keys(cs.deletes).length;
  return { updated, inserted, deleted, total: updated + inserted + deleted };
}

function columnIndex(columns: readonly ColumnInfo[], name: string): number {
  const at = columns.findIndex((c) => c.name === name);
  if (at < 0) throw new Error(`The table has no column ${name}.`);
  return at;
}

/**
 * The key of a row, in key order. Throws when a key column is NULL (a
 * unique key over NOT NULL columns never is, so the row was not read as
 * the table now is).
 */
export function rowKeyOf(key: KeyInfo, columns: readonly ColumnInfo[], values: readonly Value[]): RowKey {
  return {
    columns: key.columns.map((name): [string, Value] => {
      const value = values[columnIndex(columns, name)];
      if (!value || value.t === 'null') throw new Error(`The key column ${name} is NULL in this row; reload the rows.`);
      return [name, value];
    }),
  };
}

/** A stable id for a row key, used to find the row again after a reload. */
export function keyId(rowKey: RowKey): string {
  return JSON.stringify(rowKey.columns);
}

/** Rows as read, with their ids. */
export function toGridRows(key: KeyInfo, columns: readonly ColumnInfo[], rows: readonly Value[][]): GridRowData[] {
  return rows.map((values) => ({ id: keyId(rowKeyOf(key, columns, values)), values }));
}

/**
 * Sets a cell of an existing row. A value equal to what the row holds
 * removes the pending edit of that cell. Rows marked for deletion cannot
 * be edited (undo the delete first); columns the server computes refuse.
 */
export function setCell(
  cs: ChangeSet,
  dialect: GridDialect,
  key: KeyInfo,
  columns: readonly ColumnInfo[],
  row: GridRowData,
  column: string,
  cell: CellValue,
): void {
  if (row.id in cs.deletes) throw new Error('This row is marked for deletion. Undo the delete to edit it.');
  const at = columnIndex(columns, column);
  const info = columns[at] as ColumnInfo;
  const access = columnAccess(info);
  if (!access.update && cell.kind !== 'default') throw new Error(access.reason ?? `${column} cannot be written.`);
  if (cell.kind === 'value' && cell.value.t === 'null' && !info.nullable) {
    throw new Error(`${column} is NOT NULL.`);
  }
  const original = row.values[at] as Value;
  const entry = cs.updates[row.id];
  if (cellRestoresOriginal(dialect, cell, original)) {
    if (entry) {
      entry.set = without(entry.set, column);
      if (Object.keys(entry.set).length === 0) cs.updates = without(cs.updates, row.id);
    }
    return;
  }
  if (entry) entry.set[column] = cell;
  else cs.updates[row.id] = { key: rowKeyOf(key, columns, row.values), original: [...row.values], set: { [column]: cell } };
}

/** Undoes the pending edit of one cell. */
export function revertCell(cs: ChangeSet, rowId: string, column: string): void {
  const entry = cs.updates[rowId];
  if (!entry) return;
  entry.set = without(entry.set, column);
  if (Object.keys(entry.set).length === 0) cs.updates = without(cs.updates, rowId);
}

/** Marks a row for deletion, or undoes that. Returns true when the row is now marked. */
export function toggleDelete(cs: ChangeSet, key: KeyInfo, columns: readonly ColumnInfo[], row: GridRowData): boolean {
  if (row.id in cs.deletes) {
    cs.deletes = without(cs.deletes, row.id);
    return false;
  }
  cs.deletes[row.id] = { key: rowKeyOf(key, columns, row.values), original: [...row.values] };
  return true;
}

/** Adds an empty new row (every column gets its default) and returns its id. */
export function addInsert(cs: ChangeSet): string {
  const id = `new:${cs.nextInsert}`;
  cs.nextInsert += 1;
  cs.inserts.push({ id, cells: {} });
  return id;
}

/** Sets a cell of a new row; `null` goes back to the column default. */
export function setInsertCell(cs: ChangeSet, columns: readonly ColumnInfo[], id: string, column: string, cell: CellValue | null): void {
  const row = cs.inserts.find((r) => r.id === id);
  if (!row) throw new Error('That new row is no longer pending.');
  const info = columns[columnIndex(columns, column)] as ColumnInfo;
  if (cell === null || cell.kind === 'default') {
    row.cells = without(row.cells, column);
    return;
  }
  const access = columnAccess(info);
  if (!access.insert) throw new Error(access.reason ?? `${column} cannot be written.`);
  if (cell.kind === 'value' && cell.value.t === 'null' && !info.nullable) throw new Error(`${column} is NOT NULL.`);
  row.cells[column] = cell;
}

/** Removes a pending new row. */
export function removeInsert(cs: ChangeSet, id: string): void {
  cs.inserts = cs.inserts.filter((r) => r.id !== id);
}

/** Drops every pending change. */
export function discardAll(cs: ChangeSet): void {
  cs.updates = {};
  cs.deletes = {};
  cs.inserts = [];
}

/** Drops the pending change a planned change came from. */
export function discardRef(cs: ChangeSet, ref: ChangeRef): void {
  if (ref.kind === 'update') cs.updates = without(cs.updates, ref.id);
  else if (ref.kind === 'delete') cs.deletes = without(cs.deletes, ref.id);
  else removeInsert(cs, ref.id);
}

/** A NOT NULL column without default that a new row leaves empty, or `null`. */
export function missingRequired(columns: readonly ColumnInfo[], row: RowInsert): string | null {
  const miss = columns.find((c) => !c.nullable && !c.hasDefault && columnAccess(c).insert && !(c.name in row.cells));
  return miss ? miss.name : null;
}

/**
 * The `RowChange` list for `row_edit_plan`, with which entry each came
 * from. Deletes go first, then updates, then inserts, so a row can be
 * replaced by a new one with the same key in one apply. Updates send the
 * original values of the changed columns; deletes every column the server
 * can compare (with `checkOriginal` a row someone else changed is then not
 * deleted).
 */
export function buildRowChanges(cs: ChangeSet, columns: readonly ColumnInfo[]): { changes: RowChange[]; refs: ChangeRef[] } {
  const changes: RowChange[] = [];
  const refs: ChangeRef[] = [];
  const comparable = columns.map((c, i) => ({ c, i })).filter(({ c }) => !c.arrayOfArrays);
  for (const [id, entry] of Object.entries(cs.deletes)) {
    changes.push({
      op: 'delete',
      key: entry.key,
      original: comparable.map(({ c, i }): [string, Value] => [c.name, entry.original[i] as Value]),
    });
    refs.push({ kind: 'delete', id });
  }
  for (const [id, entry] of Object.entries(cs.updates)) {
    if (id in cs.deletes) continue;
    const names = Object.keys(entry.set);
    if (names.length === 0) continue;
    changes.push({
      op: 'update',
      key: entry.key,
      original: names.map((name): [string, Value] => [name, entry.original[columnIndex(columns, name)] as Value]),
      set: names.map((name): [string, CellValue] => [name, entry.set[name] as CellValue]),
    });
    refs.push({ kind: 'update', id });
  }
  for (const row of cs.inserts) {
    changes.push({ op: 'insert', values: Object.entries(row.cells) });
    refs.push({ kind: 'insert', id: row.id });
  }
  return { changes, refs };
}

/**
 * After a reload: rows on the new page refresh the snapshot of their
 * pending edits and deletes (so the next plan compares against what the
 * server holds now), and edits that now match the stored value are
 * dropped. Returns the ids that were refreshed.
 */
export function refreshSnapshots(cs: ChangeSet, dialect: GridDialect, columns: readonly ColumnInfo[], rows: readonly GridRowData[]): string[] {
  const refreshed: string[] = [];
  for (const row of rows) {
    const update = cs.updates[row.id];
    if (update) {
      update.original = [...row.values];
      for (const [name, cell] of Object.entries(update.set)) {
        if (cellRestoresOriginal(dialect, cell, row.values[columnIndex(columns, name)] as Value)) update.set = without(update.set, name);
      }
      if (Object.keys(update.set).length === 0) cs.updates = without(cs.updates, row.id);
      refreshed.push(row.id);
    }
    const del = cs.deletes[row.id];
    if (del) {
      del.original = [...row.values];
      if (!refreshed.includes(row.id)) refreshed.push(row.id);
    }
  }
  return refreshed;
}

function rowFrom(columns: readonly ColumnInfo[], written: [string, Value][], fallback: readonly Value[] | null): Value[] {
  return columns.map((c, i) => {
    const hit = written.find(([name]) => name === c.name) ?? written.find(([name]) => name.toLowerCase() === c.name.toLowerCase());
    return hit ? hit[1] : (fallback?.[i] ?? { t: 'null' });
  });
}

/** The grid after a successful apply. */
export interface MergeResult {
  rows: GridRowData[];
  /** Ids of rows that were written, for a short highlight. */
  written: string[];
  /** True when a written row could not be read back: reload to show what the server stored. */
  needsReload: boolean;
}

/**
 * Merges an applied outcome into the page: deleted rows leave, updated
 * rows take the values the server stored, new rows are appended. Clears
 * the change set.
 */
export function mergeApplied(
  cs: ChangeSet,
  key: KeyInfo,
  columns: readonly ColumnInfo[],
  rows: readonly GridRowData[],
  refs: readonly ChangeRef[],
  applied: readonly AppliedChange[],
): MergeResult {
  let out = [...rows];
  const written: string[] = [];
  let needsReload = false;
  for (const change of applied) {
    const ref = refs[change.change];
    if (!ref) continue;
    if (change.op === 'delete') {
      out = out.filter((r) => r.id !== ref.id);
      continue;
    }
    if (!change.row) {
      needsReload = true;
      continue;
    }
    if (change.op === 'update') {
      const at = out.findIndex((r) => r.id === ref.id);
      const values = rowFrom(columns, change.row, at >= 0 ? (out[at] as GridRowData).values : null);
      const id = keyId(rowKeyOf(key, columns, values));
      if (at >= 0) out[at] = { id, values };
      written.push(id);
    } else {
      const values = rowFrom(columns, change.row, null);
      const id = keyId(rowKeyOf(key, columns, values));
      out.push({ id, values });
      written.push(id);
    }
  }
  discardAll(cs);
  return { rows: out, written, needsReload };
}

/** What the view shows for an apply that did not change anything. */
export interface ApplyProblem {
  ref: ChangeRef | null;
  title: string;
  message: string;
  /** A conflict (someone else changed the data), as opposed to an error from the server. */
  conflict: boolean;
}

/** Explains a conflict or failure in words, with the entry it belongs to. */
export function describeProblem(outcome: Exclude<ApplyOutcome, { status: 'applied' }>, refs: readonly ChangeRef[]): ApplyProblem {
  const ref = refs[outcome.change] ?? null;
  const which = ref ? (ref.kind === 'insert' ? 'a new row' : ref.kind === 'delete' ? 'a deleted row' : 'an edited row') : 'a change';
  if (outcome.status === 'conflict') {
    if (outcome.reason === 'keyNotUnique') {
      return {
        ref,
        conflict: true,
        title: 'The key matched more than one row',
        message: `${outcome.message} Nothing was applied. The key of ${which} does not identify a single row.`,
      };
    }
    return {
      ref,
      conflict: true,
      title: 'Someone else changed or deleted this row',
      message: `${outcome.message} Nothing was applied. Reload the rows to see what the table holds now, then review your changes and apply again.`,
    };
  }
  const e = outcome.error;
  const extra = [e.detail, e.hint].filter(Boolean).join(' ');
  return {
    ref,
    conflict: false,
    title: `The server rejected ${which}`,
    message: `${e.message}${e.code ? ` (${e.code})` : ''}${extra ? ` ${extra}` : ''} Nothing was applied.`,
  };
}

/** A row id from `keyId` in words: `id = 7`, `a = 1, b = x`. */
export function keyLabel(id: string): string {
  try {
    const columns = JSON.parse(id) as [string, Value][];
    return columns.map(([name, value]) => `${name} = ${plainText(value) ?? 'NULL'}`).join(', ');
  } catch {
    return id;
  }
}

/** One label per planned change, for the review ("Row id = 7", "New row 2"). */
export function changeLabels(refs: readonly ChangeRef[]): string[] {
  let inserts = 0;
  return refs.map((ref) => {
    if (ref.kind === 'insert') {
      inserts += 1;
      return `New row ${inserts}`;
    }
    return `Row ${keyLabel(ref.id)}`;
  });
}

/**
 * What the grid does after `row_edit_apply` failed with `error`:
 * - `keep`: refused before anything ran with the plan kept (an open
 *   transaction); apply the same plan again later.
 * - `replan`: the plan is gone but nothing ran (expired, or taken by an
 *   earlier attempt that never started): plan the pending changes again.
 * - `close`: the apply may have run; show the error and read the rows again.
 */
export function applyFailureAction(error: CmdError): 'keep' | 'replan' | 'close' {
  if (openTransactionOf(error) !== null) return 'keep';
  if (error.kind === 'notFound') return 'replan';
  return 'close';
}
