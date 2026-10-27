import { DATA_KINDS, displayName, kindLabel, TABLE_KINDS } from '../../api/catalog';
import type { Capabilities, Container, ObjectKind, ObjectRef } from '../../api/catalog';
import type { ConnectionProfile } from '../../api/records';
import { useTabs } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import { canDrop, canEdit, hasObjectEditor, lacksEditor } from '../../lib/editorKinds';
import type { MenuItem, MenuLeaf } from '../../lib/menu';
import { qualifiedName, quoteIdent, selectRowsSql } from '../../lib/tree';
import type { ObjNode } from '../../lib/tree';
import { requestFocus, requestPartitioned } from '../table/designerIntent';
import type { DesignerSection } from '../table/designerIntent';
import { useObjectBrowser } from './objectBrowser';

/** Ids of the object tree's menu entries. */
export type ObjectActionId =
  | 'overview'
  | 'open'
  | 'data'
  | 'select'
  | 'explain'
  | 'design'
  | 'edit'
  | 'ddl'
  | 'drop'
  | 'new-query'
  | 'new-child'
  | 'new-table'
  | 'new-schema'
  | 'new-database'
  | 'new-rule'
  | 'new-policy'
  | 'new-trigger'
  | NewKindActionId
  | 'refresh'
  | 'copy-name'
  | 'copy-qualified'
  | 'browse-keys';

/** A "New" submenu entry of a database or schema node: create the first (or another) object of this kind there. */
export type NewKindActionId = `new-kind:${ObjectKind}`;

/**
 * Kinds offered in the "New" submenu of a database node (besides New
 * schema / New table / New database, which have their own entries), per
 * engine. These are the kinds whose objects live directly in the database,
 * so their group (and its New entry) only shows once one exists.
 */
const DATABASE_NEW_KINDS: Record<'postgres' | 'mysql' | 'mariadb', readonly ObjectKind[]> = {
  postgres: ['extension', 'eventTrigger'],
  mysql: ['view', 'function', 'procedure', 'event'],
  mariadb: ['view', 'function', 'procedure', 'event', 'sequence'],
};

/** Kinds offered in the "New" submenu of a PostgreSQL schema node (besides New table). */
const SCHEMA_NEW_KINDS: readonly ObjectKind[] = [
  'partitionedTable',
  'view',
  'materializedView',
  'sequence',
  'function',
  'procedure',
  'enumType',
  'compositeType',
  'rangeType',
  'domain',
];

/** Relations a trigger is created on from their node, per engine (PostgreSQL views take INSTEAD OF triggers). */
function triggerTargets(engine: Capabilities['engine'] | undefined): readonly ObjectKind[] {
  return engine === 'postgres' ? ['table', 'partitionedTable', 'view'] : engine === 'mysql' || engine === 'mariadb' ? ['table'] : [];
}

/** The kinds a database or schema node's "New" submenu offers: editable on the server, with an editor here. */
export function newKinds(node: ObjNode, capabilities: Capabilities | null): ObjectKind[] {
  if (!capabilities || capabilities.engine === 'redis') return [];
  let kinds: readonly ObjectKind[];
  if (node.type === 'database') kinds = DATABASE_NEW_KINDS[capabilities.engine];
  else if (node.type === 'schema' && capabilities.engine === 'postgres') kinds = SCHEMA_NEW_KINDS;
  else return [];
  return kinds.filter((kind) => canEdit(kind, capabilities));
}

/** The kind of a `new-kind:<kind>` menu id, when it names a kind the UI can create. */
export function newKindOf(id: string): ObjectKind | null {
  if (!id.startsWith('new-kind:')) return null;
  const kind = id.slice('new-kind:'.length) as ObjectKind;
  return TABLE_KINDS.includes(kind) || hasObjectEditor(kind) ? kind : null;
}

