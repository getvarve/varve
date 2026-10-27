/**
 * The row objects the table data view hands to AG Grid.
 *
 * AG Grid finds rows by `getRowId` and redraws a row when its data object
 * is a different object. A row object is therefore reused while nothing
 * about the row changed: the same source row, the same row number, and no
 * change to its pending edits or delete mark. Pending changes are tracked
 * with a version per row id that the view bumps whenever it changes that
 * row, so deciding whether a row changed is O(1) and never serializes the
 * pending values.
 */
import type { CellValue } from '../api/grid';
import type { Value } from '../api/sessions';
import type { ChangeSet, GridRowData } from './gridChanges';

/** One grid row: a row as read, or a new row. */
export interface ViewRow {
  id: string;
  kind: 'row' | 'insert';
  /** Row number on this page, or the insert number. */
  n: number;
  values: (Value | null)[];
  /** Pending cells by column name. */
  pending: Record<string, CellValue>;
  deleted: boolean;
}

/** Versions of the pending changes per row id. */
export class RowVersions {
  /** Bumped for changes that touch every row (discard all, apply). */
  private epoch = 0;
  private readonly versions = new Map<string, number>();

  /** Marks the pending changes of these rows as changed. */
  bump(ids: Iterable<string>): void {
    for (const id of ids) this.versions.set(id, (this.versions.get(id) ?? 0) + 1);
  }

  /** Marks the pending changes of every row as changed. */
  bumpAll(): void {
    this.epoch += 1;
    this.versions.clear();
  }

  /** The current version of a row's pending changes. */
  of(id: string): string {
    return `${this.epoch}.${this.versions.get(id) ?? 0}`;
  }
}

interface CacheEntry {
  source: GridRowData | null;
  n: number;
  version: string;
  view: ViewRow;
}

/** What the grid shows: the page's rows and the pending new rows. */
export interface RowViewInput {
  rows: readonly GridRowData[];
  /** Offset of the page: the first row is number `offset + 1`. */
  offset: number;
  changes: ChangeSet;
  versions: RowVersions;
  /** Number of columns, for the empty values of new rows. */
  columnCount: number;
  /** Wraps each new row object (the view passes `markRaw`). */
  wrap?: <T extends object>(value: T) => T;
}

/**
 * Returns a function that builds the grid rows and reuses the row objects
 * of the previous call for rows that did not change.
 */
export function createRowViewCache(): (input: RowViewInput) => ViewRow[] {
  let cache = new Map<string, CacheEntry>();
  return (input) => {
    const wrap = input.wrap ?? (<T extends object>(v: T): T => v);
    const next = new Map<string, CacheEntry>();
    const viewOf = (id: string, kind: ViewRow['kind'], n: number, source: GridRowData | null, values: (Value | null)[], pending: Record<string, CellValue>, deleted: boolean): ViewRow => {
      const version = `${input.versions.of(id)}${deleted ? 'd' : ''}`;
      const hit = cache.get(id);
      const view = hit && hit.source === source && hit.n === n && hit.version === version ? hit.view : wrap({ id, kind, n, values, pending: { ...pending }, deleted });
      next.set(id, { source, n, version, view });
      return view;
    };
    const cs = input.changes;
    const out = input.rows.map((r, i) => viewOf(r.id, 'row', input.offset + i + 1, r, r.values, cs.updates[r.id]?.set ?? {}, r.id in cs.deletes));
    const empty: null[] = Array.from({ length: input.columnCount }, () => null);
    cs.inserts.forEach((ins, i) => {
      out.push(viewOf(ins.id, 'insert', i + 1, null, empty, ins.cells, false));
    });
    cache = next;
    return out;
  };
}
