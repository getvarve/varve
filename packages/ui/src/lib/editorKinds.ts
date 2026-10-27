/**
 * Which object kinds the UI can actually edit.
 *
 * `Capabilities.editable` lists what the backend can plan for a server
 * (including kinds such as indexes that have no editor here).
 * The UI offers New, Edit and Drop only where it also has an editor for the
 * kind: the object editor tab (`components/objects/ObjectTabView.vue`, whose
 * editor map is typed by {@link OBJECT_EDITOR_KINDS}) or the table designer
 * (`TABLE_KINDS`).
 */
import { TABLE_KINDS } from '../api/catalog';
import type { Capabilities, ObjectKind, ObjectRef } from '../api/catalog';
import { buildDrop } from './ddlPlan';

/** Kinds the object editor tab has an editor for. */
export const OBJECT_EDITOR_KINDS = [
  'view',
  'materializedView',
  'function',
  'procedure',
  'trigger',
  'eventTrigger',
  'sequence',
  'event',
  'enumType',
  'compositeType',
  'rangeType',
  'domain',
  'extension',
  'database',
  'schema',
  'rule',
  'policy',
] as const satisfies readonly ObjectKind[];

/** A kind with an editor in the object editor tab. */
export type EditorKind = (typeof OBJECT_EDITOR_KINDS)[number];

/** True when the object editor tab has an editor for `kind`. */
export function hasObjectEditor(kind: ObjectKind): kind is EditorKind {
  return (OBJECT_EDITOR_KINDS as readonly ObjectKind[]).includes(kind);
}

function serverEditable(kind: ObjectKind, capabilities: Capabilities | null): boolean {
  return capabilities?.editable.includes(kind) ?? false;
}

/** The server can edit `kind` and the UI has an editor (object editor or table designer) for it. */
export function canEdit(kind: ObjectKind, capabilities: Capabilities | null): boolean {
  return serverEditable(kind, capabilities) && (TABLE_KINDS.includes(kind) || hasObjectEditor(kind));
}

/**
 * The server can drop the object and the UI has a drop flow for it: an
 * object editor that plans `buildDrop`, or for tables (and PostgreSQL
 * partitions) the drop view of the object tab.
 */
export function canDrop(object: ObjectRef, capabilities: Capabilities | null): boolean {
  if (!capabilities || !serverEditable(object.kind, capabilities)) return false;
  if (!hasObjectEditor(object.kind) && !hasTableDrop(object.kind, capabilities)) return false;
  return buildDrop(capabilities.engine, object, false) !== null;
}

/** Table kinds the object tab drops (its drop view): tables, and PostgreSQL partitions. */
export function hasTableDrop(kind: ObjectKind, capabilities: Capabilities | null): boolean {
  return TABLE_KINDS.includes(kind) || (kind === 'partition' && capabilities?.engine === 'postgres');
}

/** Kinds the server can create but the UI has no editor for: the menu says so instead of opening an empty tab. */
export function lacksEditor(kind: ObjectKind, capabilities: Capabilities | null): boolean {
  return serverEditable(kind, capabilities) && !TABLE_KINDS.includes(kind) && !hasObjectEditor(kind);
}
