import type { ColumnInfo, KeyInfo } from '../api/grid';
import type { Value } from '../api/sessions';
import {
  applyFailureAction,
  addInsert,
  buildRowChanges,
  changeCounts,
  changeLabels,
  describeProblem,
  discardRef,
  emptyChanges,
  mergeApplied,
  missingRequired,
  refreshSnapshots,
  revertCell,
  setCell,
  setInsertCell,
  toGridRows,
  toggleDelete,
} from './gridChanges';

function column(name: string, extra: Partial<ColumnInfo> = {}): ColumnInfo {
  return {
    name,
    dataType: 'text',
    typeSchema: 'pg_catalog',
    typeName: 'text',
    baseType: null,
    array: false,
    arrayOfArrays: false,
    nullable: true,
    hasDefault: false,
    generated: null,
    identity: null,
    autoIncrement: false,
    editable: true,
    ...extra,
  };
}

const columns: ColumnInfo[] = [
  column('id', { dataType: 'bigint', typeName: 'int8', nullable: false, hasDefault: true, identity: 'always', editable: false }),
  column('status', { nullable: false }),
  column('note'),
  column('total_x2', { generated: 'stored', editable: false }),
  column('tags', { array: true, arrayOfArrays: true, typeName: '_dd' }),
];

const key: KeyInfo = {
  kind: 'primary',
  name: 'orders_pkey',
  columns: ['id'],
  tableColumns: columns,
  equalityOperators: [{ schema: 'pg_catalog', name: '=' }],
  keyCollations: [null],
};

const int = (v: number): Value => ({ t: 'int', v });
const text = (v: string): Value => ({ t: 'text', v });
const NULL: Value = { t: 'null' };

function page() {
  return toGridRows(key, columns, [
    [int(1), text('paid'), NULL, int(2), NULL],
    [int(2), text('pending'), text('x'), int(4), NULL],
  ]);
}

describe('change tracking', () => {
  it('tracks edits by key and drops edits that restore the value', () => {
    const cs = emptyChanges();
    const [r1] = page();
    if (!r1) throw new Error('no row');
    setCell(cs, 'postgres', key, columns, r1, 'status', { kind: 'value', value: text('shipped') });
    expect(changeCounts(cs)).toEqual({ updated: 1, inserted: 0, deleted: 0, total: 1 });
    setCell(cs, 'postgres', key, columns, r1, 'status', { kind: 'value', value: text('paid') });
    expect(changeCounts(cs).total).toBe(0);
  });

  it('refuses writes the server would reject', () => {
    const cs = emptyChanges();
    const [r1] = page();
    if (!r1) throw new Error('no row');
    expect(() => setCell(cs, 'postgres', key, columns, r1, 'id', { kind: 'value', value: int(9) })).toThrow(/GENERATED ALWAYS/);
    expect(() => setCell(cs, 'postgres', key, columns, r1, 'total_x2', { kind: 'value', value: int(9) })).toThrow(/Generated/);
    expect(() => setCell(cs, 'postgres', key, columns, r1, 'tags', { kind: 'value', value: text('{}') })).toThrow(/domain over an array/);
    expect(() => setCell(cs, 'postgres', key, columns, r1, 'status', { kind: 'value', value: NULL })).toThrow(/NOT NULL/);
    toggleDelete(cs, key, columns, r1);
    expect(() => setCell(cs, 'postgres', key, columns, r1, 'note', { kind: 'value', value: text('a') })).toThrow(/deletion/);
  });

  it('builds deletes, then updates, then inserts with their keys and originals', () => {
    const cs = emptyChanges();
    const [r1, r2] = page();
    if (!r1 || !r2) throw new Error('no rows');
    setCell(cs, 'postgres', key, columns, r1, 'note', { kind: 'expression', value: "'a' || 'b'" });
    toggleDelete(cs, key, columns, r2);
    const id = addInsert(cs);
    setInsertCell(cs, columns, id, 'status', { kind: 'value', value: text('new') });
    const { changes, refs } = buildRowChanges(cs, columns);
    expect(changes).toEqual([
      {
        op: 'delete',
        key: { columns: [['id', int(2)]] },
        original: [
          ['id', int(2)],
          ['status', text('pending')],
          ['note', text('x')],
          ['total_x2', int(4)],
        ],
      },
      {
        op: 'update',
        key: { columns: [['id', int(1)]] },
        original: [['note', NULL]],
        set: [['note', { kind: 'expression', value: "'a' || 'b'" }]],
      },
      { op: 'insert', values: [['status', { kind: 'value', value: text('new') }]] },
    ]);
    expect(refs.map((r) => r.kind)).toEqual(['delete', 'update', 'insert']);
  });

  it('reports a required column a new row leaves out', () => {
    const cs = emptyChanges();
    const id = addInsert(cs);
    const row = cs.inserts.find((r) => r.id === id);
    if (!row) throw new Error('no insert');
    expect(missingRequired(columns, row)).toBe('status');
    setInsertCell(cs, columns, id, 'status', { kind: 'value', value: text('ok') });
    expect(missingRequired(columns, row)).toBeNull();
    expect(() => setInsertCell(cs, columns, id, 'id', { kind: 'value', value: int(5) })).toThrow();
  });

  it('refreshes snapshots after a reload so the next plan compares with the new values', () => {
    const cs = emptyChanges();
    const [r1] = page();
    if (!r1) throw new Error('no row');
    setCell(cs, 'postgres', key, columns, r1, 'status', { kind: 'value', value: text('shipped') });
    setCell(cs, 'postgres', key, columns, r1, 'note', { kind: 'value', value: text('n') });
    // Someone else set note to 'n' and status to 'refunded'.
    const reloaded = toGridRows(key, columns, [[int(1), text('refunded'), text('n'), int(2), NULL]]);
    expect(refreshSnapshots(cs, 'postgres', columns, reloaded)).toEqual([reloaded[0]?.id]);
    const { changes } = buildRowChanges(cs, columns);
    expect(changes).toEqual([
      {
        op: 'update',
        key: { columns: [['id', int(1)]] },
        original: [['status', text('refunded')]],
        set: [['status', { kind: 'value', value: text('shipped') }]],
      },
    ]);
  });

  it('merges applied rows and clears the change set', () => {
    const cs = emptyChanges();
    const rows = page();
    const [r1, r2] = rows;
    if (!r1 || !r2) throw new Error('no rows');
    setCell(cs, 'postgres', key, columns, r1, 'note', { kind: 'value', value: text('n') });
    toggleDelete(cs, key, columns, r2);
    addInsert(cs);
    const { refs } = buildRowChanges(cs, columns);
    const merged = mergeApplied(cs, key, columns, rows, refs, [
      { change: 0, op: 'delete', row: null },
      { change: 1, op: 'update', row: [['id', int(1)], ['status', text('paid')], ['note', text('n')], ['total_x2', int(2)], ['tags', NULL]] },
      { change: 2, op: 'insert', row: [['id', int(3)], ['status', text('s')], ['note', NULL], ['total_x2', int(0)], ['tags', NULL]] },
    ]);
    expect(merged.rows.map((r) => r.values[0])).toEqual([int(1), int(3)]);
    expect(merged.rows[0]?.values[2]).toEqual(text('n'));
    expect(merged.needsReload).toBe(false);
    expect(changeCounts(cs).total).toBe(0);
  });

  it('asks for a reload when a written row cannot be read back', () => {
    const cs = emptyChanges();
    addInsert(cs);
    const { refs } = buildRowChanges(cs, columns);
    expect(mergeApplied(cs, key, columns, page(), refs, [{ change: 0, op: 'insert', row: null }]).needsReload).toBe(true);
  });
});

