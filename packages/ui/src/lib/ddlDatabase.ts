/**
 * Form models of the database and schema editors (PostgreSQL databases and
 * schemas, MySQL and MariaDB databases) and the typed changes they send to
 * `edit_plan` (crates/catalog/src/postgres/edit/database.rs,
 * crates/catalog/src/mysql/edit/database.rs). Like `lib/ddlPlan.ts`: the
 * backend writes and quotes the SQL; this module decides which changes an
 * edit needs and validates the form first.
 */
import { z } from 'zod';
import type {
  ConfigSetting,
  EngineChange,
  MyDatabase,
  MyDatabaseOptions,
  PgCreateDatabase,
  PgDatabase,
  PgDatabaseOperation,
  PgLocaleProvider,
  PgSchema,
  PgSchemaOperation,
} from '../api/ddl';
import type { ServerInfo } from '../api/sessions';
import { objectRef, parseSettings, roleSpec, textOrNull } from './ddlPlan';
import type { FieldErrors } from './ddlPlan';

/** PostgreSQL keeps the first 63 bytes of a name (`NAMEDATALEN - 1`). */
const PG_NAME_BYTES = 63;
/** MySQL and MariaDB database names are at most 64 characters. */
const MY_NAME_CHARS = 64;

/** Roles `CREATE DATABASE ... OWNER` does not take (only `ALTER DATABASE ... OWNER TO` does). */
const ROLE_KEYWORDS = new Set(['CURRENT_USER', 'SESSION_USER', 'CURRENT_ROLE', 'PUBLIC']);

function utf8Bytes(text: string): number {
  return new TextEncoder().encode(text).length;
}

function same(a: string | null | undefined, b: string | null | undefined): boolean {
  return (a ?? '').trim() === (b ?? '').trim();
}

/** A PostgreSQL name problem, or `null`. */
export function pgNameProblem(name: string): string | null {
  const t = name.trim();
  if (t === '') return 'Enter a name.';
  if (utf8Bytes(t) > PG_NAME_BYTES) return `PostgreSQL names are at most ${PG_NAME_BYTES} bytes; a longer one is cut off.`;
  return null;
}

/** True on MySQL (not MariaDB) `major.minor.patch` or newer. */
export function mysqlAt(server: ServerInfo, major: number, minor: number, patch: number): boolean {
  if (server.engine !== 'mysql') return false;
  const p = server.patch ?? 0;
  if (server.major !== major) return server.major > major;
  if (server.minor !== minor) return server.minor > minor;
  return p >= patch;
}

/** True on MariaDB `major.minor` or newer. */
export function mariadbAt(server: ServerInfo, major: number, minor: number): boolean {
  return server.engine === 'mariadb' && (server.major > major || (server.major === major && server.minor >= minor));
}

function pgAt(server: ServerInfo, major: number): boolean {
  return server.engine === 'postgres' && server.major >= major;
}

// ---------------------------------------------------------------------------
// PostgreSQL databases
// ---------------------------------------------------------------------------

export interface PgDatabaseForm {
  name: string;
  /** Role name; blank keeps it (create: the current user owns it). */
  owner: string;
  comment: string;
  /** Create only. */
  template: string;
  encoding: string;
  strategy: '' | 'walLog' | 'fileCopy';
  localeProvider: '' | PgLocaleProvider;
  locale: string;
  lcCollate: string;
  lcCtype: string;
  icuLocale: string;
  icuRules: string;
  builtinLocale: string;
  /** Create and alter (alter moves every file: its own step). */
  tablespace: string;
  allowConnections: boolean;
  /** Whole number, `-1` for no limit. */
  connectionLimit: string;
  isTemplate: boolean;
  /** Session defaults for every role, one `name = value` per line. */
  settings: string;
}

function settingsText(db: PgDatabase): string {
  return db.settings
    .filter((s) => s.role === null)
    .map((s) => `${s.name} = ${s.value}`)
    .join('\n');
}

