import { hasModules } from '../api/redis';
import type { FeatureSupport } from '../api/redis';
import type { ConnectionProfile, Folder, SavedQuery } from '../api/records';
import type { Capabilities, ObjectKind, ObjectSummary } from '../api/catalog';
import {
  buildTree,
  childKey,
  databaseChildren,
  filterTree,
  folderOptions,
  groupChildren,
  moveTargets,
  neighbourDrop,
  objectChildren,
  objectRootKey,
  planDrop,
  qualifiedName,
  quoteIdent,
  redisRootNodes,
  rowConnectionId,
  schemaChildren,
  selectRowsSql,
  sidebarRows,
  sqlRootNodes,
  visibleRows,
} from './tree';
import type { ChildState, ObjNode, ObjectSource, SidebarRow, TreeNode } from './tree';

function folder(id: string, parentId: string | null, sortOrder = 0, name = id): Folder {
  return { id, workspaceId: 'personal', parentId, name, color: null, sortOrder, createdAt: 0, updatedAt: 0, version: 1 };
}

function connection(id: string, folderId: string | null, sortOrder = 0): ConnectionProfile {
  return {
    id,
    workspaceId: 'personal',
    folderId,
    name: id,
    color: null,
    sortOrder,
    engine: 'postgres',
    host: 'localhost',
    port: 5432,
    database: null,
    username: null,
    environment: 'development',
    tls: { mode: 'prefer', caPath: null, clientCertPath: null, clientKeyPath: null },
    ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
    options: {
      readOnly: false,
      confirmDestructive: true,
      statementTimeoutMs: null,
      keepaliveS: null,
      initSql: null,
      showSystemObjects: false,
      redisDb: 0,
    },
    savedSecrets: [],
    createdAt: 0,
    updatedAt: 0,
    version: 1,
  };
}

function query(id: string, folderId: string | null, connectionId: string | null = null, sortOrder = 0): SavedQuery {
  return {
    id,
    workspaceId: 'personal',
    folderId,
    connectionId,
    database: null,
    name: id,
    sql: 'SELECT 1',
    tags: [],
    sortOrder,
    createdAt: 0,
    updatedAt: 0,
    version: 1,
  };
}

const keys = (nodes: readonly TreeNode[]): string[] => nodes.map((n) => n.key);

describe('buildTree', () => {
  it('nests folders and orders folders, connections and queries by sort order', () => {
    const tree = buildTree({
      folders: [folder('webshop', null, 1), folder('billing', null, 0), folder('archive', 'webshop', 0)],
      connections: [connection('orders', 'webshop', 2), connection('staging', 'webshop', 1), connection('local', null)],
      queries: [query('revenue', 'webshop'), query('scratch', null)],
    });
    expect(keys(tree)).toEqual(['folder:billing', 'folder:webshop', 'connection:local', 'query:scratch']);
    const webshop = tree[1];
    expect(webshop && keys(webshop.children)).toEqual([
      'folder:archive',
      'connection:staging',
      'connection:orders',
      'query:revenue',
    ]);
  });

  it('puts records with a missing folder and looping folders at the top level', () => {
    const tree = buildTree({
      folders: [folder('a', 'b'), folder('b', 'a')],
      connections: [connection('lost', 'deleted-folder')],
      queries: [],
    });
    expect(keys(tree).sort()).toEqual(['connection:lost', 'folder:a', 'folder:b']);
  });

  it('shows attached queries under their connection as well', () => {
    const tree = buildTree({ folders: [], connections: [connection('orders', null)], queries: [query('q1', null, 'orders')] });
    expect(keys(tree)).toEqual(['connection:orders', 'query:q1']);
    expect(keys(tree[0]?.children ?? [])).toEqual(['attached:orders:q1']);
  });
});

