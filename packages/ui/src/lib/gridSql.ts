/**
 * SQL the table data view builds itself: the SELECT of one page and its
 * COUNT. Names and values are quoted here exactly as the backend quotes
 * them (crates/catalog/src/sql.rs `ident`, `string`); raw SQL the user
 * types (a WHERE clause in raw mode, an expression cell) is checked with
 * the same rules as `check_expression` in crates/catalog/src/data_edit/expr.rs
 * so it can never reach outside its parentheses.
 */
import type { TableTarget } from '../api/grid';

/** SQL dialect of an engine. */
export type GridDialect = 'postgres' | 'mysql';

/** The dialect of an engine name from `SessionInfo.engine`. */
export function dialectOf(engine: string): GridDialect {
  return engine === 'postgres' ? 'postgres' : 'mysql';
}

/** Quotes one identifier, doubling the quote character. Rejects empty names and NUL. */
export function quoteIdent(dialect: GridDialect, name: string): string {
  if (name === '' || name.includes('\0')) throw new Error('A name cannot be empty or contain a NUL character.');
  const q = dialect === 'postgres' ? '"' : '`';
  return `${q}${name.split(q).join(q + q)}${q}`;
}

/** Hex of the UTF-8 bytes of `text`, upper case. */
function utf8Hex(text: string): string {
  return [...new TextEncoder().encode(text)].map((b) => b.toString(16).padStart(2, '0').toUpperCase()).join('');
}

/**
 * A string literal. PostgreSQL: `'...'` with `'` doubled (Varve sessions
 * run with standard conforming strings). MySQL / MariaDB: `'...'` with `'`
 * doubled, or `_utf8mb4 X'..'` when the text holds a backslash or NUL, so
 * the value is the same whatever `NO_BACKSLASH_ESCAPES` says.
 */
export function quoteString(dialect: GridDialect, text: string): string {
  if (dialect === 'postgres') {
    if (text.includes('\0')) throw new Error('PostgreSQL text cannot contain a NUL character.');
    return `'${text.split("'").join("''")}'`;
  }
  if (text.includes('\\') || text.includes('\0')) return `_utf8mb4 X'${utf8Hex(text)}'`;
  return `'${text.split("'").join("''")}'`;
}

/** The table name, qualified with its schema (PostgreSQL) or database (MySQL / MariaDB). */
export function qualifiedTable(dialect: GridDialect, target: TableTarget): string {
  const first = dialect === 'postgres' ? target.schema : target.database;
  if (dialect === 'postgres' && !first) throw new Error('A PostgreSQL table needs its schema.');
  return first ? `${quoteIdent(dialect, first)}.${quoteIdent(dialect, target.table)}` : quoteIdent(dialect, target.table);
}

function isWordChar(c: string | undefined): boolean {
  return c !== undefined && (/[\p{L}\p{N}_$]/u.test(c));
}

function closingQuote(chars: string[], start: number, quote: string, escapes: boolean): number | null {
  let i = start + 1;
  while (i < chars.length) {
    const c = chars[i];
    if (escapes && c === '\\') {
      i += 2;
      continue;
    }
    if (c === quote) {
      if (chars[i + 1] === quote) {
        i += 2;
        continue;
      }
      return i + 1;
    }
    i += 1;
  }
  return null;
}

function dollarTag(chars: string[], start: number): number | null {
  if (isWordChar(chars[start - 1])) return null;
  let i = start + 1;
  const first = chars[i];
  if (first === '$') return i;
  if (first === undefined || !/[\p{L}_]/u.test(first)) return null;
  while (i < chars.length) {
    const c = chars[i] as string;
    if (c === '$') return i;
    if (!/[\p{L}\p{N}_]/u.test(c)) return null;
    i += 1;
  }
  return null;
}

function findFrom(chars: string[], from: number, needle: string[]): number | null {
  for (let i = from; i + needle.length <= chars.length; i += 1) {
    if (needle.every((c, j) => chars[i + j] === c)) return i;
  }
  return null;
}

