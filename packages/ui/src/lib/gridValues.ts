/**
 * Cell values of the table data view: which columns can be written and
 * why not, turning typed text into the `CellValue` the backend plans with,
 * and the text a cell editor starts from.
 */
import type { CellValue, ColumnInfo } from '../api/grid';
import type { Value } from '../api/sessions';
import { needsViewer, plainText } from './values';
import type { GridDialect } from './gridSql';

/** Why the whole table is read only in the grid, or `null` when it can be edited. */
export function tableReadOnlyReason(opts: { readOnlySession: boolean; hasKey: boolean; keyRefused?: string | null }): string | null {
  if (opts.readOnlySession) return 'This connection is read-only, so rows cannot be changed here.';
  if (opts.keyRefused) return opts.keyRefused;
  if (!opts.hasKey) {
    return 'This table has no primary key and no unique key over NOT NULL columns, so Varve cannot tell its rows apart. Add a primary key to edit rows here.';
  }
  return null;
}

/** What may be written to a column. */
export interface ColumnAccess {
  /** A value can be written on update. */
  update: boolean;
  /** A value can be given on insert (otherwise only DEFAULT). */
  insert: boolean;
  /** Shown as the column's tooltip and in the record view when it is not fully writable. */
  reason: string | null;
}

/** Write access to a column, with the reason in words when it is limited. */
export function columnAccess(column: ColumnInfo): ColumnAccess {
  if (column.arrayOfArrays) {
    return {
      update: false,
      insert: false,
      reason:
        'This column is an array of a domain over an array. Its values arrive as nested arrays that cannot be told apart from dimensions, so Varve cannot write them back. Change it in SQL.',
    };
  }
  if (column.generated) {
    return {
      update: false,
      insert: false,
      reason: `Generated column (${column.generated}): the server computes it from other columns.`,
    };
  }
  if (column.identity === 'always') {
    return { update: false, insert: false, reason: 'GENERATED ALWAYS AS IDENTITY: the server assigns it.' };
  }
  if (!column.editable) return { update: false, insert: false, reason: 'The server does not let this column be written.' };
  return { update: true, insert: true, reason: null };
}

/** True when the key of the table uses a column whose values cannot be compared (the grid then stays read only). */
export function keyRefusal(key: { columns: string[]; tableColumns: ColumnInfo[] }): string | null {
  const bad = key.columns.find((name) => key.tableColumns.find((c) => c.name === name)?.arrayOfArrays);
  return bad
    ? `The key column ${bad} is an array of a domain over an array, which Varve cannot compare, so rows cannot be told apart here.`
    : null;
}

const MYSQL_BYTES = new Set(['binary', 'varbinary', 'tinyblob', 'blob', 'mediumblob', 'longblob']);

/** True for MySQL / MariaDB columns that hold bytes; their values are typed as hex. */
export function isBytesColumn(dialect: GridDialect, column: ColumnInfo): boolean {
  if (dialect === 'postgres') return column.typeName === 'bytea';
  return MYSQL_BYTES.has(column.typeName);
}

/** Parses hex written as `\x0a1b`, `0x0A1B` or `0a1b`; `null` when it is not even-length hex. */
export function parseHex(text: string): string | null {
  const body = text.trim().replace(/^(\\x|0x)/i, '');
  return /^([0-9a-fA-F]{2})*$/.test(body) ? body.toLowerCase() : null;
}

/** The outcome of reading typed text for a column. */
export type ParsedCell = { ok: true; cell: CellValue } | { ok: false; message: string };

/**
 * Turns the text typed into a cell into a `CellValue`. Text is sent as a
 * text value and the server converts it to the column's type (PostgreSQL
 * casts the literal, MySQL converts it), so the server decides what is a
 * valid number or date. MySQL byte columns take hex, `bit` columns a number.
 */
export function parseCellText(dialect: GridDialect, column: ColumnInfo, text: string): ParsedCell {
  if (dialect === 'mysql' && MYSQL_BYTES.has(column.typeName)) {
    const hex = parseHex(text);
    return hex === null
      ? { ok: false, message: `${column.name} holds bytes: type them as hex, for example \\x0a1b.` }
      : { ok: true, cell: { kind: 'value', value: { t: 'bytes', v: hex } } };
  }
  if (dialect === 'mysql' && column.typeName === 'bit') {
    const t = text.trim();
    if (/^\d+$/.test(t)) return { ok: true, cell: { kind: 'value', value: { t: 'uInt', v: Number.isSafeInteger(Number(t)) ? Number(t) : t } } };
    const bits = /^b'([01]+)'$/i.exec(t);
    if (bits?.[1]) {
      const n = BigInt(`0b${bits[1]}`);
      return { ok: true, cell: { kind: 'value', value: { t: 'uInt', v: n <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(n) : n.toString() } } };
    }
    return { ok: false, message: `${column.name} is a bit column: type a number or b'0101'.` };
  }
  if (dialect === 'postgres' && column.typeName === 'bytea' && parseHex(text) === null) {
    return { ok: false, message: `${column.name} holds bytes: type them as hex, for example \\x0a1b.` };
  }
  return { ok: true, cell: { kind: 'value', value: { t: 'text', v: text } } };
}

function pgArrayElement(value: Value): string {
  if (value.t === 'null') return 'NULL';
  if (value.t === 'array') return pgArrayText(value);
  const text = plainText(value) ?? '';
  if (text !== '' && !/[{}",\\\s]/.test(text) && text.toUpperCase() !== 'NULL') return text;
  return `"${text.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
}

/** A PostgreSQL array literal (`{1,"a b",NULL}`) that casts back to the same array. */
export function pgArrayText(value: Extract<Value, { t: 'array' }>): string {
  return `{${value.v.map(pgArrayElement).join(',')}}`;
}

/** The text a cell editor starts from for a stored value. */
export function editText(dialect: GridDialect, value: Value): string {
  if (value.t === 'null') return '';
  if (value.t === 'array' && dialect === 'postgres') return pgArrayText(value);
  if (value.t === 'bytes') return `\\x${value.v}`;
  return plainText(value) ?? '';
}

/** The text shown for a pending cell in the grid. */
export function pendingCellText(cell: CellValue, column: ColumnInfo): string {
  switch (cell.kind) {
    case 'default':
      return column.hasDefault ? 'DEFAULT' : 'DEFAULT (NULL)';
    case 'expression':
      return `= ${cell.value}`;
    case 'value':
      if (cell.value.t === 'null') return 'NULL';
      return plainText(cell.value) ?? '';
  }
}

/** True when a value is better edited in the large value editor than inline in the cell. */
export function editsInDialog(value: Value | null): boolean {
  return value !== null && needsViewer(value);
}

/** Exact equality of two wire values (same tag and same text), used to drop edits that restore the original. */
export function sameValue(a: Value, b: Value): boolean {
  if (a.t !== b.t) return false;
  if (a.t === 'null') return true;
  if (a.t === 'array' && b.t === 'array') return a.v.length === b.v.length && a.v.every((x, i) => sameValue(x, b.v[i] as Value));
  return String((a as { v: unknown }).v) === String((b as { v: unknown }).v);
}

/** True when a pending cell writes exactly the value the row already holds. */
export function cellRestoresOriginal(dialect: GridDialect, cell: CellValue, original: Value): boolean {
  if (cell.kind !== 'value') return false;
  if (sameValue(cell.value, original)) return true;
  // Typed text equal to the original's editor text is no change either.
  return cell.value.t === 'text' && original.t !== 'null' && cell.value.v === editText(dialect, original);
}