describe('conflicts', () => {
  it('names the row a conflict belongs to and lets the user drop that change', () => {
    const cs = emptyChanges();
    const [r1, r2] = page();
    if (!r1 || !r2) throw new Error('no rows');
    setCell(cs, 'postgres', key, columns, r1, 'note', { kind: 'value', value: text('n') });
    setCell(cs, 'postgres', key, columns, r2, 'note', { kind: 'value', value: text('m') });
    const { refs } = buildRowChanges(cs, columns);
    const problem = describeProblem({ status: 'conflict', change: 1, reason: 'rowChanged', rowsMatched: 0, message: 'No row matched.' }, refs);
    expect(problem.conflict).toBe(true);
    expect(problem.ref).toEqual({ kind: 'update', id: r2.id });
    expect(problem.message).toMatch(/Nothing was applied/);
    if (problem.ref) discardRef(cs, problem.ref);
    expect(Object.keys(cs.updates)).toEqual([r1.id]);
  });

  it('explains key and server failures', () => {
    const refs = [{ kind: 'delete' as const, id: 'x' }];
    expect(describeProblem({ status: 'conflict', change: 0, reason: 'keyNotUnique', rowsMatched: 2, message: '2 rows matched.' }, refs).title).toMatch(/more than one/);
    const failed = describeProblem(
      { status: 'failed', change: 0, error: { kind: 'query', message: 'violates foreign key', code: '23503', detail: 'Key is still referenced.', hint: null, position: null } },
      refs,
    );
    expect(failed.conflict).toBe(false);
    expect(failed.message).toContain('23503');
    expect(failed.message).toContain('still referenced');
  });

  it('labels planned changes by key', () => {
    const [r1] = page();
    if (!r1) throw new Error('no row');
    expect(changeLabels([{ kind: 'update', id: r1.id }, { kind: 'insert', id: 'new:1' }])).toEqual(['Row id = 1', 'New row 1']);
  });

  it('reverts a single cell', () => {
    const cs = emptyChanges();
    const [r1] = page();
    if (!r1) throw new Error('no row');
    setCell(cs, 'postgres', key, columns, r1, 'note', { kind: 'value', value: text('n') });
    revertCell(cs, r1.id, 'note');
    expect(changeCounts(cs).total).toBe(0);
  });
});

describe('apply failures', () => {
  it('plans again when the plan expired, keeps it for an open transaction and closes otherwise', () => {
    expect(applyFailureAction({ kind: 'notFound', message: 'This plan is no longer available.' })).toBe('replan');
    expect(applyFailureAction({ kind: 'session', error: { kind: 'openTransaction', message: 'open' } })).toBe('keep');
    expect(applyFailureAction({ kind: 'session', error: { kind: 'notFound', sessionId: 's1' } })).toBe('close');
    expect(applyFailureAction({ kind: 'internal', message: 'x' })).toBe('close');
  });
});
