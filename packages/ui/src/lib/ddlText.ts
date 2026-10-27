/**
 * Reading DDL text in the UI: statement boundaries for the outline, the
 * MySQL/MariaDB session header, and small parsers for names the catalog
 * returns as text. Nothing here builds SQL that runs; editors build changes
 * and the backend generates the SQL (lib/ddlPlan.ts).
 */
import type { Engine } from '../api/records';

/** One statement of a script, with its 1-based first line. */
export interface DdlStatement {
  text: string;
  /** 1-based line of the statement's first code, after leading comments. */
  line: number;
  /** Offsets in the script: `[from, to)`, including the trailing `;` when present. */
  from: number;
  to: number;
}

type Dialect = 'postgres' | 'mysql';

function dialectOf(engine: Engine): Dialect {
  return engine === 'postgres' ? 'postgres' : 'mysql';
}

const DOLLAR_TAG = /^\$([A-Za-z_][A-Za-z_0-9]*)?\$/;

/** Words after `END` that close a flow-control block rather than `BEGIN` (MySQL). */
const BLOCK_CLOSERS = new Set(['IF', 'LOOP', 'WHILE', 'REPEAT', 'FOR']);

/**
 * Splits a script into statements at top-level `;`. Skips quotes,
 * identifiers, comments and (PostgreSQL) dollar quotes; for MySQL/MariaDB
 * the `;` inside `BEGIN ... END` bodies of routines, triggers and events
 * does not end the statement (`CASE ... END` expressions and `END IF`,
 * `END LOOP`, ... are told apart). Comment-only and blank pieces are dropped.
 */
export function splitStatements(script: string, engine: Engine): DdlStatement[] {
  const dialect = dialectOf(engine);
  const out: DdlStatement[] = [];
  let start = 0;
  /** Open `BEGIN` blocks. */
  let depth = 0;
  /** Open `CASE` (statement or expression). */
  let cases = 0;
  /** The previous word was `END`; the next word decides what it closed. */
  let endPending = false;
  let i = 0;
  let word = '';
  const n = script.length;

  const closeEnd = (next: string | null): void => {
    endPending = false;
    if (next === 'CASE') cases = Math.max(0, cases - 1);
    else if (next !== null && BLOCK_CLOSERS.has(next)) return;
    else if (cases > 0) cases -= 1;
    else depth = Math.max(0, depth - 1);
  };

  const flushWord = (): void => {
    if (word === '') return;
    const upper = word.toUpperCase();
    word = '';
    if (dialect !== 'mysql') return;
    if (endPending) {
      closeEnd(upper);
      if (upper === 'CASE' || BLOCK_CLOSERS.has(upper)) return;
    }
    if (upper === 'BEGIN') depth += 1;
    else if (upper === 'CASE') cases += 1;
    else if (upper === 'END') endPending = true;
  };

  const push = (to: number, end: number): void => {
    const piece = script.slice(start, to);
    if (hasCode(piece, dialect)) {
      const lead = piece.length - piece.trimStart().length;
      const from = start + lead;
      const trimmed = piece.trimStart();
      // The line of the code itself, after leading comments (outline targets).
      const codeAt = from + (trimmed.length - leadingCode(trimmed).length);
      out.push({ text: piece.trim(), line: lineAt(script, codeAt), from, to: end });
    }
    start = end;
  };

  while (i < n) {
    const c = script[i] as string;
    const next = script[i + 1];
    if (/[A-Za-z_0-9]/.test(c)) {
      word += c;
      i += 1;
      continue;
    }
    flushWord();
    if (endPending && (c === ';' || c === ')' || c === ',')) closeEnd(null);
    if (c === '-' && next === '-') {
      i = skipLine(script, i);
    } else if (c === '#' && dialect === 'mysql') {
      i = skipLine(script, i);
    } else if (c === '/' && next === '*') {
      i = skipBlockComment(script, i, dialect === 'postgres');
    } else if (c === "'") {
      const backslash = dialect === 'mysql' || isEscapeString(script, i);
      i = skipQuoted(script, i, "'", backslash);
    } else if (c === '"') {
      i = skipQuoted(script, i, '"', dialect === 'mysql');
    } else if (c === '`' && dialect === 'mysql') {
      i = skipQuoted(script, i, '`', false);
    } else if (c === '$' && dialect === 'postgres') {
      const tag = DOLLAR_TAG.exec(script.slice(i));
      if (tag && !/[A-Za-z_0-9]/.test(script[i - 1] ?? '')) {
        const close = script.indexOf(tag[0], i + tag[0].length);
        i = close < 0 ? n : close + tag[0].length;
      } else {
        i += 1;
      }
    } else if (c === ';' && depth === 0) {
      push(i, i + 1);
      i += 1;
    } else {
      i += 1;
    }
  }
  flushWord();
  if (endPending) closeEnd(null);
  push(n, n);
  return out;
}

