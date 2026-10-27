import type { ColumnInfo } from '../api/grid';
import { cellRestoresOriginal, columnAccess, editText, keyRefusal, parseCellText, parseHex, pgArrayText, tableReadOnlyReason } from './gridValues';

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