export function pgDatabaseFormFrom(db: PgDatabase | null): PgDatabaseForm {
  return {
    name: db?.name ?? '',
    owner: db?.owner ?? '',
    comment: db?.comment ?? '',
    template: '',
    encoding: '',
    strategy: '',
    localeProvider: '',
    locale: '',
    lcCollate: '',
    lcCtype: '',
    icuLocale: '',
    icuRules: '',
    builtinLocale: '',
    tablespace: db?.tablespace ?? '',
    allowConnections: db?.allowConnections ?? true,
    connectionLimit: String(db?.connectionLimit ?? -1),
    isTemplate: db?.isTemplate ?? false,
    settings: db ? settingsText(db) : '',
  };
}

const INT32_MAX = 2147483647;

/** The message for a connection limit, or `null`. */
export function connectionLimitProblem(text: string): string | null {
  const t = text.trim();
  if (!/^[+-]?\d+$/.test(t)) return 'Enter a whole number (-1 for no limit).';
  const value = Number(t);
  if (value < -1) return 'Enter -1 (no limit) or more.';
  if (value > INT32_MAX) return `Enter at most ${INT32_MAX}.`;
  return null;
}

/** Problems of the settings text: one `name = value` per line. */
export function settingsProblem(text: string): string | null {
  for (const line of text.split('\n')) {
    const t = line.trim();
    if (t === '') continue;
    const eq = t.indexOf('=');
    if (eq <= 0) return `Write each setting as name = value ("${t}").`;
    if (!/^[A-Za-z_][A-Za-z0-9_.]*$/.test(t.slice(0, eq).trim())) return `"${t.slice(0, eq).trim()}" is not a setting name.`;
  }
  const names = parseSettings(text).map((s) => s.name.toLowerCase());
  const twice = names.find((n, i) => names.indexOf(n) !== i);
  return twice ? `${twice} is set twice.` : null;
}

/** Validation of the PostgreSQL database form; `creating` checks the create-only options. */
export function pgDatabaseErrors(form: PgDatabaseForm, creating: boolean, server: ServerInfo): FieldErrors {
  const out: FieldErrors = {};
  const name = pgNameProblem(form.name);
  if (name) out.name = name;
  const limit = connectionLimitProblem(form.connectionLimit);
  if (limit) out.connectionLimit = limit;
  const settings = settingsProblem(form.settings);
  if (settings) out.settings = settings;
  if (!creating) return out;
  if (ROLE_KEYWORDS.has(form.owner.trim().toUpperCase())) {
    out.owner = 'CREATE DATABASE takes a role name here. Leave it blank to own the database yourself.';
  }
  const provider = form.localeProvider === '' ? 'libc' : form.localeProvider;
  if (form.strategy !== '' && !pgAt(server, 15)) out.strategy = 'The copy strategy needs PostgreSQL 15 or newer.';
  if (form.localeProvider !== '' && !pgAt(server, 15)) out.localeProvider = 'Choosing a locale provider needs PostgreSQL 15 or newer.';
  else if (form.localeProvider === 'builtin' && !pgAt(server, 17)) out.localeProvider = 'The builtin provider needs PostgreSQL 17 or newer.';
  if (form.icuLocale.trim() !== '' && provider !== 'icu') out.icuLocale = 'An ICU locale needs the icu provider.';
  if (form.icuRules.trim() !== '') {
    if (!pgAt(server, 16)) out.icuRules = 'ICU rules need PostgreSQL 16 or newer.';
    else if (provider !== 'icu') out.icuRules = 'ICU rules need the icu provider.';
  }
  if (form.builtinLocale.trim() !== '' && provider !== 'builtin') out.builtinLocale = 'A builtin locale needs the builtin provider.';
  if (provider === 'icu' && form.icuLocale.trim() === '' && form.locale.trim() === '') out.icuLocale = 'The icu provider needs an ICU locale.';
  if (provider === 'builtin' && form.builtinLocale.trim() === '' && form.locale.trim() === '') {
    out.builtinLocale = 'The builtin provider needs a locale, for example C.UTF-8.';
  }
  return out;
}

function pg(change: Extract<EngineChange, { engine: 'postgres' }>['change']): EngineChange {
  return { engine: 'postgres', change };
}