/** The "New" submenu of a database or schema node, or `null` when it would be empty. */
function newSubmenu(node: ObjNode, capabilities: Capabilities | null): MenuItem | null {
  const children: MenuLeaf[] = newKinds(node, capabilities).map((kind) => ({
    id: `new-kind:${kind}`,
    label: `New ${kindLabel(kind).toLocaleLowerCase()}…`,
  }));
  return children.length > 0 ? { id: 'new', label: 'New', children } : null;
}

/** Children of a table (columns, constraints, partitions) and databases are not created from their group. */
const NOT_FROM_GROUP: readonly ObjectKind[] = ['column', 'constraint', 'partition', 'database'];

/** Children of a table that open the owning table's designer on that item, with the section they are in. */
const DESIGNER_CHILDREN: Partial<Record<ObjectKind, DesignerSection>> = {
  index: 'indexes',
  constraint: 'constraints',
  partition: 'partitions',
};

/** The section of the owning table's designer that shows `ref` (an index, constraint or partition of a table), or `null`. */
export function designerChildSection(ref: ObjectRef): DesignerSection | null {
  return ref.parent !== null ? (DESIGNER_CHILDREN[ref.kind] ?? null) : null;
}

/** Relations a PostgreSQL rule can be created on. */
const RULE_TARGETS: readonly ObjectKind[] = ['table', 'partitionedTable', 'view'];

/** Kinds that can be created from their group, with the editor that creates them. */
function creatable(kind: ObjectKind, capabilities: Capabilities | null): boolean {
  return canEdit(kind, capabilities) && !NOT_FROM_GROUP.includes(kind);
}

/** A disabled entry that says why there is no New entry (the server could, the UI has no editor). */
function noEditorItem(kind: ObjectKind): MenuItem {
  const what = kindLabel(kind).toLocaleLowerCase();
  const where = kind === 'index' ? 'use the table designer or a query' : 'use a query';
  return { id: 'no-editor', label: `New ${what}: no editor here, ${where}`, disabled: true };
}

function refOf(node: ObjNode): ObjectRef | null {
  if (node.type === 'object' || node.type === 'database' || node.type === 'schema') return node.summary.object;
  return null;
}

/**
 * The context menu of an object tree node. Editor entries (New, Edit,
 * Design, Drop) need both: the server lists the kind as editable, and the
 * UI has an editor or drop flow for it (`lib/editorKinds.ts`).
 */
