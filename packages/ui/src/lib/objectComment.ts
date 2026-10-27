/**
 * Comments on objects that have no editor of their own (design doc 4.2,
 * "Comments E on every object"): which kinds take one, where the current
 * comment is in `catalog_describe`, and the change that sets it.
 *
 * PostgreSQL only: `COMMENT ON` exists for every kind the browser lists
 * (`PgChange` `comment`, crates/catalog/src/postgres/edit/object.rs).
 * MySQL and MariaDB have no `COMMENT ON`, and every object there that has a
 * comment also has an editor of its own: tables, columns, indexes and
 * partitions in the table designer, routines in the routine editor (which
 * changes only the comment with `ALTER PROCEDURE|FUNCTION`, keeping the
 * grants) and events in the event editor. Views, triggers and MariaDB
 * packages have none that can be changed on its own.
 *
 * The comment is stored exactly as typed (spaces and newlines included);
 * only a comment that is empty or all whitespace removes it.
 */
import type { ObjectKind, ObjectRef } from '../api/catalog';
import type { EngineChange, ObjectDetails } from '../api/ddl';
import type { Engine } from '../api/records';

/** PostgreSQL kinds without `COMMENT ON` (they do not exist on PostgreSQL). */
const PG_NO_COMMENT: readonly ObjectKind[] = ['event', 'package', 'packageBody'];

/** True when the generic comment editor can change the comment of `kind` on `engine`. */
export function commentSupported(engine: Engine, kind: ObjectKind): boolean {
  return engine === 'postgres' && !PG_NO_COMMENT.includes(kind);
}

/**
 * The comment in an object's details: relations carry it at `comment`,
 * the other PostgreSQL kinds at `meta.comment`. `undefined` when the
 * comment is unknown, not empty: the details have neither key, or say the
 * role cannot read it (`commentVisible: false`, a subscription on
 * PostgreSQL 13 without superuser rights).
 */
export function commentOf(details: ObjectDetails): string | null | undefined {
  const object = details.object as Record<string, unknown>;
  if (object.commentVisible === false) return undefined;
  const read = (value: unknown): string | null | undefined => (typeof value === 'string' ? value : value === null ? null : undefined);
  if ('comment' in object) return read(object.comment);
  const meta = object.meta;
  if (typeof meta === 'object' && meta !== null && 'comment' in meta) return read((meta as Record<string, unknown>).comment);
  return undefined;
}

/** The comment as it will be stored: exactly as typed, or `null` (removed) when it is empty or only whitespace. */
export function normalizeComment(text: string): string | null {
  return text.trim() === '' ? null : text;
}

/** Line endings as a textarea gives them back (it turns CRLF and CR into LF). */
export function sameLineEndings(text: string): string {
  return text.replace(/\r\n?/g, '\n');
}

/**
 * True when `draft` would change the stored comment `current`. Line endings
 * are compared normalized, so a stored CRLF comment the textarea shows with
 * LF is not a change until the user edits the text.
 */
export function commentChanged(draft: string, current: string | null): boolean {
  const a = normalizeComment(draft);
  const b = normalizeComment(current ?? '');
  return (a === null ? null : sameLineEndings(a)) !== (b === null ? null : sameLineEndings(b));
}

/** True when the details say the role cannot read the comment (`commentVisible: false`). */
export function commentHidden(details: ObjectDetails): boolean {
  return (details.object as Record<string, unknown>).commentVisible === false;
}

/**
 * `COMMENT ON <object> IS ...` (`IS NULL` for an empty comment), planned
 * through `edit_plan`; `null` for a kind {@link commentSupported} refuses.
 */
export function buildObjectComment(engine: Engine, object: ObjectRef, text: string): EngineChange | null {
  if (!commentSupported(engine, object.kind)) return null;
  return { engine: 'postgres', change: { object: 'comment', target: object, comment: normalizeComment(text) } };
}