function settingOps(form: PgDatabaseForm, original: PgDatabase | null): PgDatabaseOperation[] {
  const before = new Map<string, string>();
  for (const s of original?.settings ?? []) if (s.role === null) before.set(s.name.toLowerCase(), s.value.trim());
  const ops: PgDatabaseOperation[] = [];
  const typed = new Map<string, ConfigSetting>();
  const lines = new Map<string, string>();
  for (const line of form.settings.split('\n')) {
    const t = line.trim();
    if (t === '') continue;
    const eq = t.indexOf('=');
    lines.set(t.slice(0, eq).trim().toLowerCase(), t.slice(eq + 1).trim());
  }
  for (const setting of parseSettings(form.settings)) typed.set(setting.name.toLowerCase(), setting);
  for (const [name, setting] of typed) {
    if (before.get(name) !== lines.get(name)) ops.push({ op: 'set', setting: { name: setting.name, values: setting.values } });
  }
  for (const name of before.keys()) if (!typed.has(name)) ops.push({ op: 'reset', name });
  return ops;
}

/**
 * The changes that save a PostgreSQL database form, in order: create (with
 * its settings as a second change), or alter: options and settings, the
 * tablespace (its own change: it cannot run in a transaction), the comment,
 * and the rename last.
 */
export function buildPgDatabase(form: PgDatabaseForm, original: PgDatabase | null): EngineChange[] {
  const name = form.name.trim();
  if (!original) {
    const owner = textOrNull(form.owner);
    const limit = Number(form.connectionLimit.trim());
    const all: PgCreateDatabase = {
      name,
      owner: owner ? { kind: 'name', name: owner } : null,
      template: textOrNull(form.template),
      encoding: textOrNull(form.encoding),
      strategy: form.strategy === '' ? null : form.strategy,
      localeProvider: form.localeProvider === '' ? null : form.localeProvider,
      locale: textOrNull(form.locale),
      lcCollate: textOrNull(form.lcCollate),
      lcCtype: textOrNull(form.lcCtype),
      icuLocale: textOrNull(form.icuLocale),
      icuRules: textOrNull(form.icuRules),
      builtinLocale: textOrNull(form.builtinLocale),
      tablespace: textOrNull(form.tablespace),
      allowConnections: form.allowConnections ? null : false,
      connectionLimit: limit === -1 ? null : limit,
      isTemplate: form.isTemplate ? true : null,
      comment: textOrNull(form.comment),
    };
    // Only what the user set: the server's defaults apply to the rest.
    const create = Object.fromEntries(Object.entries(all).filter(([, v]) => v !== null)) as unknown as PgCreateDatabase;
    const out = [pg({ object: 'database', action: 'create', ...create })];
    const settings = settingOps(form, null);
    if (settings.length > 0) out.push(pg({ object: 'database', action: 'alter', name, operations: settings }));
    return out;
  }
  const out: EngineChange[] = [];
  const ops: PgDatabaseOperation[] = [];
  if (textOrNull(form.owner) !== null && !same(form.owner, original.owner)) ops.push({ op: 'owner', owner: roleSpec(form.owner) });
  const limit = Number(form.connectionLimit.trim());
  if (limit !== original.connectionLimit) ops.push({ op: 'connectionLimit', limit });
  if (form.allowConnections !== original.allowConnections) ops.push({ op: 'allowConnections', allow: form.allowConnections });
  if (form.isTemplate !== original.isTemplate) ops.push({ op: 'isTemplate', isTemplate: form.isTemplate });
  ops.push(...settingOps(form, original));
  if (ops.length > 0) out.push(pg({ object: 'database', action: 'alter', name: original.name, operations: ops }));
  if (textOrNull(form.tablespace) !== null && !same(form.tablespace, original.tablespace)) {
    out.push(pg({ object: 'database', action: 'alter', name: original.name, operations: [{ op: 'tablespace', tablespace: form.tablespace.trim() }] }));
  }
  if (!same(form.comment, original.comment)) {
    out.push(pg({ object: 'comment', target: objectRef('database', original.name, null, original.name), comment: textOrNull(form.comment) }));
  }
  if (name !== original.name) out.push(pg({ object: 'database', action: 'alter', name: original.name, operations: [{ op: 'rename', newName: name }] }));
  return out;
}

