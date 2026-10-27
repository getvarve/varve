import type { ObjectKind, ObjectRef } from '../../api/ddl';
import type { SessionInfo } from '../../api/sessions';
import type { ServerTarget } from '../../lib/ddlPlan';

/**
 * Props every object editor in this directory takes (U1 passes them from
 * the tab). An editor edits `object`, or creates a new object of
 * `createKind` in `container` when `object` is `null`.
 */
export interface ObjectEditorProps {
  /** The tab's user session: engine, environment, read-only flag, server version, database. */
  session: SessionInfo;
  /** Display name of the connection; the production confirmation asks to type it. */
  connectionName: string;
  /** The object to edit; `null` to create one. */
  object: ObjectRef | null;
  /** Where a new object goes (database, PostgreSQL schema, parent table for triggers). Ignored when `object` is set. */
  container?: NewObjectContainer | null;
  /** Kind to create when `object` is `null` (for editors that handle several kinds). */
  createKind?: ObjectKind | null;
  /** Called with every failed backend call (the tab view passes `useTabs().reportTabError`). */
  reportError?: ((error: unknown) => void) | null;
  /** Called when the editor starts or stops holding changes that are not applied (the tab view passes `setUnsaved`). */
  reportDirty?: ((dirty: boolean) => void) | null;
  /** `drop` opens the editor with the drop of the object planned (tree "Drop"); other values open it normally. */
  intent?: 'view' | 'edit' | 'create' | 'drop' | null;
}

/** Props of `DdlView.vue`. */
export interface DdlViewProps {
  /** The tab's user session. */
  session: SessionInfo;
  /** Connection name for the destructive confirmation. */
  connectionName: string;
  /** The object whose DDL is shown. */
  object: ObjectRef;
  /** Called with every failed backend call (the tab view passes `useTabs().reportTabError`). */
  reportError?: ((error: unknown) => void) | null;
  /** Called when edited DDL that has not run starts or stops existing (the tab view passes `setUnsaved`). */
  reportDirty?: ((dirty: boolean) => void) | null;
}

/** Where a new object is created. */
export interface NewObjectContainer {
  database: string | null;
  schema: string | null;
  /** Parent table or view (triggers). */
  parent: string | null;
}

/**
 * Events every object editor emits:
 * - `changed`: something ran on the server; refresh the object tree.
 * - `opened`: the editor now shows this object (after create or rename); retitle the tab.
 * - `dropped`: the object was dropped; close the tab.
 * - `openSql`: open this SQL in a new query tab on the same connection.
 * - `openObject`: open another object (dependency, trigger function, partition).
 * - `createObject`: open the editor for a new object of this kind in this container.
 * - `reconnect`: the tab's profile changed identity; reconnect the tab.
 * - `switchDatabase`: reconnect the tab's session to this PostgreSQL database
 *   (the database editor, when the tab uses the database it drops or renames).
 */
export interface ObjectEditorEmits {
  changed: [];
  opened: [object: ObjectRef];
  dropped: [object: ObjectRef];
  openSql: [sql: string];
  openObject: [object: ObjectRef];
  createObject: [kind: ObjectKind, container: NewObjectContainer];
  reconnect: [];
  switchDatabase: [database: string];
}

/** The server an editor plans for, from the session. */
export function serverOf(session: SessionInfo): ServerTarget {
  return { engine: session.engine, major: session.server.major, minor: session.server.minor };
}

/** Kind labels for headers. */
export const KIND_LABELS: Partial<Record<ObjectKind, string>> = {
  view: 'View',
  materializedView: 'Materialized view',
  function: 'Function',
  procedure: 'Procedure',
  trigger: 'Trigger',
  eventTrigger: 'Event trigger',
  sequence: 'Sequence',
  event: 'Event',
  enumType: 'Enum',
  compositeType: 'Composite type',
  rangeType: 'Range type',
  domain: 'Domain',
  extension: 'Extension',
  database: 'Database',
  schema: 'Schema',
  rule: 'Rule',
  policy: 'Policy',
  table: 'Table',
  partitionedTable: 'Partitioned table',
  partition: 'Partition',
};

/** `schema.name` or `database.name` for headers. */
export function displayName(object: { schema: string | null; database: string | null; name: string; signature?: string | null }, engine: string): string {
  const prefix = engine === 'postgres' ? object.schema : object.database;
  const base = prefix ? `${prefix}.${object.name}` : object.name;
  return object.signature !== undefined && object.signature !== null ? `${base}(${object.signature})` : base;
}
