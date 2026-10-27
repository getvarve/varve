/**
 * What the designer checks before a save is reviewed, beyond what the plan
 * itself can know: how many rows an engine change loses, an AUTO_INCREMENT
 * the server will raise, and indexes a character set conversion makes too
 * long. The notes go with their step into the plan review and the
 * destructive confirmation.
 *
 * Rows an engine change loses are first given as the server's estimate
 * (`TABLE_ROWS`), so the review opens at once; the exact count follows
 * when it arrives ({@link PendingCount}, counted on the metadata session
 * under a server-side cap) and replaces the note.
 */
import type { DesignerDetails, EngineChange } from '../../api/grid';
import { convertIndexWarnings, engineRowLoss } from '../../lib/gridDesigner';
import type { TableDraft } from '../../lib/gridDesigner';
import { quoteIdent } from '../../lib/tree';

/** Runs a read-only query and returns the first value of its first row as text (`null` for NULL). */
export type ScalarQuery = (sql: string) => Promise<string | null>;

/** How many rows a note speaks of: counted exactly, the server's estimate, or not known. */
export type RowsKnown = { kind: 'exact'; n: number } | { kind: 'estimate'; n: number; countTimedOut: boolean } | { kind: 'unknown' };

/** A row-loss note whose exact count is still to come: `notes[step][note]`. */
export interface PendingCount {
  step: number;
  note: number;
  table: string;
  engine: string;
  loss: 'discarded' | 'restart';
  /** The estimate the note shows until the count arrives (or when it runs out of time). */
  estimate: RowsKnown;
}

/** The review notes per change (same order as the changes) and the row counts still to come. */
export interface ReviewNotes {
  notes: string[][];
  counts: PendingCount[];
}

function rowsText(rows: RowsKnown): string {
  const plural = (n: number): string => (n === 1 ? '1 row' : `${n} rows`);
  switch (rows.kind) {
    case 'exact':
      return plural(rows.n);
    case 'estimate':
      return `about ${plural(rows.n)} (the server's estimate${rows.countTimedOut ? '; an exact count took too long' : ''})`;
    case 'unknown':
      return 'its rows (not counted)';
  }
}

/** The note for an engine change that loses rows. */
export function rowLossNote(table: string, engine: string, loss: 'discarded' | 'restart', rows: RowsKnown): string {
  const e = engine.toUpperCase();
  return loss === 'discarded'
    ? `${table} holds ${rowsText(rows)}: ${e} keeps none of them. The rows will be lost.`
    : `${table} holds ${rowsText(rows)}: ${e} keeps them only in memory, so they will be lost at the next server restart.`;
}

/** Notes per change; row counts are estimates here and listed in `counts` for the exact count. */
export async function reviewNotes(draft: TableDraft, details: DesignerDetails | null, changes: readonly EngineChange[], query: ScalarQuery): Promise<ReviewNotes> {
  const notes: string[][] = [];
  const counts: PendingCount[] = [];
  const table = `${quoteIdent('mysql', draft.database)}.${quoteIdent('mysql', draft.table)}`;
  const estimate = details?.engine === 'mysql' ? details.object.rowEstimate : null;
  for (const [step, change] of changes.entries()) {
    const out: string[] = [];
    notes.push(out);
    if (change.engine !== 'mysql' || change.change.change !== 'alterTable' || change.change.name !== draft.table || draft.isNew) continue;
    for (const op of change.change.operations) {
      if (op.op === 'setOptions' && 'options' in op) {
        const engine = op.options.engine ?? null;
        const loss = engine ? engineRowLoss(engine) : null;
        if (engine && loss) {
          const rows: RowsKnown = estimate === null ? { kind: 'unknown' } : { kind: 'estimate', n: estimate, countTimedOut: false };
          counts.push({ step, note: out.length, table: draft.table, engine, loss, estimate: rows });
          out.push(rowLossNote(draft.table, engine, loss, rows));
        }
        const next = op.options.autoIncrement ?? null;
        const column = draft.columns.find((c) => c.autoIncrement && c.origName !== null && !c.dropped);
        if (next !== null && column?.origName) {
          // The AUTO_INCREMENT column leads an index, so MAX() reads one end of it.
          let max: string | null;
          try {
            max = await query(`SELECT MAX(${quoteIdent('mysql', column.origName)}) FROM ${table}`);
          } catch {
            max = null;
          }
          if (max !== null && /^\d+$/.test(max) && BigInt(next) <= BigInt(max)) {
            out.push(`AUTO_INCREMENT ${next} is not above the largest ${column.origName} (${max}): the server keeps it at ${(BigInt(max) + 1n).toString()} instead.`);
          }
        }
      }
      if (op.op === 'convertCharset' && 'charset' in op && details?.engine === 'mysql') {
        out.push(...convertIndexWarnings(details.object, op.charset));
      }
    }
  }
  return { notes, counts };
}

/**
 * The note once the exact count is known: the count, or the estimate marked
 * as such when the server stopped the count at its cap (`null`).
 */
export function countedNote(pending: PendingCount, counted: number | null): string {
  if (counted !== null) return rowLossNote(pending.table, pending.engine, pending.loss, { kind: 'exact', n: counted });
  const rows: RowsKnown = pending.estimate.kind === 'estimate' ? { ...pending.estimate, countTimedOut: true } : pending.estimate;
  return rowLossNote(pending.table, pending.engine, pending.loss, rows);
}