describe('filterTree and visibleRows', () => {
  const tree = buildTree({
    folders: [folder('webshop', null, 0, 'Webshop'), folder('legacy', null, 1, 'Legacy')],
    connections: [connection('Orders primary', 'webshop'), connection('CMS', 'legacy')],
    queries: [],
  });

  it('keeps matches with their ancestors', () => {
    const filtered = filterTree(tree, 'orders');
    expect(keys(filtered)).toEqual(['folder:webshop']);
    expect(keys(filtered[0]?.children ?? [])).toEqual(['connection:Orders primary']);
    expect(filterTree(tree, '  ')).toHaveLength(2);
  });

  it('lists expanded rows with level and position for aria', () => {
    const rows = visibleRows(tree, new Set(['folder:webshop']));
    expect(rows.map((r) => [r.node.key, r.level, r.posInSet, r.setSize])).toEqual([
      ['folder:webshop', 1, 1, 2],
      ['connection:Orders primary', 2, 1, 1],
      ['folder:legacy', 1, 2, 2],
    ]);
    expect(visibleRows(tree, new Set(), true)).toHaveLength(4);
  });

  it('lists folders with depth for pickers', () => {
    expect(folderOptions([folder('a', null), folder('b', 'a')])).toEqual([
      { id: 'a', name: 'a', depth: 0 },
      { id: 'b', name: 'b', depth: 1 },
    ]);
  });
});

describe('planDrop', () => {
  const tree = buildTree({
    folders: [folder('a', null, 0), folder('b', 'a', 0)],
    connections: [connection('c1', null, 0), connection('c2', null, 1), connection('c3', 'a', 0)],
    queries: [],
  });
  const find = (key: string): TreeNode => {
    const rows = visibleRows(tree, new Set(), true);
    const hit = rows.find((r) => r.node.key === key);
    if (!hit) throw new Error(key);
    return hit.node;
  };

  it('moves a connection into a folder', () => {
    expect(planDrop(tree, find('connection:c1'), find('folder:b'), 'inside')).toEqual({
      operations: [{ op: 'moveConnection', id: 'c1', folderId: 'b' }],
    });
  });

  it('reorders siblings of the same kind', () => {
    expect(planDrop(tree, find('connection:c2'), find('connection:c1'), 'before')).toEqual({
      operations: [{ op: 'reorder', kind: 'connections', ids: ['c2', 'c1'] }],
    });
  });

  it('moves and reorders when dropped next to a row in another folder', () => {
    expect(planDrop(tree, find('connection:c1'), find('connection:c3'), 'after')).toEqual({
      operations: [
        { op: 'moveConnection', id: 'c1', folderId: 'a' },
        { op: 'reorder', kind: 'connections', ids: ['c3', 'c1'] },
      ],
    });
  });

  it('refuses to move a folder into its own subfolder', () => {
    expect(planDrop(tree, find('folder:a'), find('folder:b'), 'inside')).toEqual({
      error: 'A folder cannot be moved into itself or one of its subfolders.',
    });
  });

  it('moves to the top level when dropped on the section', () => {
    expect(planDrop(tree, find('folder:b'), null, 'inside')).toEqual({
      operations: [{ op: 'moveFolder', id: 'b', parentId: null }],
    });
  });
});

describe('keyboard moves', () => {
  const tree = buildTree({
    folders: [folder('a', null, 0), folder('b', 'a', 0), folder('c', null, 1)],
    connections: [connection('c1', null, 0), connection('c2', null, 1)],
    queries: [],
  });
  const all = visibleRows(tree, new Set(), true).map((r) => r.node);
  const node = (key: string): TreeNode => {
    const hit = all.find((n) => n.key === key);
    if (!hit) throw new Error(key);
    return hit;
  };

  it('offers every folder except the moved folder and its subfolders', () => {
    expect(moveTargets(tree, node('folder:a')).map((f) => f.id)).toEqual(['c']);
    expect(moveTargets(tree, node('connection:c1')).map((f) => f.id)).toEqual(['a', 'b', 'c']);
  });

  it('moves one place up or down among siblings of the same kind', () => {
    const down = neighbourDrop(tree, node('connection:c1'), 1);
    expect(down?.target.key).toBe('connection:c2');
    expect(down && planDrop(tree, node('connection:c1'), down.target, down.position)).toEqual({
      operations: [{ op: 'reorder', kind: 'connections', ids: ['c2', 'c1'] }],
    });
    expect(neighbourDrop(tree, node('connection:c1'), -1)).toBeNull();
  });
});

// Object browser

