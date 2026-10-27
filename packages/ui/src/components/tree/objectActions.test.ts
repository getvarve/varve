import type { Capabilities, ObjectKind, ObjectSummary } from '../../api/catalog';
import type { ConnectionProfile } from '../../api/records';
import type { MenuItem } from '../../lib/menu';
import { databaseChildren, schemaChildren } from '../../lib/tree';
import type { ObjNode } from '../../lib/tree';
import { designerIntent, pendingFocus } from '../table/designerIntent';
import { defaultAction, isObjectAction, objectMenuItems, runObjectAction } from './objectActions';

const opened: { fn: string; args: unknown[] }[] = [];
const record =
  (fn: string) =>
  (...args: unknown[]): void => {
    opened.push({ fn, args });
  };

/** Designer tabs the mocked `openTableDesigner` opened, so a second request finds the first. */
const designerTabs: { id: string; kind: 'tableDesigner'; connectionId: string; object: unknown }[] = [];

vi.mock('../../composables/useTabs', () => ({
  useTabs: () => ({
    tabs: designerTabs,
    activate: record('activate'),
    openObjectTab: record('openObjectTab'),
    openOverviewTab: record('openOverviewTab'),
    openDdlTab: record('openDdlTab'),
    openTableDataTab: record('openTableDataTab'),
    openTableDesigner: (...args: unknown[]) => {
      opened.push({ fn: 'openTableDesigner', args });
      const tab = { id: `t${designerTabs.length + 1}`, kind: 'tableDesigner' as const, connectionId: (args[0] as { id: string }).id, object: args[1] };
      designerTabs.push(tab);
      return tab;
    },
    openExplainTab: record('openExplainTab'),
    openLiveTab: record('openLiveTab'),
    openQueryTab: record('openQueryTab'),
    openKeysTab: record('openKeysTab'),
    openNotifyTab: record('openNotifyTab'),
    openPubSubTab: record('openPubSubTab'),
  }),
}));
vi.mock('./objectBrowser', () => ({
  useObjectBrowser: () => ({
    refresh: record('refresh'),
    capabilities: () => ({ engine: 'postgres', kinds: [], editable: ['table', 'view', 'schema', 'index'], features: [] }),
  }),
}));

function objectNode(kind: ObjectKind, name: string, hasChildren = false): Extract<ObjNode, { type: 'object' }> {
  const summary: ObjectSummary = {
    object: { kind, database: 'shop', schema: 'public', parent: null, name, signature: null },
    comment: null,
    detail: null,
    flags: [],
    hasChildren,
    system: false,
    rowEstimate: null,
    sizeBytes: null,
  };
  return { type: 'object', key: `k:${name}`, connectionId: 'pg', name, summary };
}

const pg: Capabilities = { engine: 'postgres', kinds: [], editable: ['table', 'view', 'schema', 'index'], features: [] };
const readOnlyKinds: Capabilities = { ...pg, editable: [] };
const ids = (items: { id: string }[]): string[] => items.map((i) => i.id);
const profile = { id: 'pg', engine: 'postgres', name: 'Orders' } as unknown as ConnectionProfile;

