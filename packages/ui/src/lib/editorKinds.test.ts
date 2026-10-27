import type { Capabilities, ObjectKind, ObjectRef } from '../api/catalog';
import { buildDrop } from './ddlPlan';
import { canDrop, canEdit, hasObjectEditor, hasTableDrop, lacksEditor } from './editorKinds';

const ref = (kind: ObjectKind, extra: Partial<ObjectRef> = {}): ObjectRef => ({ kind, database: 'shop', schema: 'public', parent: null, name: 'x', signature: null, ...extra });
const pg: Capabilities = {
  engine: 'postgres',
  kinds: [],
  editable: ['database', 'schema', 'table', 'partitionedTable', 'partition', 'rule', 'policy', 'index'],
  features: [],
};
const my: Capabilities = { engine: 'mysql', kinds: [], editable: ['database', 'table', 'partition', 'view'], features: [] };

describe('editor kinds', () => {
  it('has editors for databases, schemas, rules and policies', () => {
    for (const kind of ['database', 'schema', 'rule', 'policy'] as const) {
      expect(hasObjectEditor(kind)).toBe(true);
      expect(canEdit(kind, pg)).toBe(true);
      expect(lacksEditor(kind, pg)).toBe(false);
    }
    expect(lacksEditor('index', pg)).toBe(true);
    expect(canEdit('rule', { ...pg, editable: [] })).toBe(false);
  });

  it('drops tables, PostgreSQL partitions, databases and schemas', () => {
    expect(canDrop(ref('table'), pg)).toBe(true);
    expect(canDrop(ref('partitionedTable'), pg)).toBe(true);
    expect(canDrop(ref('partition'), pg)).toBe(true);
    expect(canDrop(ref('schema', { schema: null }), pg)).toBe(true);
    expect(canDrop(ref('database', { schema: null }), pg)).toBe(true);
    expect(canDrop(ref('database', { schema: null }), my)).toBe(true);
    expect(canDrop(ref('table', { schema: null }), my)).toBe(true);
    // MySQL partitions are dropped from the partitions overview, not as tables.
    expect(hasTableDrop('partition', my)).toBe(false);
    expect(canDrop(ref('partition', { schema: null }), my)).toBe(false);
    expect(canDrop(ref('index'), pg)).toBe(false);
  });

  it('drops rules and policies only with their table', () => {
    expect(canDrop(ref('rule'), pg)).toBe(false);
    expect(canDrop(ref('rule', { parent: 'orders' }), pg)).toBe(true);
    expect(canDrop(ref('policy', { parent: 'orders' }), pg)).toBe(true);
    expect(canDrop(ref('policy', { parent: 'orders' }), { ...pg, editable: ['rule'] })).toBe(false);
  });
});

describe('drop changes of the new kinds', () => {
  it('plans PostgreSQL drops with CASCADE where the kind has it and FORCE only when chosen', () => {
    expect(buildDrop('postgres', ref('table', { name: 'orders' }), true)?.change).toEqual({ object: 'table', action: 'drop', name: { schema: 'public', name: 'orders' }, behavior: 'cascade' });
    expect(buildDrop('postgres', ref('schema', { schema: null, name: 'sales' }), false)?.change).toEqual({ object: 'schema', action: 'drop', name: 'sales', behavior: 'restrict' });
    expect(buildDrop('postgres', ref('database', { schema: null, name: 'shop' }), true)?.change).toEqual({ object: 'database', action: 'drop', name: 'shop' });
    expect(buildDrop('postgres', ref('database', { schema: null, name: 'shop' }), false, { force: true })?.change).toEqual({ object: 'database', action: 'drop', name: 'shop', force: true });
    expect(buildDrop('postgres', ref('rule', { name: 'r', parent: 't' }), true)?.change).toEqual({ object: 'rule', action: 'drop', name: 'r', table: { schema: 'public', name: 't' }, behavior: 'cascade' });
    expect(buildDrop('postgres', ref('policy', { name: 'p', parent: 't' }), true)?.change).toEqual({ object: 'policy', action: 'drop', name: 'p', table: { schema: 'public', name: 't' } });
  });

  it('plans MySQL and MariaDB table and database drops', () => {
    expect(buildDrop('mysql', ref('table', { schema: null, name: 'orders' }), true)?.change).toEqual({ change: 'dropTable', database: 'shop', name: 'orders' });
    expect(buildDrop('mariadb', ref('database', { schema: null, name: 'shop' }), false)?.change).toEqual({ change: 'dropDatabase', name: 'shop' });
  });
});
