import type { Column, Value } from '../api/sessions';
import { isNumeric, isPlaceholder, jsonText, plainText } from './values';

/** Characters that make a spreadsheet read a cell as a formula. */
const FORMULA_START = /^[=+\-@\t\r]/;

/**
 * Stops spreadsheet formula injection: text that starts with `=`, `+`, `-`,
 * `@` (or a tab or carriage return) gets a leading apostrophe. Typed numbers
 * are left alone, because `-5` from a numeric column is a number, not a formula.
 */
export function neutralizeFormula(text: string): string {
  return FORMULA_START.test(text) ? `'${text}` : text;
}

/**
 * Plain text for an export: like `plainText`, but a placeholder for a value
 * too large to read (also inside an array) becomes NULL instead of its text.
 */
export function exportText(value: Value): string | null {
  if (isPlaceholder(value)) return null;
  if (value.t === 'array') return `{${value.v.map((item) => exportText(item) ?? 'NULL').join(',')}}`;
  return plainText(value);
}

/** Quotes a CSV field when it holds a separator, a quote, a line break or edge spaces (RFC 4180). */
export function quoteCsv(text: string): string {
  if (/[",\r\n]/.test(text) || text !== text.trim()) return `"${text.replace(/"/g, '""')}"`;
  return text;
}

/** One CSV cell: NULL becomes an empty field. */
export function csvCell(value: Value): string {
  // A placeholder for an entry too large to show is not data; it must never be re-imported as a value.
  const text = exportText(value);
  if (text === null) return '';
  return quoteCsv(isNumeric(value) ? text : neutralizeFormula(text));
}

/** CSV with a header row, CRLF line ends, as RFC 4180 describes. */
export function toCsv(columns: readonly Column[], rows: readonly (readonly Value[])[]): string {
  const lines = [columns.map((c) => quoteCsv(neutralizeFormula(c.name))).join(',')];
  for (const row of rows) lines.push(row.map(csvCell).join(','));
  return `${lines.join('\r\n')}\r\n`;
}

/** Unique keys for the JSON objects: a repeated column name gets `_2`, `_3`. */
export function uniqueColumnKeys(columns: readonly Column[]): string[] {
  const used = new Set<string>();
  return columns.map((c) => {
    let key = c.name;
    let n = 2;
    while (used.has(key)) {
      key = `${c.name}_${n}`;
      n += 1;
    }
    used.add(key);
    return key;
  });
}

/**
 * A JSON array with one object per row. Built as text so exact values (big
 * integers, JSON documents as sent) are written without a round trip through
 * JavaScript numbers.
 */
export function toJson(columns: readonly Column[], rows: readonly (readonly Value[])[]): string {
  const keys = uniqueColumnKeys(columns).map((k) => JSON.stringify(k));
  if (rows.length === 0) return '[]\n';
  const objects = rows.map((row) => {
    const fields = keys.map((key, i) => {
      const value = row[i];
      return `    ${key}: ${value === undefined || isPlaceholder(value) ? 'null' : jsonText(value)}`;
    });
    return fields.length === 0 ? '  {}' : `  {\n${fields.join(',\n')}\n  }`;
  });
  return `[\n${objects.join(',\n')}\n]\n`;
}

/** Tab separated text of rows, for copying to the clipboard. */
export function toTsv(rows: readonly (readonly Value[])[]): string {
  // Placeholders for entries too large to show are not data, so they copy as empty fields.
  return rows
    .map((row) => row.map((v) => (exportText(v) ?? '').replace(/[\t\r\n]+/g, ' ')).join('\t'))
    .join('\n');
}

/** Cells left out of an export because they were placeholders for values too large to load. */
export function countPlaceholders(rows: readonly (readonly Value[])[]): number {
  let n = 0;
  const visit = (value: Value): void => {
    if (isPlaceholder(value)) n += 1;
    else if (value.t === 'array') value.v.forEach(visit);
  };
  for (const row of rows) row.forEach(visit);
  return n;
}

/** The warning shown after an export that left out placeholder cells, or `null` when none were left out. */
export function placeholderNotice(count: number, format: 'csv' | 'json'): { title: string; message: string } | null {
  if (count === 0) return null;
  const cells = count === 1 ? '1 cell was' : `${count.toLocaleString('en-US')} cells were`;
  return {
    title: `${cells} left out of the export`,
    message: `${count === 1 ? 'It was' : 'They were'} too large to load, so the file has ${
      format === 'csv' ? 'an empty field' : 'null'
    } in ${count === 1 ? 'its place' : 'their place'}. Read such values in the console.`,
  };
}