describe('object menus', () => {
  beforeEach(() => {
    opened.length = 0;
  });

  it('offers data, design and drop for an editable table', () => {
    expect(ids(objectMenuItems(objectNode('table', 'orders', true), pg))).toEqual([
      'open',
      'data',
      'select',
      'explain',
      'design',
      'ddl',
      'copy-name',
      'copy-qualified',
      'refresh',
      'drop',
    ]);
    expect(ids(objectMenuItems(objectNode('view', 'v'), pg))).toContain('drop');
  });

  it('offers no New, Edit or Drop for kinds the server edits but the UI has no editor for', () => {
    const group = (kind: ObjectKind): ObjNode => ({
      type: 'group',
      key: `g:${kind}`,
      connectionId: 'pg',
      container: { database: 'shop', schema: 'public', parent: { kind: 'table', database: 'shop', schema: 'public', parent: null, name: 'orders', signature: null } },
      kind,
      count: 1,
      name: kind,
    });
    // The server lists index as editable; it has no editor tab.
    const index = objectMenuItems(objectNode('index', 'orders_pkey'), pg);
    expect(ids(index)).not.toContain('edit');
    expect(ids(index)).not.toContain('drop');
    expect(index[0]?.label).toBe('Properties');
    const indexes = objectMenuItems(group('index'), pg);
    expect(ids(indexes)).toEqual(['refresh', 'no-editor']);
    expect(indexes[1]).toMatchObject({ disabled: true, label: expect.stringMatching(/table designer/) });
  });

  it('offers New schema, New database and Drop for databases and schemas', () => {
    const schema: ObjNode = {
      type: 'schema',
      key: 's',
      connectionId: 'pg',
      name: 'public',
      database: 'shop',
      summary: { ...objectNode('table', 'x').summary, object: { kind: 'schema', database: 'shop', schema: null, parent: null, name: 'public', signature: null } },
    };
    expect(ids(objectMenuItems(schema, pg))).toContain('drop');
    expect(ids(objectMenuItems(schema, readOnlyKinds))).not.toContain('drop');
    const db: ObjNode = {
      type: 'database',
      key: 'd',
      connectionId: 'pg',
      name: 'shop',
      current: true,
      summary: { ...objectNode('table', 'x').summary, object: { kind: 'database', database: null, schema: null, parent: null, name: 'shop', signature: null } },
    };
    const dbItems = ids(objectMenuItems(db, { ...pg, editable: [...pg.editable, 'database'] }));
    expect(dbItems).toContain('new-schema');
    expect(dbItems).toContain('new-database');
    expect(dbItems.at(-1)).toBe('drop');
    expect(dbItems).not.toContain('no-editor');
    expect(ids(objectMenuItems(db, pg))).not.toContain('new-database');
    runObjectAction('new-database', db, profile);
    expect(opened.at(-1)).toEqual({ fn: 'openObjectTab', args: [profile, { createKind: 'database', container: { database: null, schema: null, parent: null } }] });
    runObjectAction('drop', db, profile);
    expect(opened.at(-1)).toEqual({ fn: 'openObjectTab', args: [profile, { object: db.summary.object, intent: 'drop' }] });
  });

  it('offers New rule and New policy on PostgreSQL tables, and policy drops without parent refuse', () => {
    const caps: Capabilities = { ...pg, editable: [...pg.editable, 'rule', 'policy'] };
    const table = objectNode('table', 'orders');
    expect(ids(objectMenuItems(table, caps))).toEqual(expect.arrayContaining(['new-rule', 'new-policy']));
    expect(ids(objectMenuItems(objectNode('view', 'v'), caps))).toContain('new-rule');
    expect(ids(objectMenuItems(objectNode('view', 'v'), caps))).not.toContain('new-policy');
    expect(ids(objectMenuItems(table, { ...caps, engine: 'mysql' }))).not.toContain('new-rule');
    runObjectAction('new-policy', table, profile);
    expect(opened.at(-1)).toEqual({
      fn: 'openObjectTab',
      args: [profile, { createKind: 'policy', container: { database: 'shop', schema: 'public', parent: table.summary.object } }],
    });
    const policy = objectNode('policy', 'own_rows');
    expect(ids(objectMenuItems(policy, caps))).not.toContain('drop');
    const withTable = { ...policy, summary: { ...policy.summary, object: { ...policy.summary.object, parent: 'orders' } } };
    expect(ids(objectMenuItems(withTable, caps))).toContain('drop');
    expect(ids(objectMenuItems(withTable, caps))).toContain('edit');
  });

  it('offers no editor entries for kinds the server does not list as editable', () => {
    const items = ids(objectMenuItems(objectNode('table', 'orders'), readOnlyKinds));
    expect(items).not.toContain('design');
    expect(items).not.toContain('drop');
    expect(items).toContain('ddl');
    expect(ids(objectMenuItems(objectNode('view', 'v'), pg))).toContain('edit');
  });

  it('offers creation in a group only for kinds created from their group', () => {
    const group = (kind: ObjectKind): ObjNode => ({
      type: 'group',
      key: `g:${kind}`,
      connectionId: 'pg',
      container: { database: 'shop', schema: 'public', parent: null },
      kind,
      count: 1,
      name: kind,
    });
    expect(ids(objectMenuItems(group('view'), pg))).toEqual(['refresh', 'new-child']);
    expect(ids(objectMenuItems(group('function'), pg))).toEqual(['refresh']);
  });

  it('opens databases and schemas as an overview', () => {
    const db: ObjNode = {
      type: 'database',
      key: 'd',
      connectionId: 'pg',
      name: 'shop',
      current: true,
      summary: { ...objectNode('table', 'x').summary, object: { kind: 'database', database: null, schema: null, parent: null, name: 'shop', signature: null } },
    };
    expect(defaultAction(db)).toBe('overview');
    expect(ids(objectMenuItems(db, pg))[0]).toBe('overview');
    runObjectAction('overview', db, profile);
    expect(opened.at(-1)).toEqual({ fn: 'openOverviewTab', args: [profile, { database: 'shop' }] });
  });

  it('opens table-like objects as data and other objects as details', () => {
    expect(defaultAction(objectNode('materializedView', 'mv'))).toBe('data');
    expect(defaultAction(objectNode('function', 'f'))).toBe('open');
    runObjectAction('data', objectNode('table', 'orders'), profile);
    runObjectAction('new-child', { type: 'group', key: 'g', connectionId: 'pg', container: { database: 'shop', schema: 'public', parent: null }, kind: 'table', count: 0, name: 'Tables' }, profile);
    runObjectAction('select', objectNode('table', 'orders'), profile);
    expect(opened.map((o) => o.fn)).toEqual(['openTableDataTab', 'openTableDesigner', 'openQueryTab']);
    expect(opened[2]?.args).toEqual([profile, null, 'SELECT * FROM "public"."orders" LIMIT 100;', 'shop']);
  });

  it('opens index, constraint and partition nodes in the owning table designer, on that item', () => {
    designerTabs.length = 0;
    const child = (kind: ObjectKind, name: string): Extract<ObjNode, { type: 'object' }> => {
      const node = objectNode(kind, name);
      return { ...node, summary: { ...node.summary, object: { ...node.summary.object, parent: 'orders' } } };
    };
    const index = child('index', 'orders_pkey');
    expect(objectMenuItems(index, pg)[0]?.label).toBe('Open in table designer');
    expect(ids(objectMenuItems(index, pg))).not.toContain('edit');
    expect(defaultAction(index)).toBe('open');
    runObjectAction('open', index, profile);
    expect(opened.at(-2)).toEqual({
      fn: 'openTableDesigner',
      args: [profile, { kind: 'table', database: 'shop', schema: 'public', parent: null, name: 'orders', signature: null }, { database: 'shop', schema: 'public', parent: null }],
    });
    expect(opened.at(-1)).toEqual({ fn: 'activate', args: ['t1'] });
    expect(pendingFocus('t1')).toMatchObject({ section: 'indexes', name: 'orders_pkey' });

    // A second item of the same table reuses the open designer tab.
    runObjectAction('open', child('constraint', 'orders_amount_check'), profile);
    expect(opened.filter((o) => o.fn === 'openTableDesigner')).toHaveLength(1);
    expect(pendingFocus('t1')).toMatchObject({ section: 'constraints', name: 'orders_amount_check' });

    // A PostgreSQL partition opens its parent as a partitioned table, on the Partitions section.
    runObjectAction('open', child('partition', 'orders_2026_10'), { ...profile, name: 'Other', id: 'pg2' } as ConnectionProfile);
    expect(opened.filter((o) => o.fn === 'openTableDesigner').at(-1)?.args[1]).toMatchObject({ kind: 'partitionedTable', name: 'orders' });
    expect(pendingFocus('t2')).toMatchObject({ section: 'partitions', name: 'orders_2026_10' });
    expect(ids(objectMenuItems(child('partition', 'orders_2026_10'), pg))).toContain('design');
    runObjectAction('design', child('partition', 'orders_2026_10'), profile);
    expect(opened.at(-1)?.args[1]).toEqual({ kind: 'partition', database: 'shop', schema: 'public', parent: null, name: 'orders_2026_10', signature: null });

    // Without table edit rights the node keeps its read-only details tab.
    expect(objectMenuItems(index, readOnlyKinds)[0]?.label).toBe('Properties');
  });

  it('starts a partitioned table from the Partitioned tables group, and designs matview indexes', () => {
    designerTabs.length = 0;
    const group: ObjNode = {
      type: 'group',
      key: 'g',
      connectionId: 'pg',
      container: { database: 'shop', schema: 'public', parent: null },
      kind: 'partitionedTable',
      count: 0,
      name: 'Partitioned tables',
    };
    runObjectAction('new-child', group, profile);
    expect(opened.at(-1)).toEqual({ fn: 'openTableDesigner', args: [profile, null, group.container] });
    expect(designerIntent('t1').partitioned).toBe(true);
    runObjectAction('new-child', { ...group, kind: 'table' }, profile);
    expect(designerIntent('t2').partitioned).toBe(false);

    const mv = objectNode('materializedView', 'mv');
    expect(ids(objectMenuItems(mv, pg))).toContain('design');
    runObjectAction('design', mv, profile);
    expect(opened.at(-1)?.args[1]).toEqual(mv.summary.object);
    expect(pendingFocus('t3')).toMatchObject({ section: 'indexes', name: null });
    expect(ids(objectMenuItems(mv, readOnlyKinds))).not.toContain('design');
  });
});

