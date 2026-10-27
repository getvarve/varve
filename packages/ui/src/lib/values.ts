import type { Value } from '../api/sessions';

/** Bytes shown in a grid cell before the preview is cut. */
export const BYTES_PREVIEW = 32;
/** Characters shown in a grid cell for JSON, arrays and long text. */
export const TEXT_PREVIEW = 200;

const NUMERIC: readonly Value['t'][] = ['int', 'uInt', 'float', 'decimal'];

function isInteger(value: Value): value is Extract<Value, { t: 'int' | 'uInt' }> {
  return value.t === 'int' || value.t === 'uInt';
}

/** Marker text the backend puts in place of entries too large to send (crates/driver-redis oversized.rs). */
export const OVERSIZED_MARK = '(too large to show';

/** True for the placeholder of an entry too large to show. It is not data: never copy or edit it. */
export function isOversized(value: Value): boolean {
  return value.t === 'other' && value.v.startsWith(OVERSIZED_MARK);
}

/**
 * True for a placeholder the backend sends instead of a value that was too
 * large to read (for set members it reads "(too large to show: N bytes at
 * the time of the scan)"). Placeholders are not data: they are shown muted
 * and never copied, edited or exported as values.
 */
export function isPlaceholder(value: Value): boolean {
  return isOversized(value);
}

/** A short explanation of a placeholder, for tooltips. */
export function placeholderHelp(_value: Value): string {
  return 'Too large to show here. Read it in pieces in the console.';
}

export function isNumeric(value: Value): boolean {
  return NUMERIC.includes(value.t);
}

/** True for values whose full text does not fit a cell: JSON, arrays, bytes, long or multi-line text. */
export function needsViewer(value: Value): boolean {
  if (isPlaceholder(value)) return false;
  switch (value.t) {
    case 'json':
    case 'array':
      return true;
    case 'bytes':
      return value.v.length / 2 > BYTES_PREVIEW;
    case 'text':
    case 'other':
      return value.v.length > TEXT_PREVIEW || /[\r\n]/.test(value.v);
    default:
      return false;
  }
}

/**
 * Plain text of a value, the same as `Value::to_plain_text` in the backend:
 * `null` for NULL, `\x` hex for bytes, the JSON text as sent, `{a,b,NULL}`
 * for arrays. Integers beyond 2^53 arrive as exact strings and are passed
 * through unchanged; floats are written like Rust's `f64` Display.
 */
export function plainText(value: Value): string | null {
  switch (value.t) {
    case 'null':
      return null;
    case 'bool':
      return value.v ? 'true' : 'false';
    case 'int':
    case 'uInt':
      return String(value.v);
    case 'float':
      return rustFloat(value.v);
    case 'bytes':
      return `\\x${value.v}`;
    case 'array':
      return `{${value.v.map((item) => plainText(item) ?? 'NULL').join(',')}}`;
    default:
      return value.v;
  }
}

/**
 * A float written the way Rust's `f64` Display writes it (what the backend's
 * exports use): the shortest digits that round-trip, never an exponent,
 * `-0` kept, and `inf`, `-inf`, `NaN` for the non-finite values the wire
 * carries as `"Infinity"`, `"-Infinity"` and `"NaN"`.
 */
export function rustFloat(v: number | 'NaN' | 'Infinity' | '-Infinity'): string {
  if (v === 'NaN' || (typeof v === 'number' && Number.isNaN(v))) return 'NaN';
  if (v === 'Infinity' || v === Number.POSITIVE_INFINITY) return 'inf';
  if (v === '-Infinity' || v === Number.NEGATIVE_INFINITY) return '-inf';
  if (Object.is(v, -0)) return '-0';
  const text = String(v);
  const match = /^(-?)(\d)(?:\.(\d+))?e([+-]\d+)$/.exec(text);
  if (!match) return text;
  const [, sign, lead, rest = '', exp] = match;
  const digits = `${lead}${rest}`;
  const point = 1 + Number(exp);
  if (point <= 0) return `${sign}0.${'0'.repeat(-point)}${digits}`;
  if (point >= digits.length) return `${sign}${digits}${'0'.repeat(point - digits.length)}`;
  return `${sign}${digits.slice(0, point)}.${digits.slice(point)}`;
}

