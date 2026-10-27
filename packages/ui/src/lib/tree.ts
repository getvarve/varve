import { displayName, kindLabel, objectIdentity } from '../api/catalog';
import type { Capabilities, Container, KindCount, ObjectKind, ObjectRef, ObjectSummary } from '../api/catalog';
import type { ConnectionProfile, Engine, Folder, ReorderKind, SavedQuery, WorkspaceRecords } from '../api/records';

export type TreeNode =
  | { kind: 'folder'; key: string; id: string; name: string; parentId: string | null; folder: Folder; children: TreeNode[] }
  | {
      kind: 'connection';
      key: string;
      id: string;
      name: string;
      parentId: string | null;
      connection: ConnectionProfile;
      children: TreeNode[];
    }
  | {
      kind: 'query';
      key: string;
      id: string;
      name: string;
      parentId: string | null;
      query: SavedQuery;
      /** True for the copy shown under its connection (design doc 3.4). */
      attached: boolean;
      children: TreeNode[];
    };

function bySortOrder<T extends { sortOrder: number; name: string }>(a: T, b: T): number {
  return a.sortOrder - b.sortOrder || a.name.localeCompare(b.name);
}

/**
 * Builds the folder tree from flat records. Folders come first, then
 * connections, then saved queries, each by manual order. Records whose
 * folder is missing, and folders whose parent chain loops, go to the top
 * level so nothing is ever hidden. Saved queries attached to a connection
 * also appear under that connection.
 */
export function buildTree(records: WorkspaceRecords): TreeNode[] {
  const folderIds = new Set(records.folders.map((f) => f.id));
  const parentOf = new Map(records.folders.map((f) => [f.id, f.parentId]));

  // A folder whose ancestors loop back to itself is shown at the top level.
  const loops = (id: string): boolean => {
    const seen = new Set<string>([id]);
    let cursor = parentOf.get(id) ?? null;
    while (cursor !== null) {
      if (seen.has(cursor)) return true;
      seen.add(cursor);
      cursor = parentOf.get(cursor) ?? null;
    }
    return false;
  };
  const home = (folderId: string | null): string | null =>
    folderId !== null && folderIds.has(folderId) ? folderId : null;

  const children = new Map<string | null, TreeNode[]>();
  const push = (parent: string | null, node: TreeNode): void => {
    const list = children.get(parent);
    if (list) list.push(node);
    else children.set(parent, [node]);
  };

  const attached = new Map<string, SavedQuery[]>();
  for (const q of [...records.queries].sort(bySortOrder)) {
    if (q.connectionId !== null) {
      const list = attached.get(q.connectionId) ?? [];
      list.push(q);
      attached.set(q.connectionId, list);
    }
  }

  for (const f of [...records.folders].sort(bySortOrder)) {
    const parent = loops(f.id) ? null : home(f.parentId);
    push(parent, { kind: 'folder', key: `folder:${f.id}`, id: f.id, name: f.name, parentId: parent, folder: f, children: [] });
  }
  for (const c of [...records.connections].sort(bySortOrder)) {
    const parent = home(c.folderId);
    const queries: TreeNode[] = (attached.get(c.id) ?? []).map((q) => ({
      kind: 'query',
      key: `attached:${c.id}:${q.id}`,
      id: q.id,
      name: q.name,
      parentId: parent,
      query: q,
      attached: true,
      children: [],
    }));
    push(parent, { kind: 'connection', key: `connection:${c.id}`, id: c.id, name: c.name, parentId: parent, connection: c, children: queries });
  }
  for (const q of [...records.queries].sort(bySortOrder)) {
    const parent = home(q.folderId);
    push(parent, { kind: 'query', key: `query:${q.id}`, id: q.id, name: q.name, parentId: parent, query: q, attached: false, children: [] });
  }

  const attach = (nodes: TreeNode[]): TreeNode[] => {
    for (const node of nodes) {
      if (node.kind === 'folder') node.children = attach(children.get(node.id) ?? []);
    }
    return nodes;
  };
  return attach(children.get(null) ?? []);
}

/**
 * Keeps nodes whose name contains `text` (case-insensitive) with their whole
 * subtree, and the ancestors of matches. An empty filter keeps everything.
 */
