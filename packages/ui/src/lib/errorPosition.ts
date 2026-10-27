import type { DriverError } from '../api/errors';
import { charPositionToOffset } from './values';

/** Where a driver error points in the editor, computed once per run. */
export interface ErrorMark {
  /** Identity of the run: the editor marks (and moves the cursor) once per key. */
  key: number;
  from: number;
  to: number;
  line: number;
  column: number;
  /** The editor text the positions refer to; the mark applies only while the editor still holds exactly this. */
  doc: string;
}

/**
 * The editor range of an error position. `position` is a 1-based character
 * offset in the submitted `text`, which started at UTF-16 `offset` of `doc`
 * (the whole editor text when the run started). The mark covers the token
 * at the position, at least one character. Depends only on the run, never
 * on later typing.
 */
export function errorMark(
  key: number,
  error: Pick<DriverError, 'position'>,
  doc: string,
  text: string,
  offset: number,
): ErrorMark | null {
  if (error.position === null || error.position < 1) return null;
  const from = Math.min(offset + charPositionToOffset(text, error.position), doc.length);
  let to = from;
  while (to < doc.length && /[\w$."]/.test(doc.charAt(to))) to += 1;
  to = Math.max(to, Math.min(from + 1, doc.length));
  const before = doc.slice(0, from);
  return { key, from, to, line: before.split('\n').length, column: from - before.lastIndexOf('\n'), doc };
}

/**
 * Runs whose error the editor already marked. Kept outside the editor
 * component, so a remount (switching tabs or views) never moves the cursor
 * back to an old error.
 */
export const markedErrorRuns = new Set<number>();

/** Run ids kept in `markedErrorRuns`; older ones are forgotten (their runs are long replaced). */
const MAX_MARKED_RUNS = 200;

/** Records that a run's error was marked, keeping the set bounded. */
export function rememberMarkedRun(key: number): void {
  markedErrorRuns.add(key);
  if (markedErrorRuns.size > MAX_MARKED_RUNS) {
    const oldest = markedErrorRuns.values().next().value;
    if (oldest !== undefined) markedErrorRuns.delete(oldest);
  }
}