function hasBackslashInString(chars: string[]): boolean {
  let i = 0;
  while (i < chars.length) {
    const c = chars[i] as string;
    if (c === "'" || c === '"' || c === '`') {
      const end = closingQuote(chars, i, c, false) ?? chars.length;
      if (c !== '`' && chars.slice(i, end).includes('\\')) return true;
      i = end;
    } else {
      i += 1;
    }
  }
  return false;
}

/**
 * Checks that `text` is one self-contained SQL expression: quotes and
 * dollar quotes closed, parentheses balanced, no comment and no `;`
 * outside literals; on MySQL no backslash inside a string. Returns the
 * problem in words, or `null` when it is fine. Mirrors the backend check.
 */
export function checkExpression(dialect: GridDialect, text: string): string | null {
  const fail = (what: string): string => `The SQL expression ${what}.`;
  if (text.trim() === '') return fail('is empty');
  if (text.includes('\0')) return fail('contains a NUL character');
  const chars = [...text];
  if (dialect === 'mysql' && hasBackslashInString(chars)) {
    return fail(
      "has a backslash inside a string, which MySQL reads differently depending on the NO_BACKSLASH_ESCAPES SQL mode; use a hex literal (X'...') instead",
    );
  }
  let depth = 0;
  let i = 0;
  while (i < chars.length) {
    const c = chars[i] as string;
    const next = chars[i + 1];
    if (c === "'") {
      const prefix = chars[i - 1];
      const escapes = dialect === 'postgres' && (prefix === 'e' || prefix === 'E') && !isWordChar(chars[i - 2]);
      const end = closingQuote(chars, i, "'", escapes);
      if (end === null) return fail('has a string that is not closed');
      i = end;
      continue;
    }
    if (c === '"') {
      const end = closingQuote(chars, i, '"', false);
      if (end === null) return fail('has a quoted name or string that is not closed');
      i = end;
      continue;
    }
    if (c === '`' && dialect === 'mysql') {
      const end = closingQuote(chars, i, '`', false);
      if (end === null) return fail('has a quoted name that is not closed');
      i = end;
      continue;
    }
    if (c === '$' && dialect === 'postgres') {
      const tagEnd = dollarTag(chars, i);
      if (tagEnd !== null) {
        const tag = chars.slice(i, tagEnd + 1);
        const close = findFrom(chars, tagEnd + 1, tag);
        if (close === null) return fail('has a dollar-quoted string that is not closed');
        i = close + tag.length;
        continue;
      }
    }
    if ((c === '-' && next === '-') || (c === '/' && next === '*') || (c === '#' && dialect === 'mysql')) {
      return fail('cannot contain a comment');
    }
    if (c === ';') return fail("cannot contain ';'");
    if (c === '(') depth += 1;
    if (c === ')') {
      if (depth === 0) return fail('closes a parenthesis it did not open');
      depth -= 1;
    }
    i += 1;
  }
  if (depth !== 0) return fail('has a parenthesis that is not closed');
  return null;
}

/** Comparison operators of the filter builder. */
export type FilterOperator = '=' | '<>' | '<' | '<=' | '>' | '>=' | 'like' | 'notLike' | 'isNull' | 'isNotNull';

/** Operators with their labels, in menu order. */
export const FILTER_OPERATORS: readonly { op: FilterOperator; label: string; needsValue: boolean }[] = [
  { op: '=', label: '=', needsValue: true },
  { op: '<>', label: '<>', needsValue: true },
  { op: '<', label: '<', needsValue: true },
  { op: '<=', label: '<=', needsValue: true },
  { op: '>', label: '>', needsValue: true },
  { op: '>=', label: '>=', needsValue: true },
  { op: 'like', label: 'LIKE', needsValue: true },
  { op: 'notLike', label: 'NOT LIKE', needsValue: true },
  { op: 'isNull', label: 'IS NULL', needsValue: false },
  { op: 'isNotNull', label: 'IS NOT NULL', needsValue: false },
];