export function filterTree(nodes: readonly TreeNode[], text: string): TreeNode[] {
  const needle = text.trim().toLocaleLowerCase();
  if (needle === '') return [...nodes];
  const out: TreeNode[] = [];
  for (const node of nodes) {
    if (node.name.toLocaleLowerCase().includes(needle)) {
      out.push(node);
      continue;
    }
    const kept = filterTree(node.children, needle);
    if (kept.length > 0) out.push({ ...node, children: kept });
  }
  return out;
}

export interface VisibleRow {
  node: TreeNode;
  level: number;
  posInSet: number;
  setSize: number;
  parentKey: string | null;
  expanded: boolean;
}

/** The rows a tree shows, in order, for rendering and keyboard navigation. */
export function visibleRows(nodes: readonly TreeNode[], expanded: ReadonlySet<string>, expandAll = false): VisibleRow[] {
  const rows: VisibleRow[] = [];
  const walk = (list: readonly TreeNode[], level: number, parentKey: string | null): void => {
    list.forEach((node, i) => {
      const open = node.children.length > 0 && (expandAll || expanded.has(node.key));
      rows.push({ node, level, posInSet: i + 1, setSize: list.length, parentKey, expanded: open });
      if (open) walk(node.children, level + 1, node.key);
    });
  };
  walk(nodes, 1, null);
  return rows;
}

/** Folder ids from the top down to `folderId`, for breadcrumbs and cycle checks. */
export function folderPath(folders: readonly Folder[], folderId: string | null): Folder[] {
  const byId = new Map(folders.map((f) => [f.id, f]));
  const path: Folder[] = [];
  const seen = new Set<string>();
  let cursor = folderId;
  while (cursor !== null && !seen.has(cursor)) {
    seen.add(cursor);
    const folder = byId.get(cursor);
    if (!folder) break;
    path.unshift(folder);
    cursor = folder.parentId;
  }
  return path;
}

/** Folders as a flat list with depth, for folder pickers. */
export function folderOptions(folders: readonly Folder[]): { id: string; name: string; depth: number }[] {
  const out: { id: string; name: string; depth: number }[] = [];
  const walk = (nodes: readonly TreeNode[], depth: number): void => {
    for (const node of nodes) {
      if (node.kind !== 'folder') continue;
      out.push({ id: node.id, name: node.name, depth });
      walk(node.children, depth + 1);
    }
  };
  walk(buildTree({ folders: [...folders], connections: [], queries: [] }), 0);
  return out;
}

/** Where a dragged row lands relative to the row under the pointer. */
export type DropPosition = 'before' | 'inside' | 'after';

export type DropOperation =
  | { op: 'moveFolder'; id: string; parentId: string | null }
  | { op: 'moveConnection'; id: string; folderId: string | null }
  | { op: 'moveQuery'; id: string; folderId: string | null }
  | { op: 'reorder'; kind: ReorderKind; ids: string[] };

function reorderKind(node: TreeNode): ReorderKind {
  return node.kind === 'folder' ? 'folders' : node.kind === 'connection' ? 'connections' : 'queries';
}

function isInside(nodes: readonly TreeNode[], ancestorId: string, folderId: string | null): boolean {
  if (folderId === null) return false;
  const find = (list: readonly TreeNode[]): TreeNode | null => {
    for (const n of list) {
      if (n.kind === 'folder' && n.id === ancestorId) return n;
      const hit = find(n.children);
      if (hit) return hit;
    }
    return null;
  };
  const ancestor = find(nodes);
  if (!ancestor) return false;
  const contains = (n: TreeNode): boolean => n.kind === 'folder' && (n.id === folderId || n.children.some(contains));
  return contains(ancestor);
}

function siblingsIn(nodes: readonly TreeNode[], parentId: string | null): TreeNode[] {
  if (parentId === null) return [...nodes];
  const walk = (list: readonly TreeNode[]): TreeNode[] | null => {
    for (const n of list) {
      if (n.kind === 'folder' && n.id === parentId) return n.children;
      const hit = walk(n.children);
      if (hit) return hit;
    }
    return null;
  };
  return walk(nodes) ?? [];
}