function summary(kind: ObjectKind, name: string, extra: Partial<ObjectSummary['object']> = {}, more: Partial<ObjectSummary> = {}): ObjectSummary {
  return {
    object: { kind, database: null, schema: null, parent: null, name, signature: null, ...extra },
    comment: null,
    detail: null,
    flags: [],
    hasChildren: false,
    system: false,
    rowEstimate: null,
    sizeBytes: null,
    ...more,
  };
}

const pgCaps: Capabilities = { engine: 'postgres', kinds: ['database', 'schema', 'table'], editable: ['table'], features: [] };

function source(roots: Record<string, ChildState>, children: Record<string, ChildState> = {}): ObjectSource {
  return {
    root: (id) => roots[id] ?? { status: 'idle' },
    children: (key) => children[key],
  };
}

const labels = (rows: readonly SidebarRow[]): string[] =>
  rows.map((r) => {
    const indent = '  '.repeat(r.level - 1);
    if (r.item.type === 'status') return `${indent}[${r.item.state.status}]`;
    return `${indent}${r.item.node.name}${r.expandable ? (r.expanded ? ' -' : ' +') : ''}`;
  });

describe('object tree nodes', () => {
  it('lists databases, server groups and server tools for PostgreSQL, hiding system databases', () => {
    const nodes = sqlRootNodes({
      connectionId: 'c1',
      engine: 'postgres',
      capabilities: { ...pgCaps, features: ['galera'] },
      databases: [summary('database', 'shop'), summary('database', 'postgres'), summary('database', 'template1', {}, { system: true })],
      serverKinds: [
        { kind: 'database', count: 3 },
        { kind: 'tablespace', count: 2 },
      ],
      currentDatabase: 'shop',
      includeSystem: false,
    });
    expect(nodes.map((n) => `${n.type}:${n.name}`)).toEqual(['database:shop', 'database:postgres', 'group:Tablespaces', 'section:Server']);
    const shop = nodes[0];
    expect(shop?.type === 'database' && shop.current).toBe(true);
    const server = nodes.at(-1);
    expect(server?.type === 'section' && server.children.map((c) => c.name)).toEqual([
      'Processes',
      'Server variables',
      'Server status',
      'Galera cluster',
      'Notifications',
    ]);
    const tablespaces = nodes[2];
    expect(tablespaces?.type === 'group' && tablespaces.container).toEqual({ database: null, schema: null, parent: null });
  });

  it('puts PostgreSQL schemas before database-wide groups, and only groups for MySQL', () => {
    const [db] = sqlRootNodes({
      connectionId: 'c1',
      engine: 'postgres',
      capabilities: pgCaps,
      databases: [summary('database', 'shop')],
      serverKinds: [],
      currentDatabase: null,
      includeSystem: false,
    });
    if (db?.type !== 'database') throw new Error('expected a database');
    const kinds = [
      { kind: 'schema' as const, count: 2 },
      { kind: 'extension' as const, count: 1 },
      { kind: 'eventTrigger' as const, count: 0 },
    ];
    const schemas = [summary('schema', 'public', { database: 'shop' }), summary('schema', 'pg_catalog', { database: 'shop' }, { system: true })];
    expect(databaseChildren(db, 'postgres', kinds, schemas, false).map((n) => `${n.type}:${n.name}`)).toEqual([
      'schema:public',
      'group:Extensions',
    ]);
    expect(databaseChildren(db, 'postgres', kinds, schemas, true).map((n) => n.name)).toEqual(['public', 'pg_catalog', 'Extensions']);
    const mysql = databaseChildren(db, 'mysql', [{ kind: 'table', count: 4 }, { kind: 'event', count: 1 }], [], false);
    expect(mysql.map((n) => (n.type === 'group' ? `${n.name}(${n.count})` : n.name))).toEqual(['Tables(4)', 'Events(1)']);
    expect(mysql[0]?.type === 'group' && mysql[0].container).toEqual({ database: 'shop', schema: null, parent: null });
  });

  it('lists the objects of a group and the child groups of an object, with unique keys', () => {
    const schema: ObjNode = {
      type: 'schema',
      key: childKey(objectRootKey('c1'), 's:public'),
      connectionId: 'c1',
      database: 'shop',
      name: 'public',
      summary: summary('schema', 'public', { database: 'shop' }),
    };
    const [tables] = schemaChildren(schema, [{ kind: 'table', count: 2 }]);
    if (tables?.type !== 'group') throw new Error('expected a group');
    expect(tables.container).toEqual({ database: 'shop', schema: 'public', parent: null });
    const objects = groupChildren(tables, [
      summary('table', 'orders', { database: 'shop', schema: 'public' }, { hasChildren: true, detail: '4.1M' }),
      summary('table', 'orders/x', { database: 'shop', schema: 'public' }),
    ]);
    expect(new Set(objects.map((o) => o.key)).size).toBe(2);
    const orders = objects[0];
    if (orders?.type !== 'object') throw new Error('expected an object');
    const children = objectChildren(orders, [{ kind: 'index', count: 3 }]);
    expect(children[0]?.type === 'group' && children[0].container.parent).toEqual(orders.summary.object);
  });

  it('shows overloaded routines with their signature', () => {
    const group: ObjNode = { type: 'group', key: 'g', connectionId: 'c1', container: { database: 'd', schema: 's', parent: null }, kind: 'function', count: 2, name: 'Functions' };
    const nodes = groupChildren(group, [summary('function', 'f', { signature: 'integer' }), summary('function', 'f', { signature: 'text' })]);
    expect(nodes.map((n) => n.name)).toEqual(['f(integer)', 'f(text)']);
    expect(nodes[0]?.key).not.toBe(nodes[1]?.key);
  });

  it('builds the Redis tree with databases in order and functions only from Redis 7', () => {
    const nodes = redisRootNodes('r1', [{ db: 3, keys: '10' }, { db: 0, keys: '1200' }], 6, false);
    expect(nodes.map((n) => n.name)).toEqual(['Browse keys', 'Console', 'db0', 'db3', 'Server']);
    const server = nodes.at(-1);
    const names = server?.type === 'section' ? server.children.map((c) => c.name) : [];
    expect(names).not.toContain('Functions');
    expect(names).toContain('Pub/Sub');
    const seven = redisRootNodes('r1', [], 7, false).at(-1);
    expect(seven?.type === 'section' && seven.children.map((c) => c.name)).toContain('Functions');
  });

  it('shows Modules and Search only when the server has a module or Search', () => {
    const panels = (modules: boolean): string[] => {
      const server = redisRootNodes('r1', [], 8, modules).at(-1);
      return server?.type === 'section' ? server.children.map((c) => c.name) : [];
    };
    expect(panels(false)).not.toContain('Modules and Search');
    expect(panels(true)).toContain('Modules and Search');
    const feature = (available: boolean): FeatureSupport => ({ feature: 'search', available, reason: available ? null : 'missing' });
    // Redis 6.2 or Valkey without modules: nothing; Valkey's built-in lua module does not count.
    expect(hasModules({ features: [feature(false)], modules: [{ name: 'lua', version: '1' }], keyTypes: [] })).toBe(false);
    expect(hasModules({ features: [feature(false)], modules: null, keyTypes: [] })).toBe(false);
    // Neither COMMAND INFO nor MODULE LIST may run: the report cannot tell, so the panel stays.
    const unknown: FeatureSupport = { feature: 'search', available: false, reason: 'Varve cannot tell whether this server has Search: this user may not run COMMAND INFO or MODULE LIST' };
    expect(hasModules({ features: [unknown, { ...unknown, feature: 'json' }], modules: null, keyTypes: [] })).toBe(true);
    // Search (or any module feature), or another loaded module.
    expect(hasModules({ features: [feature(true)], modules: null, keyTypes: [] })).toBe(true);
    expect(hasModules({ features: [feature(false)], modules: [{ name: 'vectorset', version: '1' }], keyTypes: [] })).toBe(true);
  });
});

