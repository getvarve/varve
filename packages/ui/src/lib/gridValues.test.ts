import type { ColumnInfo } from '../api/grid';
import { cellRestoresOriginal, columnAccess, editText, emptyMeansNull, emptyOutcome, isBytesColumn, keyRefusal, parseCellText, parseHex, pgArrayText, tableReadOnlyReason } from './gridValues';

function column(name: string, extra: Partial<ColumnInfo> = {}): ColumnInfo {
  return {
    name,
    dataType: 'text',
    typeSchema: null,
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

describe('editability', () => {
  it('explains read-only tables and columns', () => {
    expect(tableReadOnlyReason({ readOnlySession: false, hasKey: false })).toMatch(/no primary key/);
    expect(tableReadOnlyReason({ readOnlySession: true, hasKey: true })).toMatch(/read-only/);
    expect(tableReadOnlyReason({ readOnlySession: false, hasKey: true })).toBeNull();
    expect(columnAccess(column('d', { arrayOfArrays: true, array: true })).reason).toMatch(/domain over an array/);
    expect(columnAccess(column('g', { generated: 'virtual', editable: false })).update).toBe(false);
    expect(columnAccess(column('i', { identity: 'byDefault' })).update).toBe(true);
    expect(keyRefusal({ columns: ['d'], tableColumns: [column('d', { arrayOfArrays: true })] })).toMatch(/key column d/);
  });
});

describe('typed text', () => {
  it('sends text and lets the server convert it', () => {
    expect(parseCellText('postgres', column('n', { typeName: 'int4' }), '42')).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'text', v: '42' } } });
  });

  it('takes hex for byte columns and numbers for MySQL bits', () => {
    expect(parseHex('\\x0A1b')).toBe('0a1b');
    expect(parseHex('0x0')).toBeNull();
    expect(parseCellText('mysql', column('b', { typeName: 'varbinary' }), '0xFF')).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'bytes', v: 'ff' } } });
    expect(parseCellText('mysql', column('b', { typeName: 'blob' }), 'zz').ok).toBe(false);
    expect(parseCellText('postgres', column('b', { typeName: 'bytea' }), 'zz').ok).toBe(false);
    expect(parseCellText('mysql', column('f', { typeName: 'bit' }), "b'101'")).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'uInt', v: 5 } } });
  });

  it('sends PostgreSQL bytea as bytes whichever way the hex is written', () => {
    const bytes = { ok: true, cell: { kind: 'value', value: { t: 'bytes', v: '0a1b' } } };
    const bytea = column('b', { typeName: 'bytea' });
    expect(parseCellText('postgres', bytea, '0a1b')).toEqual(bytes);
    expect(parseCellText('postgres', bytea, '0x0A1B')).toEqual(bytes);
    expect(parseCellText('postgres', bytea, '\\x0a1b')).toEqual(bytes);
    expect(parseCellText('postgres', bytea, '')).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'bytes', v: '' } } });
    // A domain over bytea is bytes too; an array of bytea is typed as an array literal.
    const domain = column('d', { typeName: 'checksum', baseType: ['pg_catalog', 'bytea'] });
    expect(isBytesColumn('postgres', domain)).toBe(true);
    expect(parseCellText('postgres', domain, '0x0a1b')).toEqual(bytes);
    expect(isBytesColumn('postgres', column('a', { typeName: '_bytea', array: true }))).toBe(false);
  });

  it('makes an emptied cell NULL for types without an empty value', () => {
    const isNull = { ok: true, cell: { kind: 'value', value: { t: 'null' } } };
    expect(parseCellText('postgres', column('n', { typeName: 'int4', dataType: 'integer' }), '')).toEqual(isNull);
    expect(parseCellText('postgres', column('d', { typeName: 'timestamptz' }), '')).toEqual(isNull);
    expect(parseCellText('postgres', column('u', { typeName: 'uuid' }), '')).toEqual(isNull);
    expect(parseCellText('postgres', column('j', { typeName: 'jsonb' }), '')).toEqual(isNull);
    expect(parseCellText('postgres', column('t', { typeName: '_text', array: true }), '')).toEqual(isNull);
    expect(parseCellText('postgres', column('p', { typeName: 'price', baseType: ['pg_catalog', 'numeric'] }), '')).toEqual(isNull);
    expect(parseCellText('mysql', column('n', { typeName: 'int' }), '')).toEqual(isNull);
    expect(parseCellText('mysql', column('d', { typeName: 'datetime' }), '')).toEqual(isNull);
    expect(parseCellText('mysql', column('f', { typeName: 'bit' }), '')).toEqual(isNull);
    // Only an empty cell: typed text still goes to the server as typed.
    expect(parseCellText('postgres', column('n', { typeName: 'int4' }), ' ')).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'text', v: ' ' } } });
  });

  it('puts an emptied cell of a new row back to not set, also for NOT NULL columns', () => {
    const unset = { ok: true, cell: { kind: 'default' } };
    expect(parseCellText('postgres', column('status', { typeName: 'int4', hasDefault: true }), '', true)).toEqual(unset);
    expect(parseCellText('postgres', column('id', { typeName: 'int8', nullable: false, hasDefault: true }), '', true)).toEqual(unset);
    expect(parseCellText('mysql', column('at', { typeName: 'datetime', nullable: false }), '', true)).toEqual(unset);
    expect(parseCellText('postgres', column('t', { typeName: '_text', array: true }), '', true)).toEqual(unset);
    // Types with an empty value keep it, and typed text is a value as in any row.
    expect(parseCellText('postgres', column('t'), '', true)).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'text', v: '' } } });
    expect(parseCellText('mysql', column('b', { typeName: 'blob' }), '', true)).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'bytes', v: '' } } });
    expect(parseCellText('postgres', column('n', { typeName: 'int4' }), '5', true)).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'text', v: '5' } } });
  });

  it('says what an empty value field turns into', () => {
    const int = column('n', { typeName: 'int4' });
    expect(emptyOutcome('postgres', int, false)).toBe('Empty is stored as NULL.');
    expect(emptyOutcome('postgres', { ...int, nullable: false }, false)).toBeNull();
    expect(emptyOutcome('postgres', { ...int, hasDefault: true }, true)).toBe('Empty leaves the column out: the server fills in its default.');
    expect(emptyOutcome('mysql', column('d', { typeName: 'date', nullable: false }), true)).toBe('Empty leaves the column out: it becomes NULL.');
    expect(emptyOutcome('postgres', column('t'), true)).toBeNull();
    expect(emptyOutcome('postgres', column('t'), false)).toBeNull();
  });

  it('refuses an emptied cell of a NOT NULL column without an empty value', () => {
    const parsed = parseCellText('postgres', column('qty', { typeName: 'int4', dataType: 'integer', nullable: false }), '');
    expect(parsed).toEqual({ ok: false, message: 'qty needs a value: it cannot be NULL, and integer has no empty value.' });
    expect(parseCellText('mysql', column('at', { typeName: 'date', dataType: 'date', nullable: false }), '').ok).toBe(false);
  });

  it('keeps the empty text for text, byte, enum and unknown types', () => {
    const empty = { ok: true, cell: { kind: 'value', value: { t: 'text', v: '' } } };
    expect(parseCellText('postgres', column('t'), '')).toEqual(empty);
    expect(parseCellText('postgres', column('v', { typeName: 'varchar', nullable: false }), '')).toEqual(empty);
    expect(parseCellText('postgres', column('m', { typeName: 'mood', typeSchema: 'public' }), '')).toEqual(empty);
    expect(parseCellText('mysql', column('s', { typeName: 'enum' }), '')).toEqual(empty);
    expect(parseCellText('mysql', column('b', { typeName: 'blob' }), '')).toEqual({ ok: true, cell: { kind: 'value', value: { t: 'bytes', v: '' } } });
    expect(emptyMeansNull('postgres', column('t'))).toBe(false);
    expect(emptyMeansNull('mysql', column('c', { typeName: 'char' }))).toBe(false);
  });

  it('writes PostgreSQL arrays that cast back to the same array', () => {
    expect(pgArrayText({ t: 'array', v: [{ t: 'text', v: 'a b' }, { t: 'null' }, { t: 'text', v: 'NULL' }, { t: 'text', v: 'q"x' }] })).toBe('{"a b",NULL,"NULL","q\\"x"}');
    expect(editText('postgres', { t: 'array', v: [{ t: 'int', v: 1 }, { t: 'int', v: 2 }] })).toBe('{1,2}');
    expect(editText('mysql', { t: 'bytes', v: '00ff' })).toBe('\\x00ff');
  });

  it('treats typed text equal to the stored value as no change', () => {
    expect(cellRestoresOriginal('postgres', { kind: 'value', value: { t: 'text', v: '1' } }, { t: 'int', v: 1 })).toBe(true);
    expect(cellRestoresOriginal('postgres', { kind: 'value', value: { t: 'text', v: '' } }, { t: 'null' })).toBe(false);
    expect(cellRestoresOriginal('postgres', { kind: 'default' }, { t: 'int', v: 1 })).toBe(false);
  });
});