/**
 * Plans the backend calls for a drop. Dropping inside a folder moves the item
 * there; dropping before or after a row moves the item to that row's folder
 * and, when both are the same kind, reorders the siblings. Returns `null` for
 * drops that do nothing, and an `error` for a folder dropped into itself.
 */
export function planDrop(
  roots: readonly TreeNode[],
  dragged: TreeNode,
  target: TreeNode | null,
  position: DropPosition,
): { operations: DropOperation[] } | { error: string } | null {
  if (dragged.kind === 'query' && dragged.attached) return null;
  if (target && target.kind === 'query' && target.attached) return null;
  if (target && target.key === dragged.key) return null;

  let parentId: string | null;
  if (target === null) parentId = null;
  else if (position === 'inside') {
    if (target.kind !== 'folder') return null;
    parentId = target.id;
  } else parentId = target.parentId;

  if (dragged.kind === 'folder' && (parentId === dragged.id || isInside(roots, dragged.id, parentId))) {
    return { error: 'A folder cannot be moved into itself or one of its subfolders.' };
  }

  const operations: DropOperation[] = [];
  if (parentId !== dragged.parentId) {
    if (dragged.kind === 'folder') operations.push({ op: 'moveFolder', id: dragged.id, parentId });
    else if (dragged.kind === 'connection') operations.push({ op: 'moveConnection', id: dragged.id, folderId: parentId });
    else operations.push({ op: 'moveQuery', id: dragged.id, folderId: parentId });
  }

  if (target !== null && position !== 'inside' && target.kind === dragged.kind) {
    const ids = siblingsIn(roots, parentId)
      .filter((n) => n.kind === dragged.kind && !(n.kind === 'query' && n.attached) && n.id !== dragged.id)
      .map((n) => n.id);
    const at = ids.indexOf(target.id);
    if (at >= 0) {
      ids.splice(position === 'before' ? at : at + 1, 0, dragged.id);
      operations.push({ op: 'reorder', kind: reorderKind(dragged), ids });
    }
  }
  return operations.length > 0 ? { operations } : null;
}

/** The node with `key`, anywhere in the tree. */
export function findNode(nodes: readonly TreeNode[], key: string): TreeNode | null {
  for (const node of nodes) {
    if (node.key === key) return node;
    const hit = findNode(node.children, key);
    if (hit) return hit;
  }
  return null;
}

/** Folders a node may move into: every folder, except a folder itself and its subfolders. */
export function moveTargets(nodes: readonly TreeNode[], node: TreeNode): { id: string; name: string; depth: number }[] {
  const out: { id: string; name: string; depth: number }[] = [];
  const walk = (list: readonly TreeNode[], depth: number): void => {
    for (const n of list) {
      if (n.kind !== 'folder') continue;
      if (node.kind === 'folder' && n.id === node.id) continue;
      out.push({ id: n.id, name: n.name, depth });
      walk(n.children, depth + 1);
    }
  };
  walk(nodes, 0);
  return out;
}

/**
 * The drop that moves `node` one place up or down among its siblings of the
 * same kind (keyboard reordering), or `null` at the edge.
 */
export function neighbourDrop(roots: readonly TreeNode[], node: TreeNode, direction: -1 | 1): { target: TreeNode; position: DropPosition } | null {
  const siblings = (node.parentId === null ? roots : (findNode(roots, `folder:${node.parentId}`)?.children ?? [])).filter(
    (n) => n.kind === node.kind && !(n.kind === 'query' && n.attached),
  );
  const at = siblings.findIndex((n) => n.key === node.key);
  const target = siblings[at + direction];
  if (at < 0 || !target) return null;
  return { target, position: direction < 0 ? 'before' : 'after' };
}

// Object browser: the lazily loaded objects under a connection.

/** Live server panels a `live` tab shows. */
export type LivePanel =
  | 'processes'
  | 'variables'
  | 'serverStatus'
  | 'galera'
  | 'redisInfo'
  | 'redisClients'
  | 'redisSlowlog'
  | 'redisLatency'
  | 'redisConfig'
  | 'redisMemory'
  | 'redisFunctions'
  | 'redisTopology'
  | 'redisModules';

