/**
 * Cheap grid cell text for values of any size.
 *
 * AG Grid calls value getters and formatters for every visible cell on
 * every scroll frame. `cellText` in values.ts works on the whole value
 * (a regex over a whole JSON document, the whole text of an array), which
 * costs O(size) per cell per frame. The functions here read only a short
 * prefix of the value and cache the result per value object, so a value
 * is formatted once however often it is drawn. The full value is only
 * built for the record view and the value editor.
 */
import type { Value } from '../api/sessions';
import type { GridDialect } from './gridSql';
import { editText } from './gridValues';
import { cellText, fullText, jsonText, needsViewer, plainText, reindentJson, TEXT_PREVIEW } from './values';

const previewCache = new WeakMap<Value, string>();
const editCache: Record<GridDialect, WeakMap<Value, string>> = { postgres: new WeakMap(), mysql: new WeakMap() };

function cut(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/** One-line JSON of a prefix that is long enough for a `max` character preview. */
function jsonPreview(text: string, max: number): string {
  let scan = max * 2 + 16;
  for (;;) {
    const part = text.slice(0, scan);
    const line = part.replace(/\s*[\r\n]+\s*/g, ' ').trim();
    if (line.length > max || part.length === text.length) return cut(line, max);
    scan *= 2;
  }
}

/** `{a,b,NULL}` of an array, built only until it is longer than `max`. */
function arrayPreview(value: Extract<Value, { t: 'array' }>, max: number): string {
  let out = '{';
  for (let i = 0; i < value.v.length; i += 1) {
    const item = value.v[i] as Value;
    const text = item.t === 'array' ? arrayPreview(item, max) : (plainText(item) ?? 'NULL');
    out += (i > 0 ? ',' : '') + text.slice(0, max + 1);
    if (out.length > max) return cut(out, max);
  }
  return cut(`${out}}`, max);
}

/**
 * The single-line preview of a value for a grid cell: the same text as
 * `cellText`, computed from a bounded prefix and cached per value.
 */
export function previewText(value: Value): string {
  const hit = previewCache.get(value);
  if (hit !== undefined) return hit;
  let text: string;
  switch (value.t) {
    case 'json':
      text = jsonPreview(value.v, TEXT_PREVIEW);
      break;
    case 'array':
      text = arrayPreview(value, TEXT_PREVIEW);
      break;
    case 'text':
    case 'other':
      text = cut(value.v.slice(0, TEXT_PREVIEW + 2).replace(/\r?\n/g, ' ⏎ '), TEXT_PREVIEW);
      break;
    default:
      text = cellText(value);
  }
  previewCache.set(value, text);
  return text;
}

/** The text a cell editor starts from (`editText`), computed once per value. */
export function cachedEditText(dialect: GridDialect, value: Value): string {
  const cache = editCache[dialect];
  const hit = cache.get(value);
  if (hit !== undefined) return hit;
  const text = editText(dialect, value);
  cache.set(value, text);
  return text;
}

const dialogCache = new WeakMap<Value, boolean>();
const tooltipCache = new WeakMap<Value, string>();

/** Characters shown in a cell tooltip. */
export const TOOLTIP_CHARS = 4000;
/** JSON up to this length is re-indented for the tooltip; longer JSON shows its raw start. */
const TOOLTIP_REINDENT_LIMIT = 20_000;

/** `needsViewer`, computed once per value: the value is edited in the value editor, not inline. */
export function opensInDialog(value: Value): boolean {
  const hit = dialogCache.get(value);
  if (hit !== undefined) return hit;
  const out = needsViewer(value);
  dialogCache.set(value, out);
  return out;
}

/** Cuts `text` to `max` characters, with an ellipsis only when something was cut. */
function capped(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/**
 * The start of an array as JSON (the form `jsonText` writes), built only
 * until it is longer than `max`, so a huge array costs no more than its
 * first `max` characters.
 */
function arrayJsonPrefix(value: Extract<Value, { t: 'array' }>, max: number): string {
  let out = '[';
  for (let i = 0; i < value.v.length; i += 1) {
    const item = value.v[i] as Value;
    let piece: string;
    if (item.t === 'array') piece = arrayJsonPrefix(item, max);
    else if (item.t === 'text' || item.t === 'other') piece = JSON.stringify(item.v.slice(0, max + 1));
    else piece = jsonText(item).slice(0, max + 1);
    out += (i > 0 ? ',' : '') + piece;
    if (out.length > max) return out;
  }
  return `${out}]`;
}

/**
 * Tooltip text of a large value, at most `TOOLTIP_CHARS` characters plus
 * an ellipsis when it was cut, built once per value. JSON and arrays are
 * re-indented (arrays as a JSON list, as the value panel shows them).
 */
export function tooltipText(value: Value): string {
  const hit = tooltipCache.get(value);
  if (hit !== undefined) return hit;
  let text: string;
  if (value.t === 'json' && value.v.length > TOOLTIP_REINDENT_LIMIT) {
    text = capped(value.v, TOOLTIP_CHARS);
  } else if (value.t === 'array') {
    const prefix = arrayJsonPrefix(value, TOOLTIP_CHARS);
    text = capped(reindentJson(prefix), TOOLTIP_CHARS);
    // A prefix that stopped early was cut even when its re-indented text fits.
    if (!prefix.endsWith(']') || prefix.length > TOOLTIP_CHARS) text = text.endsWith('…') ? text : `${text}…`;
  } else {
    text = capped(fullText(value), TOOLTIP_CHARS);
  }
  tooltipCache.set(value, text);
  return text;
}