/** True when the operator compares with a value. */
export function operatorNeedsValue(op: FilterOperator): boolean {
  return FILTER_OPERATORS.find((o) => o.op === op)?.needsValue ?? true;
}

/** One condition of the filter builder. */
export interface FilterCondition {
  column: string;
  op: FilterOperator;
  /** The value as typed; the server converts it to the column's type. */
  value: string;
}

/** The filter of the data view: builder conditions joined with AND, or a raw WHERE clause. */
export type GridFilter = { mode: 'builder'; conditions: FilterCondition[] } | { mode: 'raw'; sql: string };

/** A column to sort by. */
export interface SortKey {
  column: string;
  descending: boolean;
}

/** The SQL text of one builder condition. */
export function conditionSql(dialect: GridDialect, condition: FilterCondition): string {
  const col = quoteIdent(dialect, condition.column);
  switch (condition.op) {
    case 'isNull':
      return `${col} IS NULL`;
    case 'isNotNull':
      return `${col} IS NOT NULL`;
    case 'like':
    case 'notLike': {
      // PostgreSQL LIKE needs text: cast so numbers and dates can be matched by pattern too.
      const subject = dialect === 'postgres' ? `CAST(${col} AS text)` : col;
      return `${subject} ${condition.op === 'like' ? 'LIKE' : 'NOT LIKE'} ${quoteString(dialect, condition.value)}`;
    }
    default:
      return `${col} ${condition.op} ${quoteString(dialect, condition.value)}`;
  }
}

/**
 * The WHERE clause (without the keyword) of a filter, or `null` for none.
 * Throws with the reason when a raw clause is not one safe expression.
 */
export function whereSql(dialect: GridDialect, filter: GridFilter): string | null {
  if (filter.mode === 'raw') {
    if (filter.sql.trim() === '') return null;
    const problem = checkExpression(dialect, filter.sql);
    if (problem) throw new Error(problem);
    return `(${filter.sql})`;
  }
  if (filter.conditions.length === 0) return null;
  return filter.conditions.map((c) => conditionSql(dialect, c)).join(' AND ');
}

/** What the data view reads. */
export interface PageQuery {
  dialect: GridDialect;
  target: TableTarget;
  /** Columns to read, in order; empty reads every column (`*`), for a view whose columns are not known yet. */
  columns: string[];
  filter: GridFilter;
  sort: SortKey[];
  /**
   * Columns appended to the ORDER BY so pages are stable (the key), unless
   * already sorted on.
   */
  tiebreak: string[];
  limit: number;
  offset: number;
}

/** `SELECT <columns> FROM <table> [WHERE] [ORDER BY] LIMIT n OFFSET m`. */
export function selectPageSql(q: PageQuery): string {
  if (!Number.isInteger(q.limit) || q.limit < 1 || !Number.isInteger(q.offset) || q.offset < 0) {
    throw new Error('The page size and offset must be whole numbers.');
  }
  const cols = q.columns.length === 0 ? '*' : q.columns.map((c) => quoteIdent(q.dialect, c)).join(', ');
  let sql = `SELECT ${cols} FROM ${qualifiedTable(q.dialect, q.target)}`;
  const where = whereSql(q.dialect, q.filter);
  if (where) sql += ` WHERE ${where}`;
  const order = [...q.sort];
  for (const column of q.tiebreak) {
    if (!order.some((s) => s.column === column)) order.push({ column, descending: false });
  }
  if (order.length > 0) {
    sql += ` ORDER BY ${order.map((s) => `${quoteIdent(q.dialect, s.column)}${s.descending ? ' DESC' : ' ASC'}`).join(', ')}`;
  }
  return `${sql} LIMIT ${q.limit} OFFSET ${q.offset}`;
}

/** `SELECT COUNT(*) FROM <table> [WHERE]`, for "Count exactly". */
export function countSql(dialect: GridDialect, target: TableTarget, filter: GridFilter): string {
  const where = whereSql(dialect, filter);
  return `SELECT COUNT(*) FROM ${qualifiedTable(dialect, target)}${where ? ` WHERE ${where}` : ''}`;
}