export const LIVE_PANEL_LABELS: Record<LivePanel, string> = {
  processes: 'Processes',
  variables: 'Server variables',
  serverStatus: 'Server status',
  galera: 'Galera cluster',
  redisInfo: 'Server info',
  redisClients: 'Clients',
  redisSlowlog: 'Slow log',
  redisLatency: 'Latency',
  redisConfig: 'Config',
  redisMemory: 'Memory',
  redisFunctions: 'Functions',
  redisTopology: 'Cluster and Sentinel',
  redisModules: 'Modules and Search',
};

/** Tree entries that open a tool of the connection rather than an object. */
export type BrowserAction = 'keys' | 'console' | 'notify' | 'pubsub';

/** One node of the object tree under a connection. `key` is unique across the whole sidebar. */
export type ObjNode =
  | { type: 'database'; key: string; connectionId: string; name: string; summary: ObjectSummary; current: boolean }
  | { type: 'schema'; key: string; connectionId: string; database: string; name: string; summary: ObjectSummary }
  | { type: 'group'; key: string; connectionId: string; container: Container; kind: ObjectKind; count: number; name: string }
  | { type: 'object'; key: string; connectionId: string; name: string; summary: ObjectSummary }
  /** A fixed group (the server tools); its children never load. */
  | { type: 'section'; key: string; connectionId: string; name: string; children: ObjNode[] }
  | { type: 'panel'; key: string; connectionId: string; name: string; panel: LivePanel }
  | { type: 'redisDb'; key: string; connectionId: string; name: string; db: number; keys: string }
  | { type: 'action'; key: string; connectionId: string; name: string; action: BrowserAction };

/** The key the object tree of a connection hangs from. */
export function objectRootKey(connectionId: string): string {
  return `objects:${connectionId}`;
}

/** A child key: the parent's key plus an escaped part, so different names never collide. */
export function childKey(parent: string, part: string): string {
  return `${parent}/${encodeURIComponent(part)}`;
}

/** True for nodes that have (or may have) children. */
export function objectExpandable(node: ObjNode): boolean {
  switch (node.type) {
    case 'database':
    case 'schema':
    case 'group':
    case 'section':
      return true;
    case 'object':
      return node.summary.hasChildren;
    default:
      return false;
  }
}

/**
 * The database a node's catalog calls must run in. PostgreSQL browses each
 * database through its own session; MySQL and MariaDB use one session for all.
 */
export function nodeDatabase(node: ObjNode): string | null {
  switch (node.type) {
    case 'database':
      return node.name;
    case 'schema':
      return node.database;
    case 'group':
      return node.container.database;
    case 'object':
      return node.summary.object.database;
    default:
      return null;
  }
}

function groupNodes(parentKey: string, connectionId: string, container: Container, kinds: readonly KindCount[], skip: readonly ObjectKind[]): ObjNode[] {
  return kinds
    .filter((k) => k.count > 0 && !skip.includes(k.kind))
    .map((k) => ({
      type: 'group',
      key: childKey(parentKey, `g:${k.kind}`),
      connectionId,
      container,
      kind: k.kind,
      count: k.count,
      name: kindLabel(k.kind, true),
    }));
}

export interface SqlRootInput {
  connectionId: string;
  engine: Exclude<Engine, 'redis'>;
  capabilities: Capabilities;
  databases: readonly ObjectSummary[];
  /** Kinds of the server container (`database: null`), for example tablespaces. */
  serverKinds: readonly KindCount[];
  /** The database the browse session is connected to (PostgreSQL), marked as current. */
  currentDatabase: string | null;
  includeSystem: boolean;
}