export function objectMenuItems(node: ObjNode, capabilities: Capabilities | null): MenuItem[] {
  const editable = (kind: ObjectKind): boolean => canEdit(kind, capabilities);
  switch (node.type) {
    case 'database': {
      const items: MenuItem[] = [
        { id: 'overview', label: 'Open overview' },
        { id: 'new-query', label: 'New query' },
        { id: 'refresh', label: 'Refresh' },
        { id: 'open', label: 'Properties', separatorBefore: true },
        { id: 'ddl', label: 'Show DDL' },
      ];
      if (editable('schema')) items.push({ id: 'new-schema', label: 'New schema…' });
      else if (capabilities?.engine !== 'postgres' && editable('table')) items.push({ id: 'new-table', label: 'New table…' });
      else if (lacksEditor('schema', capabilities)) items.push(noEditorItem('schema'));
      if (editable('database')) items.push({ id: 'new-database', label: 'New database…' });
      const sub = newSubmenu(node, capabilities);
      if (sub) items.push(sub);
      items.push({ id: 'copy-name', label: 'Copy name' });
      if (canDrop(node.summary.object, capabilities)) items.push({ id: 'drop', label: 'Drop database…', danger: true, separatorBefore: true });
      return items;
    }
    case 'schema': {
      const items: MenuItem[] = [
        { id: 'overview', label: 'Open overview' },
        { id: 'new-query', label: 'New query' },
        { id: 'refresh', label: 'Refresh' },
        { id: 'open', label: 'Properties', separatorBefore: true },
        { id: 'ddl', label: 'Show DDL' },
      ];
      if (editable('table')) items.push({ id: 'new-table', label: 'New table…' });
      const sub = newSubmenu(node, capabilities);
      if (sub) items.push(sub);
      items.push({ id: 'copy-name', label: 'Copy name' });
      if (canDrop(node.summary.object, capabilities)) items.push({ id: 'drop', label: 'Drop schema…', danger: true, separatorBefore: true });
      return items;
    }
    case 'group': {
      const items: MenuItem[] = [{ id: 'refresh', label: 'Refresh' }];
      if (creatable(node.kind, capabilities)) {
        items.push({ id: 'new-child', label: `New ${kindLabel(node.kind).toLocaleLowerCase()}…` });
      } else if (lacksEditor(node.kind, capabilities) && !NOT_FROM_GROUP.includes(node.kind)) {
        items.push(noEditorItem(node.kind));
      }
      return items;
    }
    case 'object': {
      const kind = node.summary.object.kind;
      const data = DATA_KINDS.includes(kind);
      // Indexes, constraints and partitions are edited in their table's designer.
      const inDesigner = designerChildSection(node.summary.object) !== null && editable('table');
      const items: MenuItem[] = [
        { id: 'open', label: inDesigner ? 'Open in table designer' : editable(kind) && !TABLE_KINDS.includes(kind) ? 'Open' : 'Properties' },
      ];
      if (data) {
        items.push({ id: 'data', label: 'View data' });
        items.push({ id: 'select', label: 'Select rows in a new query' });
        items.push({ id: 'explain', label: 'Explain a select' });
      }
      if (TABLE_KINDS.includes(kind) && editable(kind)) items.push({ id: 'design', label: 'Design table' });
      // A PostgreSQL partition is a table of its own: its indexes, constraints and properties.
      else if (kind === 'partition' && capabilities?.engine === 'postgres' && editable('table')) items.push({ id: 'design', label: 'Design partition' });
      else if (kind === 'materializedView' && capabilities?.engine === 'postgres' && capabilities.editable.includes('index')) {
        // Its indexes are designed in the table designer (indexes only).
        items.push({ id: 'design', label: 'Design indexes' });
        if (editable(kind)) items.push({ id: 'edit', label: 'Edit…' });
      } else if (editable(kind) && !inDesigner) items.push({ id: 'edit', label: 'Edit…' });
      // Rules and policies are created on their table (their group only shows once one exists).
      if (capabilities?.engine === 'postgres') {
        if (RULE_TARGETS.includes(kind) && editable('rule')) items.push({ id: 'new-rule', label: 'New rule…' });
        if (TABLE_KINDS.includes(kind) && editable('policy')) items.push({ id: 'new-policy', label: 'New policy…' });
      }
      // Triggers too: their group under the table only shows once one exists.
      if (triggerTargets(capabilities?.engine).includes(kind) && editable('trigger')) items.push({ id: 'new-trigger', label: 'New trigger…' });
      items.push({ id: 'ddl', label: 'Show DDL' });
      items.push({ id: 'copy-name', label: 'Copy name', separatorBefore: true });
      if (node.summary.object.schema !== null || node.summary.object.database !== null) {
        items.push({ id: 'copy-qualified', label: 'Copy qualified name' });
      }
      if (node.summary.hasChildren) items.push({ id: 'refresh', label: 'Refresh' });
      if (canDrop(node.summary.object, capabilities)) {
        items.push({ id: 'drop', label: `Drop ${kindLabel(kind).toLocaleLowerCase()}…`, danger: true, separatorBefore: true });
      }
      return items;
    }
    case 'redisDb':
      return [{ id: 'browse-keys', label: 'Browse keys' }];
    case 'section':
      return [];
    case 'panel':
    case 'action':
      return [{ id: 'open', label: 'Open' }];
  }
}

/**
 * What Enter or a double click does: databases and schemas open their
 * overview, objects their data or details, tools open; groups and the
 * server section (`null`) expand instead.
 */
export function defaultAction(node: ObjNode): ObjectActionId | null {
  switch (node.type) {
    case 'database':
    case 'schema':
      return 'overview';
    case 'object':
      return DATA_KINDS.includes(node.summary.object.kind) ? 'data' : 'open';
    case 'panel':
    case 'action':
      return 'open';
    case 'redisDb':
      return 'browse-keys';
    default:
      return null;
  }
}