function isEscapeString(script: string, quote: number): boolean {
  const before = script[quote - 1];
  return (before === 'E' || before === 'e') && !/[A-Za-z_0-9]/.test(script[quote - 2] ?? '');
}

function skipLine(script: string, i: number): number {
  const end = script.indexOf('\n', i);
  return end < 0 ? script.length : end + 1;
}

function skipBlockComment(script: string, i: number, nested: boolean): number {
  let level = 0;
  let j = i;
  while (j < script.length) {
    if (script[j] === '/' && script[j + 1] === '*') {
      level = nested ? level + 1 : 1;
      j += 2;
    } else if (script[j] === '*' && script[j + 1] === '/') {
      level -= 1;
      j += 2;
      if (level <= 0) return j;
    } else {
      j += 1;
    }
  }
  return script.length;
}

function skipQuoted(script: string, i: number, quote: string, backslash: boolean): number {
  let j = i + 1;
  while (j < script.length) {
    const c = script[j];
    if (backslash && c === '\\') {
      j += 2;
    } else if (c === quote) {
      if (script[j + 1] === quote) j += 2;
      else return j + 1;
    } else {
      j += 1;
    }
  }
  return script.length;
}

/** True when the text holds anything other than whitespace and comments. */
function hasCode(text: string, dialect: Dialect): boolean {
  let i = 0;
  while (i < text.length) {
    const c = text[i] as string;
    if (/\s/.test(c)) i += 1;
    else if (c === '-' && text[i + 1] === '-') i = skipLine(text, i);
    else if (c === '#' && dialect === 'mysql') i = skipLine(text, i);
    else if (c === '/' && text[i + 1] === '*') i = skipBlockComment(text, i, dialect === 'postgres');
    else return true;
  }
  return false;
}

function lineAt(text: string, offset: number): number {
  let line = 1;
  for (let i = 0; i < offset && i < text.length; i += 1) if (text[i] === '\n') line += 1;
  return line;
}

/** The statement's leading code, without leading comments. */
export function leadingCode(statement: string): string {
  let text = statement;
  for (;;) {
    const trimmed = text.trimStart();
    if (trimmed.startsWith('--') || trimmed.startsWith('#')) {
      const end = trimmed.indexOf('\n');
      text = end < 0 ? '' : trimmed.slice(end + 1);
    } else if (trimmed.startsWith('/*')) {
      const end = trimmed.indexOf('*/');
      text = end < 0 ? '' : trimmed.slice(end + 2);
    } else {
      return trimmed;
    }
  }
}

/** Outline groups of a DDL script, in the order they first appear. */
export type OutlineGroup =
  | 'create'
  | 'constraints'
  | 'owner'
  | 'indexes'
  | 'triggers'
  | 'rules'
  | 'policies'
  | 'comments'
  | 'privileges'
  | 'other';

export interface OutlineEntry {
  group: OutlineGroup;
  label: string;
  /** 1-based line of the first statement of the group. */
  line: number;
  count: number;
}

const GROUP_LABELS: Record<OutlineGroup, string> = {
  create: 'Definition',
  constraints: 'Constraints',
  owner: 'Owner',
  indexes: 'Indexes',
  triggers: 'Triggers',
  rules: 'Rules',
  policies: 'Policies',
  comments: 'Comments',
  privileges: 'Privileges',
  other: 'Other statements',
};

/** Which outline group a statement belongs to. */
export function statementGroup(statement: string): OutlineGroup {
  const code = leadingCode(statement).replace(/\s+/g, ' ').toUpperCase();
  if (/^COMMENT ON /.test(code)) return 'comments';
  if (/^(GRANT|REVOKE) /.test(code)) return 'privileges';
  if (/ OWNER TO /.test(code)) return 'owner';
  if (/^CREATE (UNIQUE )?INDEX /.test(code)) return 'indexes';
  if (/^CREATE (OR REPLACE )?(CONSTRAINT )?TRIGGER /.test(code) || /^ALTER TABLE .* (ENABLE|DISABLE) (ALWAYS |REPLICA )?TRIGGER /.test(code)) {
    return 'triggers';
  }
  if (/^CREATE (OR REPLACE )?RULE /.test(code)) return 'rules';
  if (/^(CREATE|ALTER) POLICY /.test(code) || / (ENABLE|FORCE) ROW LEVEL SECURITY/.test(code)) return 'policies';
  if (/^ALTER TABLE .* ADD CONSTRAINT /.test(code)) return 'constraints';
  if (/^CREATE /.test(code)) return 'create';
  return 'other';
}

/** The outline of a script: one entry per group, with the first line and a count. */
export function outline(script: string, engine: Engine): OutlineEntry[] {
  const out: OutlineEntry[] = [];
  for (const statement of splitStatements(script, engine)) {
    const group = statementGroup(statement.text);
    const hit = out.find((o) => o.group === group);
    if (hit) hit.count += 1;
    else out.push({ group, label: GROUP_LABELS[group], line: statement.line, count: 1 });
  }
  return out;
}

