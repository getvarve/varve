import type { ColumnInfo, KeyInfo } from '../api/grid';
import type { Value } from '../api/sessions';
import { addInsert, emptyChanges, removeInsert, setCell, setInsertCell, toGridRows, toggleDelete } from './gridChanges';
import { createRowViewCache, RowVersions } from './gridRows';

function column(name: string): ColumnInfo {
  return {
    name,
    dataType: 'text',
    typeSchema: 'pg_catalog',
    typeName: 'text',
    baseType: null,
    array: false,
    arrayOfArrays: false,
    nullable: name !== 'id',
    hasDefault: false,
    generated: null,
    identity: null,
    autoIncrement: false,
    editable: true,
  };
}

const columns = [column('id'), column('doc')];
const key: KeyInfo = { kind: 'primary', name: 'pk', columns: ['id'], tableColumns: columns, equalityOperators: [], keyCollations: [] };
const text = (v: string): Value => ({ t: 'text', v });

function setup() {
  const rows = toGridRows(key, columns, [
    [text('1'), text('a')],
    [text('2'), text('b')],
  ]);
  const cs = emptyChanges();
  const versions = new RowVersions();
  const build = createRowViewCache();
  const view = (offset = 0, page = rows) => build({ rows: page, offset, changes: cs, versions, columnCount: columns.length });
  return { rows, cs, versions, view };
}

describe('row object cache', () => {
  it('reuses every row object when nothing changed', () => {
    const { view } = setup();
    const first = view();
    const second = view();
    expect(second[0]).toBe(first[0]);
    expect(second[1]).toBe(first[1]);
  });

  it('replaces a row whose source row was read again', () => {
    const { rows, view } = setup();
    const first = view();
    const reread = [{ ...(rows[0] as (typeof rows)[number]) }, rows[1] as (typeof rows)[number]];
    const second = view(0, reread);
    expect(second[0]).not.toBe(first[0]);
    expect(second[1]).toBe(first[1]);
  });

  it('replaces rows whose row number changed', () => {
    const { view } = setup();
    const first = view(0);
    const second = view(200);
    expect(second[0]).not.toBe(first[0]);
    expect(second[0]?.n).toBe(201);
  });

  it('replaces only the row whose pending change changed, even when the change set is edited in place', () => {
    const { rows, cs, versions, view } = setup();
    const r1 = rows[0];
    if (!r1) throw new Error('fixture');
    setCell(cs, 'postgres', key, columns, r1, 'doc', { kind: 'value', value: text('x') });
    versions.bump([r1.id]);
    const first = view();
    // Same entry object, new value: only the version tells the cache.
    setCell(cs, 'postgres', key, columns, r1, 'doc', { kind: 'value', value: text('y') });
    versions.bump([r1.id]);
    const second = view();
    expect(second[0]).not.toBe(first[0]);
    expect(second[0]?.pending.doc).toEqual({ kind: 'value', value: text('y') });
    expect(second[1]).toBe(first[1]);
  });

  it('replaces a row whose delete mark changed', () => {
    const { rows, cs, versions, view } = setup();
    const r2 = rows[1];
    if (!r2) throw new Error('fixture');
    const first = view();
    toggleDelete(cs, key, columns, r2);
    versions.bump([r2.id]);
    const second = view();
    expect(second[1]).not.toBe(first[1]);
    expect(second[1]?.deleted).toBe(true);
    expect(second[0]).toBe(first[0]);
  });

  it('keeps new rows until they change, and renumbers them when one is removed', () => {
    const { cs, versions, view } = setup();
    const a = addInsert(cs);
    const b = addInsert(cs);
    const first = view();
    expect(view()[3]).toBe(first[3]);
    setInsertCell(cs, columns, b, 'doc', { kind: 'value', value: text('z') });
    versions.bump([b]);
    const second = view();
    expect(second[3]).not.toBe(first[3]);
    expect(second[2]).toBe(first[2]);
    removeInsert(cs, a);
    versions.bump([a]);
    const third = view();
    expect(third[2]?.id).toBe(b);
    expect(third[2]?.n).toBe(1);
  });

  it('replaces every row after a change to all of them', () => {
    const { versions, view } = setup();
    const first = view();
    versions.bumpAll();
    const second = view();
    expect(second[0]).not.toBe(first[0]);
    expect(second[1]).not.toBe(first[1]);
  });
});