async function copy(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch (error: unknown) {
    useToasts().push('danger', 'Could not copy', error instanceof Error ? error.message : 'The clipboard is not available.');
  }
}

/** Runs a menu entry (or the default action) of an object tree node. */
export function runObjectAction(id: ObjectActionId, node: ObjNode, profile: ConnectionProfile): void {
  const tabs = useTabs();
  const browser = useObjectBrowser();
  const ref = refOf(node);
  switch (id) {
    case 'overview':
      if (node.type === 'database') tabs.openOverviewTab(profile, { database: node.name });
      else if (node.type === 'schema') tabs.openOverviewTab(profile, { database: node.database, schema: node.name });
      return;
    case 'open':
      if (node.type === 'panel') tabs.openLiveTab(profile, node.panel);
      else if (node.type === 'action') openTool(node, profile);
      else if (ref && node.type === 'object' && designerChildSection(ref) !== null && canEdit('table', browser.capabilities(node.connectionId))) {
        openOwnerDesigner(profile, ref);
      } else if (ref) tabs.openObjectTab(profile, { object: ref });
      return;
    case 'data':
      if (ref) tabs.openTableDataTab(profile, ref);
      return;
    case 'select':
      if (ref) tabs.openQueryTab(profile, null, selectRowsSql(profile.engine, ref), ref.database);
      return;
    case 'explain':
      if (ref) tabs.openExplainTab(profile, selectRowsSql(profile.engine, ref), ref.database);
      return;
    case 'design':
      if (!ref) return;
      if (ref.kind === 'materializedView') {
        const tab = tabs.openTableDesigner(profile, ref, { database: ref.database, schema: ref.schema, parent: null });
        requestFocus(tab.id, 'indexes', null);
      } else if (ref.kind === 'partition') {
        // The partition itself (a table of its own); its node is named by the parent table.
        tabs.openTableDesigner(profile, { ...ref, parent: null }, { database: ref.database, schema: ref.schema, parent: null });
      } else tabs.openTableDesigner(profile, ref, { database: ref.database, schema: ref.schema, parent: null });
      return;
    case 'edit':
      if (ref) tabs.openObjectTab(profile, { object: ref, intent: 'edit' });
      return;
    case 'drop':
      if (ref) tabs.openObjectTab(profile, { object: ref, intent: 'drop' });
      return;
    case 'ddl':
      if (ref) tabs.openDdlTab(profile, ref);
      return;
    case 'new-query':
      if (node.type === 'database') tabs.openQueryTab(profile, null, '', node.name);
      else if (node.type === 'schema') {
        // A new session starts on the default search path; this line puts the schema first when run.
        const sql = profile.engine === 'postgres' ? `SET search_path TO ${quoteIdent('postgres', node.name)}, public;\n` : '';
        tabs.openQueryTab(profile, null, sql, node.database);
      }
      return;
    case 'new-schema':
      if (node.type === 'database') tabs.openObjectTab(profile, { createKind: 'schema', container: { database: node.name, schema: null, parent: null } });
      return;
    case 'new-database':
      if (node.type === 'database') tabs.openObjectTab(profile, { createKind: 'database', container: { database: null, schema: null, parent: null } });
      return;
    case 'new-rule':
    case 'new-policy':
    case 'new-trigger':
      if (node.type === 'object' && ref) {
        const createKind = id === 'new-rule' ? 'rule' : id === 'new-policy' ? 'policy' : 'trigger';
        tabs.openObjectTab(profile, { createKind, container: { database: ref.database, schema: ref.schema, parent: ref } });
      }
      return;
    case 'new-table':
      if (node.type === 'database') tabs.openTableDesigner(profile, null, { database: node.name, schema: null, parent: null });
      else if (node.type === 'schema') tabs.openTableDesigner(profile, null, { database: node.database, schema: node.name, parent: null });
      return;
    case 'new-child':
      if (node.type !== 'group') return;
      if (TABLE_KINDS.includes(node.kind)) {
        const tab = tabs.openTableDesigner(profile, null, node.container);
        if (node.kind === 'partitionedTable') requestPartitioned(tab.id);
      } else tabs.openObjectTab(profile, { createKind: node.kind, container: node.container });
      return;
    case 'refresh':
      void browser.refresh(node.connectionId, node);
      return;
    case 'copy-name':
      void copy(ref ? displayName(ref, false) : node.name);
      return;
    case 'copy-qualified':
      if (ref) void copy(qualifiedName(profile.engine, ref));
      return;
    case 'browse-keys':
      if (node.type === 'redisDb') tabs.openKeysTab(profile, node.db);
      return;
    default:
      createInContainer(id, node, profile);
  }
}