/**
 * The script without its `COMMENT ON` statements (PostgreSQL; MySQL and
 * MariaDB keep comments inside the CREATE). Leading notes and the text
 * between statements are kept as they are.
 */
export function withoutComments(script: string, engine: Engine): string {
  const statements = splitStatements(script, engine).filter((s) => statementGroup(s.text) === 'comments');
  if (statements.length === 0) return script;
  let out = '';
  let at = 0;
  for (const s of statements) {
    out += script.slice(at, s.from);
    at = s.to;
    // Drop the blank line that separated the removed statement.
    while (script[at] === '\n' && (out.endsWith('\n\n') || out === '')) at += 1;
  }
  out += script.slice(at);
  return out.replace(/\n{3,}/g, '\n\n').replace(/^\n+/, '');
}

/** The first characters of a MySQL/MariaDB DDL session header (`crates/catalog/src/mysql/session.rs`). */
export const DDL_SESSION_PREFIX = '-- varve-session: ';

/** The settings of a session header line, in header order; `null` when the script has none. */
export function sessionHeader(script: string): [string, string][] | null {
  const first = script.split('\n', 1)[0] ?? '';
  if (!first.startsWith(DDL_SESSION_PREFIX)) return null;
  const pairs: [string, string][] = [];
  for (const part of first.slice(DDL_SESSION_PREFIX.length).split(';')) {
    const item = part.trim();
    if (item === '') continue;
    const eq = item.indexOf('=');
    if (eq < 0) pairs.push([item, '']);
    else pairs.push([item.slice(0, eq).trim(), item.slice(eq + 1).trim()]);
  }
  return pairs;
}

/** The `collation_database` of a session header, if any: the run then changes the database's collation briefly. */
export function headerCollationDatabase(script: string): string | null {
  return sessionHeader(script)?.find(([k]) => k === 'collation_database')?.[1] ?? null;
}

/**
 * Splits `schema.name` as PostgreSQL prints `regproc`/`regprocedure`/type
 * names: double-quoted parts are unquoted, an unqualified name has a `null`
 * schema (it is visible through `search_path`). An argument list
 * (`f(integer)`) is dropped.
 */
export function parseQualified(text: string): { schema: string | null; name: string } {
  const parts: string[] = [];
  let current = '';
  let i = 0;
  const source = text.trim();
  while (i < source.length) {
    const c = source[i] as string;
    if (c === '"') {
      let j = i + 1;
      while (j < source.length) {
        if (source[j] === '"' && source[j + 1] === '"') {
          current += '"';
          j += 2;
        } else if (source[j] === '"') {
          break;
        } else {
          current += source[j];
          j += 1;
        }
      }
      i = j + 1;
    } else if (c === '.') {
      parts.push(current);
      current = '';
      i += 1;
    } else if (c === '(') {
      break;
    } else {
      current += c;
      i += 1;
    }
  }
  parts.push(current.trim());
  const name = parts[parts.length - 1] ?? '';
  const schema = parts.length > 1 ? (parts[parts.length - 2] ?? null) : null;
  return { schema, name };
}

/**
 * The arguments of a trigger's function call in `pg_get_triggerdef` text
 * (`... EXECUTE FUNCTION f('a', 'b''c')`): string literals, unquoted.
 */
export function triggerArguments(definition: string): string[] {
  const match = /EXECUTE (?:FUNCTION|PROCEDURE)\s+/i.exec(definition);
  if (!match) return [];
  let i = match.index + match[0].length;
  // Skip the (possibly quoted) function name.
  while (i < definition.length && definition[i] !== '(') {
    if (definition[i] === '"') i = skipQuoted(definition, i, '"', false);
    else i += 1;
  }
  i += 1;
  const args: string[] = [];
  while (i < definition.length) {
    const c = definition[i] as string;
    if (c === ')') break;
    if (c === "'") {
      let value = '';
      let j = i + 1;
      while (j < definition.length) {
        if (definition[j] === "'" && definition[j + 1] === "'") {
          value += "'";
          j += 2;
        } else if (definition[j] === "'") {
          break;
        } else {
          value += definition[j];
          j += 1;
        }
      }
      args.push(value);
      i = j + 1;
    } else if (/[^\s,]/.test(c)) {
      // A bare token (numbers are printed as literals, but stay safe).
      let j = i;
      while (j < definition.length && !/[\s,)]/.test(definition[j] as string)) j += 1;
      args.push(definition.slice(i, j));
      i = j;
    } else {
      i += 1;
    }
  }
  return args;
}

/** Splits a `name=value` entry of `proconfig` into a setting; list values (`search_path`) become several items. */
export function parseConfigEntry(entry: string): { name: string; values: string[] } {
  const eq = entry.indexOf('=');
  const name = eq < 0 ? entry : entry.slice(0, eq);
  const raw = eq < 0 ? '' : entry.slice(eq + 1);
  const values = name === 'search_path' || name.endsWith('_path') ? raw.split(',').map((v) => v.trim().replace(/^"(.*)"$/, '$1')) : [raw];
  return { name, values };
}