/** Databases, server-wide groups and the server tools of a PostgreSQL, MySQL or MariaDB connection. */
export function sqlRootNodes(input: SqlRootInput): ObjNode[] {
  const root = objectRootKey(input.connectionId);
  const id = input.connectionId;
  const databases: ObjNode[] = input.databases
    .filter((d) => input.includeSystem || !d.system)
    .map((d) => ({
      type: 'database',
      key: childKey(root, `db:${d.object.name}`),
      connectionId: id,
      name: d.object.name,
      summary: d,
      current: d.object.name === input.currentDatabase,
    }));
  const server = groupNodes(root, id, { database: null, schema: null, parent: null }, input.serverKinds, ['database']);
  const sectionKey = childKey(root, 'server');
  const panels: LivePanel[] = ['processes', 'variables', 'serverStatus'];
  if (input.capabilities.features.includes('galera')) panels.push('galera');
  const tools: ObjNode[] = panels.map((panel) => ({
    type: 'panel',
    key: childKey(sectionKey, panel),
    connectionId: id,
    name: LIVE_PANEL_LABELS[panel],
    panel,
  }));
  if (input.engine === 'postgres') {
    tools.push({ type: 'action', key: childKey(sectionKey, 'notify'), connectionId: id, name: 'Notifications', action: 'notify' });
  }
  return [...databases, ...server, { type: 'section', key: sectionKey, connectionId: id, name: 'Server', children: tools }];
}

/**
 * Children of a database: for PostgreSQL its schemas, then the
 * database-wide groups (extensions, event triggers, ...); for MySQL and
 * MariaDB the object groups.
 */
export function databaseChildren(
  node: Extract<ObjNode, { type: 'database' }>,
  engine: Engine,
  kinds: readonly KindCount[],
  schemas: readonly ObjectSummary[],
  includeSystem: boolean,
): ObjNode[] {
  const container: Container = { database: node.name, schema: null, parent: null };
  if (engine !== 'postgres') return groupNodes(node.key, node.connectionId, container, kinds, []);
  const schemaNodes: ObjNode[] = schemas
    .filter((s) => includeSystem || !s.system)
    .map((s) => ({
      type: 'schema',
      key: childKey(node.key, `s:${s.object.name}`),
      connectionId: node.connectionId,
      database: node.name,
      name: s.object.name,
      summary: s,
    }));
  return [...schemaNodes, ...groupNodes(node.key, node.connectionId, container, kinds, ['schema'])];
}

/** The object groups of a PostgreSQL schema. */
export function schemaChildren(node: Extract<ObjNode, { type: 'schema' }>, kinds: readonly KindCount[]): ObjNode[] {
  return groupNodes(node.key, node.connectionId, { database: node.database, schema: node.name, parent: null }, kinds, []);
}

/** The objects of a group. */
export function groupChildren(node: Extract<ObjNode, { type: 'group' }>, objects: readonly ObjectSummary[]): ObjNode[] {
  return objects.map((summary) => ({
    type: 'object',
    key: childKey(node.key, objectIdentity(summary.object)),
    connectionId: node.connectionId,
    name: displayName(summary.object, false),
    summary,
  }));
}

/** The child groups of an object (a table's indexes, triggers, partitions, ...). */
export function objectChildren(node: Extract<ObjNode, { type: 'object' }>, kinds: readonly KindCount[]): ObjNode[] {
  const ref = node.summary.object;
  return groupNodes(node.key, node.connectionId, { database: ref.database, schema: ref.schema, parent: ref }, kinds, []);
}

/**
 * Tools, databases and server panels of a Redis connection. "Modules and
 * Search" shows only when `modules` says the server has a module or Search
 * (api/redis.ts `hasModules`; `true` when that could not be read).
 */
export function redisRootNodes(
  connectionId: string,
  keyspace: readonly { db: number; keys: string }[],
  serverMajor: number,
  modules: boolean,
): ObjNode[] {
  const root = objectRootKey(connectionId);
  const sectionKey = childKey(root, 'server');
  const panels: LivePanel[] = ['redisInfo', 'redisClients', 'redisSlowlog', 'redisLatency', 'redisConfig', 'redisMemory'];
  if (serverMajor >= 7) panels.push('redisFunctions');
  panels.push('redisTopology');
  if (modules) panels.push('redisModules');
  const tools: ObjNode[] = panels.map((panel) => ({
    type: 'panel',
    key: childKey(sectionKey, panel),
    connectionId,
    name: LIVE_PANEL_LABELS[panel],
    panel,
  }));
  tools.push({ type: 'action', key: childKey(sectionKey, 'pubsub'), connectionId, name: 'Pub/Sub', action: 'pubsub' });
  return [
    { type: 'action', key: childKey(root, 'keys'), connectionId, name: 'Browse keys', action: 'keys' },
    { type: 'action', key: childKey(root, 'console'), connectionId, name: 'Console', action: 'console' },
    ...[...keyspace]
      .sort((a, b) => a.db - b.db)
      .map(
        (d): ObjNode => ({ type: 'redisDb', key: childKey(root, `db:${d.db}`), connectionId, name: `db${d.db}`, db: d.db, keys: d.keys }),
      ),
    { type: 'section', key: sectionKey, connectionId, name: 'Server', children: tools },
  ];
}