/** Whether the form renames the database or moves its tablespace: both need every session off it. */
export function needsDatabaseToItself(form: PgDatabaseForm, original: PgDatabase | null): boolean {
  if (!original) return false;
  return form.name.trim() !== original.name || (textOrNull(form.tablespace) !== null && !same(form.tablespace, original.tablespace));
}

/** `DROP DATABASE`; `force` (13+) terminates the other sessions first. */
export function buildPgDatabaseDrop(name: string, force: boolean): EngineChange {
  return pg({ object: 'database', action: 'drop', name, ...(force ? { force: true } : {}) });
}

// ---------------------------------------------------------------------------
// PostgreSQL schemas
// ---------------------------------------------------------------------------

export interface PgSchemaForm {
  name: string;
  /** Blank: the current user (create) or unchanged (alter). */
  owner: string;
  comment: string;
}

export function pgSchemaFormFrom(schema: PgSchema | null): PgSchemaForm {
  return { name: schema?.name ?? '', owner: schema?.owner ?? '', comment: schema?.comment ?? '' };
}

export function pgSchemaErrors(form: PgSchemaForm, original: PgSchema | null): FieldErrors {
  const out: FieldErrors = {};
  const name = pgNameProblem(form.name);
  if (name) out.name = name;
  else if (/^pg_/i.test(form.name.trim()) && form.name.trim() !== original?.name) out.name = 'Names starting with pg_ are reserved for system schemas.';
  return out;
}

/** Create, or alter: owner, comment, then the rename. */
export function buildPgSchema(form: PgSchemaForm, original: PgSchema | null, database: string | null): EngineChange[] {
  const name = form.name.trim();
  if (!original) {
    const owner = textOrNull(form.owner);
    return [
      pg({
        object: 'schema',
        action: 'create',
        name,
        ...(owner ? { owner: roleSpec(owner) } : {}),
        ...(textOrNull(form.comment) ? { comment: textOrNull(form.comment) } : {}),
      }),
    ];
  }
  const out: EngineChange[] = [];
  const ops: PgSchemaOperation[] = [];
  if (textOrNull(form.owner) !== null && !same(form.owner, original.owner)) ops.push({ op: 'owner', owner: roleSpec(form.owner) });
  if (ops.length > 0) out.push(pg({ object: 'schema', action: 'alter', name: original.name, operations: ops }));
  if (!same(form.comment, original.comment)) {
    out.push(pg({ object: 'comment', target: objectRef('schema', database, null, original.name), comment: textOrNull(form.comment) }));
  }
  if (name !== original.name) out.push(pg({ object: 'schema', action: 'alter', name: original.name, operations: [{ op: 'rename', newName: name }] }));
  return out;
}

// ---------------------------------------------------------------------------
// MySQL and MariaDB databases
// ---------------------------------------------------------------------------

/** `''` leaves the server default (create) or the current value (alter). */
export type Tristate = '' | 'on' | 'off';

export interface MyDatabaseForm {
  name: string;
  charset: string;
  collation: string;
  /** MySQL 8.0.16+: default table encryption. */
  encryption: Tristate;
  /** MySQL 8.0.22+: refuse changes to the database. */
  readOnly: Tristate;
  /** MariaDB 10.5+. */
  comment: string;
}

function tristate(value: boolean | null): Tristate {
  if (value === null) return '';
  return value ? 'on' : 'off';
}

export function myDatabaseFormFrom(db: MyDatabase | null): MyDatabaseForm {
  return {
    name: db?.name ?? '',
    charset: db?.charset ?? '',
    collation: db?.collation ?? '',
    encryption: tristate(db?.encryption ?? null),
    readOnly: tristate(db?.readOnly ?? null),
    comment: db?.comment ?? '',
  };
}

/** What the server supports of the MySQL / MariaDB database options. */
export interface MyDatabaseFeatures {
  encryption: boolean;
  readOnly: boolean;
  comment: boolean;
}

export function myDatabaseFeatures(server: ServerInfo): MyDatabaseFeatures {
  return {
    encryption: mysqlAt(server, 8, 0, 16),
    readOnly: mysqlAt(server, 8, 0, 22),
    comment: mariadbAt(server, 10, 5),
  };
}