describe('creating the first object of a kind', () => {
  const summaryOf = (kind: ObjectKind, name: string, database: string | null): ObjectSummary => ({
    object: { kind, database, schema: null, parent: null, name, signature: null },
    comment: null,
    detail: null,
    flags: [],
    hasChildren: true,
    system: false,
    rowEstimate: null,
    sizeBytes: null,
  });
  const schemaNode: Extract<ObjNode, { type: 'schema' }> = { type: 'schema', key: 's', connectionId: 'pg', name: 'fresh', database: 'shop', summary: summaryOf('schema', 'fresh', 'shop') };
  const dbNode = (connectionId: string): Extract<ObjNode, { type: 'database' }> => ({
    type: 'database',
    key: `d:${connectionId}`,
    connectionId,
    name: 'shop',
    current: true,
    summary: summaryOf('database', 'shop', null),
  });
  const submenu = (items: MenuItem[]): MenuItem | undefined => items.find((i) => i.id === 'new');
  const childIds = (items: MenuItem[]): string[] => (submenu(items)?.children ?? []).map((c) => c.id);

  /** What the PostgreSQL catalog lists as editable (crates/catalog/src/postgres relations and programs `editable`). */
  const pgFull: Capabilities = {
    engine: 'postgres',
    kinds: [],
    editable: ['database', 'schema', 'table', 'partitionedTable', 'index', 'view', 'materializedView', 'sequence', 'function', 'procedure', 'trigger', 'eventTrigger', 'enumType', 'compositeType', 'rangeType', 'domain', 'extension', 'rule', 'policy'],
    features: [],
  };
  const mysql: Capabilities = { engine: 'mysql', kinds: [], editable: ['database', 'table', 'view', 'function', 'procedure', 'trigger', 'event', 'index'], features: [] };
  const mariadb: Capabilities = { ...mysql, engine: 'mariadb', editable: [...mysql.editable, 'sequence'] };

  beforeEach(() => {
    opened.length = 0;
    designerTabs.length = 0;
  });

  it('offers every editable kind in a fresh PostgreSQL schema, which has no groups', () => {
    // A fresh schema: the catalog returns no kinds, so no group (and no group New entry) shows.
    expect(schemaChildren(schemaNode, [])).toEqual([]);
    const items = objectMenuItems(schemaNode, pgFull);
    expect(ids(items)).toContain('new-table');
    expect(childIds(items)).toEqual([
      'new-kind:partitionedTable',
      'new-kind:view',
      'new-kind:materializedView',
      'new-kind:sequence',
      'new-kind:function',
      'new-kind:procedure',
      'new-kind:enumType',
      'new-kind:compositeType',
      'new-kind:rangeType',
      'new-kind:domain',
    ]);
    expect(submenu(items)?.children?.map((c) => c.label)).toContain('New materialized view…');
    // Only kinds the server marks editable.
    expect(childIds(objectMenuItems(schemaNode, { ...pgFull, editable: ['table', 'view'] }))).toEqual(['new-kind:view']);
    expect(submenu(objectMenuItems(schemaNode, readOnlyKinds))).toBeUndefined();
  });

  it('opens the editor of each kind from the schema submenu, in that schema', () => {
    const container = { database: 'shop', schema: 'fresh', parent: null };
    for (const id of childIds(objectMenuItems(schemaNode, pgFull))) {
      expect(isObjectAction(id)).toBe(true);
      if (!isObjectAction(id)) continue;
      runObjectAction(id, schemaNode, profile);
      const kind = id.slice('new-kind:'.length);
      if (kind === 'partitionedTable') {
        expect(opened.at(-1)).toEqual({ fn: 'openTableDesigner', args: [profile, null, container] });
        expect(designerIntent('t1').partitioned).toBe(true);
      } else {
        expect(opened.at(-1)).toEqual({ fn: 'openObjectTab', args: [profile, { createKind: kind, container }] });
      }
    }
    expect(opened).toHaveLength(10);
  });

  it('offers event triggers and extensions on a PostgreSQL database, in the database', () => {
    const db = dbNode('pg');
    expect(databaseChildren(db, 'postgres', [], [], false)).toEqual([]);
    const items = objectMenuItems(db, pgFull);
    expect(childIds(items)).toEqual(['new-kind:extension', 'new-kind:eventTrigger']);
    runObjectAction('new-kind:eventTrigger', db, profile);
    expect(opened.at(-1)).toEqual({ fn: 'openObjectTab', args: [profile, { createKind: 'eventTrigger', container: { database: 'shop', schema: null, parent: null } }] });
    runObjectAction('new-kind:extension', db, profile);
    expect(opened.at(-1)?.args[1]).toMatchObject({ createKind: 'extension' });
  });

  it('offers views, routines, events and (MariaDB) sequences on a fresh MySQL or MariaDB database', () => {
    const my = dbNode('my');
    expect(databaseChildren(my, 'mysql', [], [], false)).toEqual([]);
    expect(ids(objectMenuItems(my, mysql))).toContain('new-table');
    expect(childIds(objectMenuItems(my, mysql))).toEqual(['new-kind:view', 'new-kind:function', 'new-kind:procedure', 'new-kind:event']);
    expect(childIds(objectMenuItems(my, mariadb))).toEqual(['new-kind:view', 'new-kind:function', 'new-kind:procedure', 'new-kind:event', 'new-kind:sequence']);
    const myProfile = { id: 'my', engine: 'mariadb', name: 'Maria' } as unknown as ConnectionProfile;
    for (const kind of ['view', 'function', 'procedure', 'event', 'sequence'] as const) {
      runObjectAction(`new-kind:${kind}`, my, myProfile);
      expect(opened.at(-1)).toEqual({ fn: 'openObjectTab', args: [myProfile, { createKind: kind, container: { database: 'shop', schema: null, parent: null } }] });
    }
  });

  it('offers New trigger on tables (and PostgreSQL views), opening the trigger editor on that relation', () => {
    const table = objectNode('table', 'orders');
    expect(ids(objectMenuItems(table, pgFull))).toContain('new-trigger');
    expect(ids(objectMenuItems(objectNode('partitionedTable', 'events'), pgFull))).toContain('new-trigger');
    expect(ids(objectMenuItems(objectNode('view', 'v'), pgFull))).toContain('new-trigger');
    expect(ids(objectMenuItems(objectNode('view', 'v'), mysql))).not.toContain('new-trigger');
    expect(ids(objectMenuItems(objectNode('table', 'orders'), mysql))).toContain('new-trigger');
    expect(ids(objectMenuItems(table, pg))).not.toContain('new-trigger');
    expect(ids(objectMenuItems(objectNode('function', 'f'), pgFull))).not.toContain('new-trigger');
    runObjectAction('new-trigger', table, profile);
    expect(opened.at(-1)).toEqual({ fn: 'openObjectTab', args: [profile, { createKind: 'trigger', container: { database: 'shop', schema: 'public', parent: table.summary.object } }] });
  });

  it('accepts only creatable kinds as New submenu ids', () => {
    expect(isObjectAction('new-kind:view')).toBe(true);
    expect(isObjectAction('new-kind:table')).toBe(true);
    expect(isObjectAction('new-kind:index')).toBe(false);
    expect(isObjectAction('new-kind:nonsense')).toBe(false);
    runObjectAction('new-kind:view', objectNode('table', 'orders'), profile);
    expect(opened).toHaveLength(0);
  });
});