function cut(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/** JSON text on one line, without parsing it (JSON strings cannot hold raw line breaks). */
function oneLineJson(text: string): string {
  return text.replace(/\s*[\r\n]+\s*/g, ' ').trim();
}

/** Short single-line text for a grid cell. */
export function cellText(value: Value): string {
  switch (value.t) {
    case 'null':
      return 'NULL';
    case 'bytes': {
      const bytes = value.v.length / 2;
      if (bytes <= BYTES_PREVIEW) return `\\x${value.v}`;
      return `\\x${value.v.slice(0, BYTES_PREVIEW * 2)}… (${bytes.toLocaleString('en-US')} bytes)`;
    }
    case 'json':
      return cut(oneLineJson(value.v), TEXT_PREVIEW);
    case 'array':
      return cut(plainText(value) ?? '', TEXT_PREVIEW);
    case 'text':
    case 'other':
      return cut(value.v.replace(/\r?\n/g, ' ⏎ '), TEXT_PREVIEW);
    default:
      return plainText(value) ?? 'NULL';
  }
}

/** Full text for a tooltip or the value panel: JSON re-indented as text (numbers kept exact), everything else in full. */
export function fullText(value: Value): string {
  if (value.t === 'json') return reindentJson(value.v);
  if (value.t === 'array') return reindentJson(jsonText(value));
  return plainText(value) ?? 'NULL';
}

/**
 * The value as JSON text for exports. Numbers stay numbers, including exact
 * big integers (written as their digits); non-finite floats, exact decimals
 * and bytes become strings; JSON documents are embedded as sent.
 */
export function jsonText(value: Value): string {
  if (isPlaceholder(value)) return 'null';
  switch (value.t) {
    case 'null':
      return 'null';
    case 'bool':
      return value.v ? 'true' : 'false';
    case 'int':
    case 'uInt':
      return String(value.v);
    case 'float':
      return typeof value.v === 'number' ? String(value.v) : JSON.stringify(value.v);
    case 'json':
      return value.v.trim();
    case 'array':
      return `[${value.v.map(jsonText).join(',')}]`;
    case 'bytes':
      return JSON.stringify(`\\x${value.v}`);
    default:
      return JSON.stringify(value.v);
  }
}

/** CSS class of a cell by type, for NULL styling and number alignment. */
export function cellClass(value: Value): string {
  if (value.t === 'null') return 'v-null';
  if (isPlaceholder(value)) return 'v-placeholder';
  if (isNumeric(value)) return 'v-num';
  if (value.t === 'bool') return 'v-bool';
  if (value.t === 'bytes') return 'v-bytes';
  if (value.t === 'json' || value.t === 'array') return 'v-json';
  return 'v-text';
}

/** Sort order for grid columns: NULL first, numbers numerically, everything else by text. */
export function compareValues(a: Value, b: Value): number {
  if (a.t === 'null' || b.t === 'null') return (a.t === 'null' ? 0 : 1) - (b.t === 'null' ? 0 : 1);
  if (isInteger(a) && isInteger(b)) {
    const x = BigInt(String(a.v));
    const y = BigInt(String(b.v));
    if (x !== y) return x < y ? -1 : 1;
  } else if (isNumeric(a) && isNumeric(b)) {
    const x = Number(plainText(a));
    const y = Number(plainText(b));
    if (!Number.isNaN(x) && !Number.isNaN(y) && x !== y) return x < y ? -1 : 1;
  }
  if (a.t === 'bool' && b.t === 'bool') return Number(a.v) - Number(b.v);
  return (plainText(a) ?? '').localeCompare(plainText(b) ?? '');
}

/** Converts a 1-based character position into a UTF-16 offset in `text` (what the editor uses). */
export function charPositionToOffset(text: string, position: number): number {
  let offset = 0;
  let chars = 1;
  for (const ch of text) {
    if (chars >= position) return offset;
    offset += ch.length;
    chars += 1;
  }
  return text.length;
}

/** Counts the characters (code points) of a string. */
export function charCount(text: string): number {
  return [...text].length;
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.round((ms % 60_000) / 1000);
  return `${minutes} min ${seconds} s`;
}

export function formatBytes(input: number | string): string {
  const bytes = Number(input);
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit] ?? 'TB'}`;
}

/** Whole seconds of a TTL in milliseconds (decimal text from the backend), or `null` without expiry. */
export function ttlSeconds(ttlMs: string | null): bigint | null {
  if (ttlMs === null || !/^-?\d+$/.test(ttlMs)) return null;
  const ms = BigInt(ttlMs);
  return ms < 0n ? null : ms / 1000n;
}

/** Redis TTL in a short human form; `null` means the key does not expire. */
export function formatTtl(ttlMs: string | null): string {
  const total = ttlSeconds(ttlMs);
  if (total === null) return 'No expiry';
  if (total < 60n) return `${total} s`;
  const minutes = total / 60n;
  if (minutes < 60n) return `${minutes}m ${total % 60n}s`;
  const hours = minutes / 60n;
  if (hours < 48n) return `${hours}h ${minutes % 60n}m`;
  return `${hours / 24n}d ${hours % 24n}h`;
}

/** A count with thousands separators; decimal strings (u64 from the backend) are formatted exactly. */
export function formatCount(n: number | string): string {
  if (typeof n === 'string') return /^-?\d+$/.test(n) ? BigInt(n).toLocaleString('en-US') : n;
  return n.toLocaleString('en-US');
}

/**
 * Re-indents JSON text without parsing it into numbers, so big integers and
 * long decimals keep every digit and keys keep their order. Assumes `text`
 * is valid JSON.
 */
export function reindentJson(text: string, indent = '  '): string {
  let out = '';
  let depth = 0;
  let inString = false;
  let escaped = false;
  const newline = (): string => `\n${indent.repeat(depth)}`;
  for (let i = 0; i < text.length; i += 1) {
    const ch = text.charAt(i);
    if (inString) {
      out += ch;
      if (escaped) escaped = false;
      else if (ch === '\\') escaped = true;
      else if (ch === '"') inString = false;
      continue;
    }
    if (/\s/.test(ch)) continue;
    if (ch === '"') {
      inString = true;
      out += ch;
    } else if (ch === '{' || ch === '[') {
      const close = ch === '{' ? '}' : ']';
      let j = i + 1;
      while (j < text.length && /\s/.test(text.charAt(j))) j += 1;
      if (text.charAt(j) === close) {
        out += ch + close;
        i = j;
      } else {
        depth += 1;
        out += ch + newline();
      }
    } else if (ch === '}' || ch === ']') {
      depth -= 1;
      out += newline() + ch;
    } else if (ch === ',') {
      out += `,${newline()}`;
    } else if (ch === ':') {
      out += ': ';
    } else {
      out += ch;
    }
  }
  return out;
}

/** Pretty JSON when `text` is a JSON object or array, otherwise `null`. Numbers are kept as written. */
export function prettyJson(text: string): string | null {
  const trimmed = text.trim();
  if (!(trimmed.startsWith('{') || trimmed.startsWith('['))) return null;
  try {
    JSON.parse(trimmed);
  } catch {
    return null;
  }
  return reindentJson(trimmed);
}