describe('sidebarRows', () => {
  const records = { folders: [folder('team', null)], connections: [connection('pg', 'team')], queries: [query('attached', null, 'pg')] };
  const tree = buildTree(records);
  const root = objectRootKey('pg');
  const db: ObjNode = { type: 'database', key: childKey(root, 'db:shop'), connectionId: 'pg', name: 'shop', summary: summary('database', 'shop'), current: true };
  const tables: ObjNode = {
    type: 'group',
    key: childKey(db.key, 'g:table'),
    connectionId: 'pg',
    container: { database: 'shop', schema: null, parent: null },
    kind: 'table',
    count: 2,
    name: 'Tables',
  };
  const loaded = (nodes: ObjNode[]): ChildState => ({ status: 'loaded', nodes });
  const objects = groupChildren(tables, [summary('table', 'orders', { database: 'shop' }), summary('table', 'customers', { database: 'shop' })]);

  it('shows a connect row under an expanded connection that is not browsed, before attached queries', () => {
    const rows = sidebarRows(tree, { expanded: new Set(['folder:team', 'connection:pg']), objectExpanded: new Set(), filter: '' }, source({}));
    // The saved query also sits at the top level, where its folder (none) puts it.
    expect(labels(rows)).toEqual(['team -', '  pg -', '    [idle]', '    attached', 'attached']);
    const status = rows[2];
    expect(status?.parentKey).toBe('connection:pg');
    expect(status && rowConnectionId(status)).toBe('pg');
  });

  it('shows loading rows for children not loaded yet and the loaded objects in order', () => {
    const state = { expanded: new Set(['folder:team', 'connection:pg']), objectExpanded: new Set([db.key, tables.key]), filter: '' };
    const loading = sidebarRows(tree, state, source({ pg: loaded([db]) }));
    expect(labels(loading)).toEqual(['team -', '  pg -', '    shop -', '      [loading]', '    attached', 'attached']);
    const rows = sidebarRows(tree, state, source({ pg: loaded([db]) }, { [db.key]: loaded([tables]), [tables.key]: loaded(objects) }));
    expect(labels(rows)).toEqual(['team -', '  pg -', '    shop -', '      Tables -', '        orders', '        customers', '    attached', 'attached']);
    const orders = rows[4];
    expect(orders?.posInSet).toBe(1);
    expect(orders?.setSize).toBe(2);
    expect(orders?.parentKey).toBe(tables.key);
    expect(orders?.level).toBe(5);
  });

  it('shows errors and empty results as status rows', () => {
    const state = { expanded: new Set(['folder:team', 'connection:pg']), objectExpanded: new Set([db.key]), filter: '' };
    const failed = sidebarRows(
      tree,
      state,
      source({ pg: loaded([db]) }, { [db.key]: { status: 'error', title: 'Reconnect needed', message: 'edited', reconnect: true } }),
    );
    expect(labels(failed)).toContain('      [error]');
    const empty = sidebarRows(tree, state, source({ pg: loaded([db]) }, { [db.key]: loaded([]) }));
    expect(labels(empty)).toContain('      [empty]');
  });

  it('filters loaded objects and keeps their ancestors expanded, without loading anything', () => {
    const state = { expanded: new Set<string>(), objectExpanded: new Set<string>(), filter: 'cust' };
    const rows = sidebarRows(tree, state, source({ pg: loaded([db]) }, { [db.key]: loaded([tables]), [tables.key]: loaded(objects) }));
    expect(labels(rows)).toEqual(['team -', '  pg -', '    shop -', '      Tables -', '        customers']);
    // Nothing loaded below the database: nothing to find there.
    const unloaded = sidebarRows(tree, state, source({ pg: loaded([db]) }));
    expect(unloaded).toEqual([]);
  });

  it('keeps a matching connection with its whole subtree, objects as the user left them', () => {
    const state = { expanded: new Set<string>(), objectExpanded: new Set<string>(), filter: 'pg' };
    const rows = sidebarRows(tree, state, source({ pg: loaded([db]) }, { [db.key]: loaded([tables]) }));
    expect(labels(rows)).toEqual(['team -', '  pg -', '    shop +', '    attached']);
  });
});

describe('identifier quoting', () => {
  it('quotes and escapes per engine', () => {
    expect(quoteIdent('postgres', 'we"ird')).toBe('"we""ird"');
    expect(quoteIdent('mysql', 'we`ird')).toBe('`we``ird`');
    const ref = { kind: 'table' as const, database: 'shop', schema: 'sales', parent: null, name: 'orders', signature: null };
    expect(qualifiedName('postgres', ref)).toBe('"sales"."orders"');
    expect(qualifiedName('mariadb', ref)).toBe('`shop`.`orders`');
    expect(selectRowsSql('mysql', ref, 10)).toBe('SELECT * FROM `shop`.`orders` LIMIT 10;');
  });
});