/** The loading state of a node's children (or of a connection's object tree). */
export type ChildState =
  /** Not browsing yet: the user starts it (no password prompt without asking). */
  | { status: 'idle' }
  | { status: 'loading'; message: string }
  | { status: 'error'; title: string; message: string; reconnect: boolean }
  | { status: 'loaded'; nodes: ObjNode[] };

/** Where the flattening reads object children from. */
export interface ObjectSource {
  /** The object tree of a connection. */
  root(connectionId: string): ChildState;
  /** The children of an expandable object node; `undefined` when not asked for yet. */
  children(key: string): ChildState | undefined;
}

export type SidebarItem =
  | { type: 'node'; node: TreeNode }
  | { type: 'object'; node: ObjNode }
  | {
      type: 'status';
      /** Key of the node whose children this row stands for (a connection's tree: its object root key). */
      ownerKey: string;
      connectionId: string;
      state: Exclude<ChildState, { status: 'loaded' }> | { status: 'empty' };
    };

export interface SidebarRow {
  key: string;
  item: SidebarItem;
  level: number;
  posInSet: number;
  setSize: number;
  parentKey: string | null;
  expandable: boolean;
  expanded: boolean;
}

function itemKey(item: SidebarItem): string {
  if (item.type === 'status') return `${item.ownerKey}#status`;
  return item.node.key;
}

function itemName(item: SidebarItem): string | null {
  return item.type === 'status' ? null : item.node.name;
}

function connectionOf(item: SidebarItem): string | null {
  if (item.type === 'node') return item.node.kind === 'connection' ? item.node.id : null;
  if (item.type === 'status') return item.connectionId;
  return item.node.connectionId;
}

function stateItems(ownerKey: string, connectionId: string, state: ChildState | undefined): SidebarItem[] {
  if (state === undefined) return [{ type: 'status', ownerKey, connectionId, state: { status: 'loading', message: 'Loading…' } }];
  if (state.status === 'loaded') {
    if (state.nodes.length === 0) return [{ type: 'status', ownerKey, connectionId, state: { status: 'empty' } }];
    return state.nodes.map((node) => ({ type: 'object', node }));
  }
  return [{ type: 'status', ownerKey, connectionId, state }];
}

/** The children of a sidebar item, or `null` for a leaf. */
function itemChildren(item: SidebarItem, source: ObjectSource): SidebarItem[] | null {
  if (item.type === 'status') return null;
  if (item.type === 'node') {
    const node = item.node;
    if (node.kind === 'query') return null;
    const children: SidebarItem[] = node.children.map((child) => ({ type: 'node', node: child }));
    if (node.kind === 'folder') return children;
    return [...stateItems(objectRootKey(node.id), node.id, source.root(node.id)), ...children];
  }
  const node = item.node;
  if (!objectExpandable(node)) return null;
  if (node.type === 'section') return node.children.map((child) => ({ type: 'object', node: child }));
  return stateItems(node.key, node.connectionId, source.children(node.key));
}

/** Children that are already there, without status rows (what the filter searches). */
function loadedChildren(item: SidebarItem, source: ObjectSource): SidebarItem[] {
  return (itemChildren(item, source) ?? []).filter((child) => child.type !== 'status');
}

interface Kept {
  item: SidebarItem;
  /** The item itself matches: shown with its whole subtree. */
  full: boolean;
  /** Matching descendants, when the item itself does not match. */
  children: Kept[];
}