const CHARSET_NAME = /^[A-Za-z0-9_]+$/;

export const myDatabaseSchema = z.object({
  name: z
    .string()
    .refine((v) => v.trim() !== '', 'Enter a name.')
    .refine((v) => v.trim().length <= MY_NAME_CHARS, `Database names are at most ${MY_NAME_CHARS} characters.`)
    .refine((v) => v === '' || v.trim() === v, 'Names cannot start or end with a space.'),
  charset: z.string().refine((v) => v.trim() === '' || CHARSET_NAME.test(v.trim()), 'Enter a character set name such as utf8mb4.'),
  collation: z.string().refine((v) => v.trim() === '' || CHARSET_NAME.test(v.trim()), 'Enter a collation name such as utf8mb4_0900_ai_ci.'),
  encryption: z.enum(['', 'on', 'off']),
  readOnly: z.enum(['', 'on', 'off']),
  comment: z.string().refine((v) => v.length <= 1024, 'Comments are at most 1024 characters.'),
});

/** Validation, including options the server version lacks. */
export function myDatabaseErrors(form: MyDatabaseForm, original: MyDatabase | null, features: MyDatabaseFeatures): FieldErrors {
  const out: FieldErrors = {};
  const result = myDatabaseSchema.safeParse(form);
  if (!result.success) {
    for (const issue of result.error.issues) {
      const path = issue.path.join('.');
      if (!(path in out)) out[path] = issue.message;
    }
  }
  if (original && form.name.trim() !== original.name) out.name = 'MySQL and MariaDB cannot rename a database. Create a new one and move the tables.';
  if (form.encryption !== tristate(original?.encryption ?? null) && !features.encryption) out.encryption = 'Default encryption needs MySQL 8.0.16 or newer.';
  if (form.readOnly !== tristate(original?.readOnly ?? null) && !features.readOnly) out.readOnly = 'Read-only databases need MySQL 8.0.22 or newer.';
  if (!same(form.comment, original?.comment) && !features.comment) out.comment = 'Database comments need MariaDB 10.5 or newer.';
  if (!original && form.readOnly === 'on') out.readOnly = 'Create the database first; a new database cannot start read only.';
  if (original?.readOnly === true && form.readOnly !== 'off' && !out.readOnly) {
    const changed =
      !same(form.charset, original.charset) ||
      !same(form.collation, original.collation) ||
      form.encryption !== tristate(original.encryption) ||
      !same(form.comment, original.comment);
    if (changed) out.readOnly = 'The database is read only (MySQL refuses changes to it): choose Writable to change its options in the same statement.';
  }
  return out;
}

function my(change: Extract<EngineChange, { engine: 'mysql' }>['change']): EngineChange {
  return { engine: 'mysql', change };
}

/** `CREATE DATABASE` with the options given, or one `ALTER DATABASE` with what changed (none: no change). */
export function buildMyDatabase(form: MyDatabaseForm, original: MyDatabase | null): EngineChange[] {
  const options: MyDatabaseOptions = {};
  const changed = (a: string, b: string | null | undefined): boolean => !original || !same(a, b);
  if (textOrNull(form.charset) !== null && changed(form.charset, original?.charset)) options.charset = form.charset.trim();
  if (textOrNull(form.collation) !== null && changed(form.collation, original?.collation)) options.collation = form.collation.trim();
  if (form.encryption !== '' && form.encryption !== tristate(original?.encryption ?? null)) options.encryption = form.encryption === 'on';
  if (original ? !same(form.comment, original.comment) : textOrNull(form.comment) !== null) options.comment = form.comment.trim();
  if (!original) return [my({ change: 'createDatabase', name: form.name.trim(), ...options })];
  const readOnly = form.readOnly !== '' && form.readOnly !== tristate(original.readOnly) ? form.readOnly === 'on' : null;
  if (Object.keys(options).length === 0 && readOnly === null) return [];
  return [my({ change: 'alterDatabase', name: original.name, ...options, ...(readOnly !== null ? { readOnly } : {}) })];
}