/** A "New" submenu entry of a database or schema node: the table designer for table kinds, the object editor for the rest. */
function createInContainer(id: NewKindActionId, node: ObjNode, profile: ConnectionProfile): void {
  const kind = newKindOf(id);
  if (kind === null) return;
  let container: Container;
  if (node.type === 'database') container = { database: node.name, schema: null, parent: null };
  else if (node.type === 'schema') container = { database: node.database, schema: node.name, parent: null };
  else return;
  const tabs = useTabs();
  if (TABLE_KINDS.includes(kind)) {
    const tab = tabs.openTableDesigner(profile, null, container);
    if (kind === 'partitionedTable') requestPartitioned(tab.id);
  } else tabs.openObjectTab(profile, { createKind: kind, container });
}

/**
 * Opens the designer of the table that owns an index, constraint or
 * partition, on that item. A designer tab already open on the table is
 * reused (whatever kind it was opened as); otherwise the table is opened as
 * a PostgreSQL partitioned table (partitions), or as a table, which the
 * designer reads as a materialized view when it is one (an index node names
 * only its relation).
 */
function openOwnerDesigner(profile: ConnectionProfile, ref: ObjectRef): void {
  const tabs = useTabs();
  const section = designerChildSection(ref);
  if (section === null || ref.parent === null) return;
  const owner: ObjectRef = {
    kind: ref.kind === 'partition' && profile.engine === 'postgres' ? 'partitionedTable' : 'table',
    database: ref.database,
    schema: ref.schema,
    parent: null,
    name: ref.parent,
    signature: null,
  };
  const open = tabs.tabs.find(
    (t) =>
      t.kind === 'tableDesigner' &&
      t.connectionId === profile.id &&
      t.object !== null &&
      t.object.parent === null &&
      t.object.name === owner.name &&
      t.object.schema === owner.schema &&
      t.object.database === owner.database,
  );
  const tab = open ?? tabs.openTableDesigner(profile, owner, { database: owner.database, schema: owner.schema, parent: null });
  tabs.activate(tab.id);
  requestFocus(tab.id, section, ref.name);
}

function openTool(node: Extract<ObjNode, { type: 'action' }>, profile: ConnectionProfile): void {
  const tabs = useTabs();
  switch (node.action) {
    case 'keys':
      tabs.openKeysTab(profile);
      return;
    case 'console':
      tabs.openQueryTab(profile);
      return;
    case 'notify':
      tabs.openNotifyTab(profile);
      return;
    case 'pubsub':
      tabs.openPubSubTab(profile);
      return;
  }
}

/** True for menu ids this module handles. */
export function isObjectAction(id: string): id is ObjectActionId {
  if (newKindOf(id) !== null) return true;
  return (
    [
      'overview',
      'open',
      'data',
      'select',
      'explain',
      'design',
      'edit',
      'ddl',
      'drop',
      'new-query',
      'new-child',
      'new-table',
      'new-schema',
      'new-database',
      'new-rule',
      'new-policy',
      'new-trigger',
      'refresh',
      'copy-name',
      'copy-qualified',
      'browse-keys',
    ] as readonly string[]
  ).includes(id);
}