function keep(list: readonly SidebarItem[], needle: string, source: ObjectSource): Kept[] {
  const out: Kept[] = [];
  for (const item of list) {
    const name = itemName(item);
    if (name === null) continue;
    if (name.toLocaleLowerCase().includes(needle)) {
      out.push({ item, full: true, children: [] });
      continue;
    }
    const children = keep(loadedChildren(item, source), needle, source);
    if (children.length > 0) out.push({ item, full: false, children });
  }
  return out;
}

export interface SidebarState {
  /** Expanded folders and connections (persisted by the workspace). */
  expanded: ReadonlySet<string>;
  /** Expanded object nodes. */
  objectExpanded: ReadonlySet<string>;
  filter: string;
}

/**
 * The rows of the whole sidebar tree in order: folders, connections and
 * saved queries, and under each expanded connection its objects (or a
 * status row while they load, fail or are not browsed yet).
 *
 * With a filter, an item whose name matches is kept with its whole subtree
 * (folders and connections in it expanded, objects as the user left them),
 * and an item with matching descendants among what is loaded is kept,
 * expanded, with only those descendants. The filter never loads anything.
 */
export function sidebarRows(roots: readonly TreeNode[], state: SidebarState, source: ObjectSource): SidebarRow[] {
  const rows: SidebarRow[] = [];
  const needle = state.filter.trim().toLocaleLowerCase();

  const isExpanded = (item: SidebarItem, filtered: boolean): boolean => {
    if (item.type === 'node') return filtered || state.expanded.has(item.node.key);
    if (item.type === 'object') return state.objectExpanded.has(item.node.key);
    return false;
  };

  const walkAll = (list: readonly SidebarItem[], level: number, parentKey: string | null, filtered: boolean): void => {
    list.forEach((item, i) => {
      const children = itemChildren(item, source);
      const expandable = children !== null;
      const expanded = expandable && isExpanded(item, filtered);
      const key = itemKey(item);
      rows.push({ key, item, level, posInSet: i + 1, setSize: list.length, parentKey, expandable, expanded });
      if (expanded && children) walkAll(children, level + 1, key, filtered);
    });
  };

  const walkKept = (list: readonly Kept[], level: number, parentKey: string | null): void => {
    list.forEach((kept, i) => {
      const key = itemKey(kept.item);
      if (kept.full) {
        const children = itemChildren(kept.item, source);
        const expandable = children !== null;
        const expanded = expandable && isExpanded(kept.item, true);
        rows.push({ key, item: kept.item, level, posInSet: i + 1, setSize: list.length, parentKey, expandable, expanded });
        if (expanded && children) walkAll(children, level + 1, key, true);
        return;
      }
      rows.push({ key, item: kept.item, level, posInSet: i + 1, setSize: list.length, parentKey, expandable: true, expanded: true });
      walkKept(kept.children, level + 1, key);
    });
  };

  const top: SidebarItem[] = roots.map((node) => ({ type: 'node', node }));
  if (needle === '') walkAll(top, 1, null, false);
  else walkKept(keep(top, needle, source), 1, null);
  return rows;
}

/** The connection a sidebar row belongs to (itself for a connection row), or `null`. */
export function rowConnectionId(row: SidebarRow): string | null {
  return connectionOf(row.item);
}

/** Quotes an identifier for the engine: PostgreSQL `"name"`, MySQL and MariaDB `` `name` ``. */
export function quoteIdent(engine: Engine, name: string): string {
  if (engine === 'postgres') return `"${name.replaceAll('"', '""')}"`;
  return `\`${name.replaceAll('`', '``')}\``;
}

/** The schema (PostgreSQL) or database (MySQL, MariaDB) qualified, quoted name of an object. */
export function qualifiedName(engine: Engine, ref: ObjectRef): string {
  const scope = engine === 'postgres' ? ref.schema : ref.database;
  return scope !== null ? `${quoteIdent(engine, scope)}.${quoteIdent(engine, ref.name)}` : quoteIdent(engine, ref.name);
}

/** A query that reads the first rows of a table or view. */
export function selectRowsSql(engine: Engine, ref: ObjectRef, limit = 100): string {
  return `SELECT * FROM ${qualifiedName(engine, ref)} LIMIT ${limit};`;
}
