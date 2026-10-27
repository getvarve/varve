/**
 * Form models of the object editors and the typed changes they send to
 * `edit_plan`. The backend turns a change into SQL (quoting every name and
 * value itself); this module only decides WHICH changes an edit needs, in
 * which order, and validates the form before anything is planned.
 *
 * Each editor has: a form type, `*FormFrom` (form from the described
 * object), `*Schema` (zod validation, messages per field) and `build*`
 * (form plus the original object to `EngineChange[]`). One change is one
 * server-held plan; an edit that needs several (definition, then comment,
 * then rename) runs them in the order returned.
 */
import { z } from 'zod';
import type {
  ConfigSetting,
  EngineChange,
  MyAlterOp,
  MyAlterRoutine,
  MyChange,
  MyDataType,
  MyDefiner,
  MyEvent,
  MyEventSchedule,
  MyEventStatus,
  MyEventTime,
  MyIntervalUnit,
  MyRoutine,
  MySequence,
  MySequenceFields,
  MySequenceLimit,
  MyTrigger,
  MyTypeName,
  MyView,
  ObjectKind,
  ObjectRef,
  PgArgMode,
  PgChange,
  PgCompositeAttribute,
  PgCompositeOperation,
  PgCompositeType,
  PgDomain,
  PgDomainOperation,
  PgEnumType,
  PgEventTrigger,
  PgEventTriggerEvent,
  PgFunction,
  PgMaterializedView,
  PgMaterializedViewOperation,
  PgParallel,
  PgRangeType,
  PgRoutineArg,
  PgRoutineBody,
  PgRoutineOperation,
  PgSequence,
  PgSequenceLimit,
  PgSequenceOperation,
  PgSequenceOptions,
  PgSequenceType,
  PgTrigger,
  PgTriggerEvent,
  PgView,
  PgViewOperation,
  PgVolatility,
  QualifiedName,
  RoleSpec,
  FiringState,
} from '../api/ddl';
import type { Engine } from '../api/records';
import { parseConfigEntry, parseQualified, splitStatements, triggerArguments } from './ddlText';
import { commentChanged, normalizeComment, sameLineEndings } from './objectComment';

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/** Field path to message, for `FormField`. */
export type FieldErrors = Record<string, string>;

/** The server an editor plans for. */
export interface ServerTarget {
  engine: Engine;
  major: number;
  minor: number;
}

/** True on PostgreSQL `major` or newer. */
export function pgAtLeast(server: ServerTarget, major: number): boolean {
  return server.engine === 'postgres' && server.major >= major;
}

/** Messages per field of a failed zod parse; the first message of a path wins. */
export function errorsOf<T>(schema: z.ZodType<T>, values: unknown): FieldErrors {
  const result = schema.safeParse(values);
  if (result.success) return {};
  const out: FieldErrors = {};
  for (const issue of result.error.issues) {
    const path = issue.path.join('.');
    if (!(path in out)) out[path] = issue.message;
  }
  return out;
}

const custom = (ctx: z.RefinementCtx, path: (string | number)[], message: string): void => {
  ctx.addIssue({ code: z.ZodIssueCode.custom, path, message });
};

/** Trimmed text, or `null` when blank. */
export function textOrNull(value: string): string | null {
  const t = value.trim();
  return t === '' ? null : t;
}

function same(a: string | null | undefined, b: string | null | undefined): boolean {
  return (a ?? '').trim() === (b ?? '').trim();
}

/** Whole numbers the editors accept: JavaScript sends exact integers only up to +-(2^53 - 1). */
export function parseInteger(text: string): number | null {
  const t = text.trim();
  if (!/^[+-]?\d+$/.test(t)) return null;
  const value = Number(t);
  return Number.isSafeInteger(value) ? value : null;
}

/** Signed and unsigned 64-bit limits (sequence values, `bigint`). */
export const INT64_MIN = -(2n ** 63n);
export const INT64_MAX = 2n ** 63n - 1n;
export const UINT64_MAX = 2n ** 64n - 1n;

/** An exact whole number within `min` to `max` (64-bit signed by default), or `null`. Never passes through a JavaScript number. */
export function parseBigInteger(text: string, min: bigint = INT64_MIN, max: bigint = INT64_MAX): bigint | null {
  const t = text.trim();
  if (!/^[+-]?\d+$/.test(t)) return null;
  const value = BigInt(t);
  return value < min || value > max ? null : value;
}

/** Decimal text of a whole number field for the backend (64-bit values travel as text); `null` when blank or invalid. */
export function integerText(text: string, min: bigint = INT64_MIN, max: bigint = INT64_MAX): string | null {
  return parseBigInteger(text, min, max)?.toString() ?? null;
}

/** The message for a 64-bit whole number field, or `null` when it is fine (blank allowed when `optional`). */
export function bigIntegerProblem(text: string, optional: boolean, min: bigint = INT64_MIN, max: bigint = INT64_MAX): string | null {
  const t = text.trim();
  if (t === '') return optional ? null : 'Enter a whole number.';
  if (!/^[+-]?\d+$/.test(t)) return 'Enter a whole number.';
  if (parseBigInteger(t, min, max) === null) return `Enter a whole number from ${min.toString()} to ${max.toString()}.`;
  return null;
}

/** The message for a whole number field, or `null` when it is fine (blank allowed when `optional`). */
export function integerProblem(text: string, optional: boolean): string | null {
  const t = text.trim();
  if (t === '') return optional ? null : 'Enter a whole number.';
  if (!/^[+-]?\d+$/.test(t)) return 'Enter a whole number.';
  if (parseInteger(t) === null) {
    return 'Values beyond -9007199254740991 to 9007199254740991 cannot be sent from this form. Edit the DDL instead.';
  }
  return null;
}

/** A role from an owner field: `PUBLIC`, `CURRENT_USER`, ... or a role name. */
export function roleSpec(owner: string): RoleSpec {
  const t = owner.trim();
  switch (t.toUpperCase()) {
    case 'CURRENT_USER':
      return { kind: 'currentUser' };
    case 'SESSION_USER':
      return { kind: 'sessionUser' };
    case 'CURRENT_ROLE':
      return { kind: 'currentRole' };
    default:
      return { kind: 'name', name: t };
  }
}

function pg(change: PgChange): EngineChange {
  return { engine: 'postgres', change };
}

function my(change: MyChange): EngineChange {
  return { engine: 'mysql', change };
}

function qn(schema: string | null, name: string): QualifiedName {
  return { schema: textOrNull(schema ?? ''), name: name.trim() };
}

/** An `ObjectRef` for comment and owner changes. */
export function objectRef(
  kind: ObjectKind,
  database: string | null,
  schema: string | null,
  name: string,
  extra: { parent?: string | null; signature?: string | null } = {},
): ObjectRef {
  return { kind, database, schema, parent: extra.parent ?? null, name, signature: extra.signature ?? null };
}

/** Comment change for any PostgreSQL object (`null` removes it). */
function commentChange(target: ObjectRef, comment: string): EngineChange {
  return pg({ object: 'comment', target, comment: textOrNull(comment) });
}

/** The owner, schema and name part every PostgreSQL editor shares. */
export interface PgIdentity {
  schema: string;
  name: string;
  owner: string;
  comment: string;
}

/** Owner, schema and rename operations, in that order (renames are tracked by the backend). */
function identityOps<T extends { op: 'owner'; owner: RoleSpec } | { op: 'setSchema'; schema: string } | { op: 'rename'; newName: string }>(
  form: PgIdentity,
  original: { schema: string; name: string; owner: string | null },
): T[] {
  const ops: T[] = [];
  if (textOrNull(form.owner) !== null && !same(form.owner, original.owner)) ops.push({ op: 'owner', owner: roleSpec(form.owner) } as T);
  if (!same(form.schema, original.schema)) ops.push({ op: 'setSchema', schema: form.schema.trim() } as T);
  if (!same(form.name, original.name)) ops.push({ op: 'rename', newName: form.name.trim() } as T);
  return ops;
}

const identitySchema = {
  schema: z.string().refine((v) => v.trim() !== '', 'Choose a schema.'),
  name: z.string().refine((v) => v.trim() !== '', 'Enter a name.'),
  owner: z.string(),
  comment: z.string(),
};

/** The kind of statements a list of changes produces, for confirmation copy. */
export function isDrop(change: EngineChange): boolean {
  const c = change.change as { action?: string; change?: string };
  return c.action === 'drop' || (typeof c.change === 'string' && c.change.startsWith('drop'));
}

// ---------------------------------------------------------------------------
// PostgreSQL views
// ---------------------------------------------------------------------------

export interface PgViewForm extends PgIdentity {
  query: string;
  checkOption: '' | 'local' | 'cascaded';
  securityBarrier: boolean;
  securityInvoker: boolean;
}

export function pgViewFormFrom(view: PgView | null, schema: string): PgViewForm {
  if (!view) {
    return { schema, name: '', owner: '', comment: '', query: '', checkOption: '', securityBarrier: false, securityInvoker: false };
  }
  return {
    schema: view.schema,
    name: view.name,
    owner: view.owner,
    comment: view.comment ?? '',
    query: view.definition.trim().replace(/;\s*$/, ''),
    checkOption: view.checkOption === 'local' || view.checkOption === 'cascaded' ? view.checkOption : '',
    securityBarrier: view.securityBarrier,
    securityInvoker: view.securityInvoker,
  };
}

export const pgViewSchema = z.object({
  ...identitySchema,
  query: z.string().refine((v) => v.trim() !== '', 'Enter the SELECT that defines the view.'),
  checkOption: z.enum(['', 'local', 'cascaded']),
  securityBarrier: z.boolean(),
  securityInvoker: z.boolean(),
});

/** Definition and options as one `CREATE [OR REPLACE] VIEW` (it replaces the whole option list). */
function pgViewCreate(form: PgViewForm, name: QualifiedName, orReplace: boolean, server: ServerTarget, comment: string | null): EngineChange {
  return pg({
    object: 'view',
    action: 'create',
    name,
    orReplace,
    query: form.query.trim().replace(/;\s*$/, ''),
    checkOption: form.checkOption === '' ? null : form.checkOption,
    securityBarrier: orReplace || form.securityBarrier ? form.securityBarrier : null,
    // security_invoker exists from PostgreSQL 15; older servers reject the option even when false.
    securityInvoker: pgAtLeast(server, 15) && (orReplace || form.securityInvoker) ? form.securityInvoker : null,
    comment,
  });
}

/** Changes for a PostgreSQL view: definition, comment, then owner, schema and name. */
export function buildPgView(form: PgViewForm, original: PgView | null, database: string | null, server: ServerTarget): EngineChange[] {
  if (!original) {
    const out = [pgViewCreate(form, qn(form.schema, form.name), false, server, textOrNull(form.comment))];
    if (textOrNull(form.owner)) {
      out.push(pg({ object: 'view', action: 'alter', name: qn(form.schema, form.name), operations: [{ op: 'owner', owner: roleSpec(form.owner) }] }));
    }
    return out;
  }
  const out: EngineChange[] = [];
  const name = qn(original.schema, original.name);
  const before = pgViewFormFrom(original, original.schema);
  const definitionChanged = !same(form.query, before.query);
  const optionsChanged =
    form.checkOption !== before.checkOption || form.securityBarrier !== before.securityBarrier || form.securityInvoker !== before.securityInvoker;
  if (definitionChanged) {
    out.push(pgViewCreate(form, name, true, server, null));
  } else if (optionsChanged) {
    const set: { checkOption?: 'local' | 'cascaded'; securityBarrier?: boolean; securityInvoker?: boolean } = {};
    const reset: ('checkOption' | 'securityBarrier' | 'securityInvoker')[] = [];
    if (form.checkOption !== before.checkOption) {
      if (form.checkOption === '') reset.push('checkOption');
      else set.checkOption = form.checkOption;
    }
    if (form.securityBarrier !== before.securityBarrier) set.securityBarrier = form.securityBarrier;
    if (form.securityInvoker !== before.securityInvoker) set.securityInvoker = form.securityInvoker;
    const operations: PgViewOperation[] = [];
    if (Object.keys(set).length > 0) operations.push({ op: 'setOptions', ...set });
    if (reset.length > 0) operations.push({ op: 'resetOptions', options: reset });
    out.push(pg({ object: 'view', action: 'alter', name, operations }));
  }
  if (!same(form.comment, before.comment)) {
    out.push(commentChange(objectRef('view', database, original.schema, original.name), form.comment));
  }
  const ops = identityOps<PgViewOperation & { op: 'owner' | 'setSchema' | 'rename' }>(form, original);
  if (ops.length > 0) out.push(pg({ object: 'view', action: 'alter', name, operations: ops }));
  return out;
}

// ---------------------------------------------------------------------------
// MySQL / MariaDB views
// ---------------------------------------------------------------------------

export type DefinerChoice = 'keep' | 'currentUser' | 'account';

export interface MyDefinerForm {
  definer: DefinerChoice;
  definerUser: string;
  definerHost: string;
}

/** Splits `user@host` as information_schema prints a definer (the host may contain `@`, the user may not). */
export function parseDefiner(text: string | null): { user: string; host: string } | null {
  if (!text) return null;
  const at = text.lastIndexOf('@');
  if (at <= 0) return null;
  const strip = (v: string): string => v.replace(/^[`'"](.*)[`'"]$/, '$1');
  return { user: strip(text.slice(0, at)), host: strip(text.slice(at + 1)) };
}

/** Definer fields from a described definer: keep it unless the user changes it. */
export function definerFormFrom(definer: string | null): MyDefinerForm {
  const parsed = parseDefiner(definer);
  return parsed
    ? { definer: 'keep', definerUser: parsed.user, definerHost: parsed.host }
    : { definer: 'currentUser', definerUser: '', definerHost: '%' };
}

/** The definer clause to send: the described definer is sent back unchanged so recreating keeps it. */
export function definerOf(form: MyDefinerForm): MyDefiner | null {
  if (form.definer === 'currentUser') return { kind: 'currentUser' };
  if (form.definerUser.trim() === '') return null;
  return { kind: 'account', user: form.definerUser.trim(), host: form.definerHost.trim() === '' ? '%' : form.definerHost.trim() };
}

function definerRefine(form: MyDefinerForm, ctx: z.RefinementCtx): void {
  if (form.definer === 'account' && form.definerUser.trim() === '') custom(ctx, ['definerUser'], 'Enter the definer user.');
}

const definerShape = {
  definer: z.enum(['keep', 'currentUser', 'account']),
  definerUser: z.string(),
  definerHost: z.string(),
};

export interface MyViewForm extends MyDefinerForm {
  name: string;
  query: string;
  algorithm: '' | 'undefined' | 'merge' | 'temptable';
  security: '' | 'definer' | 'invoker';
  checkOption: '' | 'local' | 'cascaded';
}

function lowerOrEmpty<T extends string>(value: string | null, allowed: readonly T[]): T | '' {
  const v = (value ?? '').toLowerCase();
  return (allowed as readonly string[]).includes(v) ? (v as T) : '';
}

export function myViewFormFrom(view: MyView | null): MyViewForm {
  if (!view) {
    return { name: '', query: '', algorithm: '', security: '', checkOption: '', definer: 'currentUser', definerUser: '', definerHost: '%' };
  }
  return {
    name: view.name,
    query: (view.definition ?? '').trim(),
    algorithm: lowerOrEmpty(view.algorithm, ['undefined', 'merge', 'temptable'] as const),
    security: lowerOrEmpty(view.security, ['definer', 'invoker'] as const),
    checkOption: lowerOrEmpty(view.checkOption, ['local', 'cascaded'] as const),
    ...definerFormFrom(view.definer),
  };
}

export const myViewSchema = z
  .object({
    name: z.string().refine((v) => v.trim() !== '', 'Enter a name.'),
    query: z.string().refine((v) => v.trim() !== '', 'Enter the SELECT that defines the view.'),
    algorithm: z.enum(['', 'undefined', 'merge', 'temptable']),
    security: z.enum(['', 'definer', 'invoker']),
    checkOption: z.enum(['', 'local', 'cascaded']),
    ...definerShape,
  })
  .superRefine(definerRefine);

/**
 * `CREATE OR REPLACE VIEW` (edit) or `CREATE VIEW` (new). A renamed view
 * is created under the new name and the old one dropped afterwards.
 */
export function buildMyView(form: MyViewForm, original: MyView | null, database: string): EngineChange[] {
  const name = form.name.trim();
  const create = my({
    change: 'createView',
    database,
    name,
    replace: original !== null && original.name === name,
    algorithm: form.algorithm === '' ? null : form.algorithm,
    definer: definerOf(form),
    security: form.security === '' ? null : form.security,
    query: form.query.trim().replace(/;\s*$/, ''),
    checkOption: form.checkOption === '' ? null : form.checkOption,
  });
  if (original && original.name !== name) return [create, my({ change: 'dropView', database, name: original.name })];
  return [create];
}

// ---------------------------------------------------------------------------
// PostgreSQL materialized views
// ---------------------------------------------------------------------------

export interface PgMatViewForm extends PgIdentity {
  query: string;
  withData: boolean;
  tablespace: string;
}

export function pgMatViewFormFrom(view: PgMaterializedView | null, schema: string): PgMatViewForm {
  if (!view) return { schema, name: '', owner: '', comment: '', query: '', withData: true, tablespace: '' };
  return {
    schema: view.schema,
    name: view.name,
    owner: view.owner,
    comment: view.comment ?? '',
    query: view.definition.trim().replace(/;\s*$/, ''),
    withData: view.populated,
    tablespace: view.tablespace ?? '',
  };
}

export const pgMatViewSchema = z.object({
  ...identitySchema,
  query: z.string().refine((v) => v.trim() !== '', 'Enter the SELECT that defines the materialized view.'),
  withData: z.boolean(),
  tablespace: z.string(),
});

/** What saving a materialized view does: an in-place change, or a drop and create (definition changed). */
export interface PgMatViewBuild {
  changes: EngineChange[];
  /** True when the view is dropped and created again. */
  recreates: boolean;
  /** `CREATE INDEX` statements of the dropped view, to run again afterwards (catalog text, not user input). */
  indexes: string[];
}

export function buildPgMatView(form: PgMatViewForm, original: PgMaterializedView | null, database: string | null): PgMatViewBuild {
  const create = (name: QualifiedName, comment: string | null): EngineChange =>
    pg({
      object: 'materializedView',
      action: 'create',
      name,
      query: form.query.trim().replace(/;\s*$/, ''),
      withData: form.withData,
      tablespace: textOrNull(form.tablespace),
      comment,
    });
  if (!original) {
    const changes = [create(qn(form.schema, form.name), textOrNull(form.comment))];
    if (textOrNull(form.owner)) {
      changes.push(
        pg({ object: 'materializedView', action: 'alter', name: qn(form.schema, form.name), operations: [{ op: 'owner', owner: roleSpec(form.owner) }] }),
      );
    }
    return { changes, recreates: false, indexes: [] };
  }
  const before = pgMatViewFormFrom(original, original.schema);
  if (!same(form.query, before.query)) {
    // A materialized view has no OR REPLACE: drop it, create it under its new identity, restore owner and indexes.
    const target = qn(form.schema, form.name);
    const changes: EngineChange[] = [
      pg({ object: 'materializedView', action: 'drop', name: qn(original.schema, original.name), behavior: 'restrict' }),
      create(target, textOrNull(form.comment)),
    ];
    const owner = textOrNull(form.owner);
    if (owner) changes.push(pg({ object: 'materializedView', action: 'alter', name: target, operations: [{ op: 'owner', owner: roleSpec(owner) }] }));
    const indexes = original.indexes.map((i) => retargetIndex(i.definition, original, form));
    return { changes, recreates: true, indexes };
  }
  const changes: EngineChange[] = [];
  const name = qn(original.schema, original.name);
  if (!same(form.tablespace, before.tablespace) && textOrNull(form.tablespace)) {
    changes.push(pg({ object: 'materializedView', action: 'alter', name, operations: [{ op: 'setTablespace', tablespace: form.tablespace.trim() }] }));
  }
  if (!same(form.comment, before.comment)) changes.push(commentChange(objectRef('materializedView', database, original.schema, original.name), form.comment));
  const ops = identityOps<PgMaterializedViewOperation & { op: 'owner' | 'setSchema' | 'rename' }>(form, original);
  if (ops.length > 0) changes.push(pg({ object: 'materializedView', action: 'alter', name, operations: ops }));
  if (form.withData !== before.withData) {
    changes.push(pg({ object: 'materializedView', action: 'refresh', name: qn(form.schema, form.name), withData: form.withData }));
  }
  return { changes, recreates: false, indexes: [] };
}

/**
 * An index definition of the old view pointed at the recreated one. Only
 * the `ON <schema>.<name>` target changes; `pg_get_indexdef` always prints
 * it schema-qualified with quoted identifiers where needed.
 */
function retargetIndex(definition: string, original: PgMaterializedView, form: PgMatViewForm): string {
  if (same(form.schema, original.schema) && same(form.name, original.name)) return definition;
  const quote = (v: string): string => (/^[a-z_][a-z0-9_$]*$/.test(v) ? v : `"${v.replace(/"/g, '""')}"`);
  const from = ` ON ${quote(original.schema)}.${quote(original.name)} `;
  const to = ` ON ${quote(form.schema.trim())}.${quote(form.name.trim())} `;
  return definition.replace(from, to);
}

/** `REFRESH MATERIALIZED VIEW` with the options the user picked. */
export function buildPgMatViewRefresh(view: PgMaterializedView, concurrently: boolean, withData: boolean): EngineChange {
  return pg({
    object: 'materializedView',
    action: 'refresh',
    name: qn(view.schema, view.name),
    concurrently,
    hasUniqueIndex: view.refreshConcurrently,
    withData,
  });
}

/** Why concurrent refresh is not possible, or `null` when it is. */
export function concurrentRefreshProblem(view: PgMaterializedView, withData: boolean): string | null {
  if (!withData) return 'WITH NO DATA cannot run concurrently.';
  if (!view.populated) return 'The view is not populated yet; run a plain refresh first.';
  if (!view.indexes.some((i) => i.unique && i.predicate === null && i.keys.every((k) => !k.expression))) {
    return 'Concurrent refresh needs a unique index on plain columns without a WHERE clause.';
  }
  return null;
}

// ---------------------------------------------------------------------------
// PostgreSQL functions and procedures
// ---------------------------------------------------------------------------

export interface PgArgForm {
  mode: PgArgMode;
  name: string;
  dataType: string;
  default: string;
}

export type PgBodyKind = 'definition' | 'return' | 'atomic' | 'object';

export interface PgRoutineForm extends PgIdentity {
  routine: 'function' | 'procedure';
  args: PgArgForm[];
  /** `type`, `table` (RETURNS TABLE) or `none` (procedures). */
  returnsKind: 'type' | 'table' | 'none';
  returnType: string;
  setof: boolean;
  returnColumns: { name: string; dataType: string }[];
  language: string;
  bodyKind: PgBodyKind;
  /** Source text (`definition`), expression (`return`), statements (`atomic`) or symbol (`object`). */
  body: string;
  /** Library file for `object` bodies. */
  file: string;
  volatility: PgVolatility;
  strict: boolean;
  securityDefiner: boolean;
  leakproof: boolean;
  parallel: PgParallel;
  cost: string;
  rows: string;
  window: boolean;
  /** `name = value` lines. */
  settings: string;
}

const ARG_MODES: Record<string, PgArgMode> = { in: 'in', out: 'out', inOut: 'inOut', variadic: 'variadic' };

/** The SQL-standard body of a function from `pg_get_functiondef` text: `RETURN expr` or `BEGIN ATOMIC ... END`. */
export function sqlStandardBody(definition: string): { kind: 'return' | 'atomic'; body: string } | null {
  const atomic = /\n\s*BEGIN ATOMIC\b/i.exec(definition);
  if (atomic) {
    const rest = definition.slice(atomic.index + atomic[0].length);
    const end = rest.search(/\bEND\s*;?\s*$/i);
    const inner = end < 0 ? rest : rest.slice(0, end);
    const statements = splitStatements(inner, 'postgres').map((s) => s.text);
    return { kind: 'atomic', body: statements.map((s) => `${s};`).join('\n') };
  }
  const ret = /\n\s*RETURN\s/i.exec(definition);
  if (ret) return { kind: 'return', body: definition.slice(ret.index + ret[0].length).trim().replace(/;\s*$/, '') };
  return null;
}

/** RETURNS TABLE columns are the `table` arguments of the described function. */
export function pgRoutineFormFrom(fn: PgFunction | null, kind: 'function' | 'procedure', schema: string): PgRoutineForm {
  if (!fn) {
    return {
      schema,
      name: '',
      owner: '',
      comment: '',
      routine: kind,
      args: [],
      returnsKind: kind === 'procedure' ? 'none' : 'type',
      returnType: kind === 'procedure' ? '' : 'void',
      setof: false,
      returnColumns: [],
      language: 'plpgsql',
      bodyKind: 'definition',
      body: kind === 'procedure' ? 'BEGIN\n\nEND;' : 'BEGIN\n  RETURN;\nEND;',
      file: '',
      volatility: 'volatile',
      strict: false,
      securityDefiner: false,
      leakproof: false,
      parallel: 'unsafe',
      cost: '',
      rows: '',
      window: false,
      settings: '',
    };
  }
  const tableColumns = fn.arguments.filter((a) => a.mode === 'table');
  const standard = fn.sqlStandardBody ? sqlStandardBody(fn.definition) : null;
  let bodyKind: PgBodyKind = 'definition';
  let body = fn.source ?? '';
  let file = '';
  if (standard) {
    bodyKind = standard.kind;
    body = standard.body;
  } else if (fn.library !== null) {
    bodyKind = 'object';
    file = fn.library;
    body = fn.source ?? '';
  }
  return {
    schema: fn.schema,
    name: fn.name,
    owner: fn.meta.owner ?? '',
    comment: fn.meta.comment ?? '',
    routine: kind,
    args: fn.arguments
      .filter((a) => a.mode !== 'table')
      .map((a) => ({ mode: ARG_MODES[a.mode] ?? 'in', name: a.name ?? '', dataType: a.dataType, default: a.default ?? '' })),
    returnsKind: kind === 'procedure' ? 'none' : tableColumns.length > 0 ? 'table' : 'type',
    returnType: fn.returnType ?? '',
    setof: fn.returnsSet && tableColumns.length === 0,
    returnColumns: tableColumns.map((a) => ({ name: a.name ?? '', dataType: a.dataType })),
    language: fn.language,
    bodyKind,
    body,
    file,
    volatility: fn.volatility,
    strict: fn.strict,
    securityDefiner: fn.securityDefiner,
    leakproof: fn.leakproof,
    parallel: fn.parallel,
    cost: String(fn.cost),
    rows: fn.rows === null ? '' : String(fn.rows),
    window: fn.window,
    settings: fn.config.map((c) => {
      const s = parseConfigEntry(c);
      return `${s.name} = ${s.values.join(', ')}`;
    }).join('\n'),
  };
}

/** `name = value` lines to settings; list parameters (`search_path`) split on commas. */
export function parseSettings(text: string): ConfigSetting[] {
  const out: ConfigSetting[] = [];
  for (const line of text.split('\n')) {
    const t = line.trim();
    if (t === '') continue;
    const eq = t.indexOf('=');
    const name = (eq < 0 ? t : t.slice(0, eq)).trim();
    const raw = eq < 0 ? '' : t.slice(eq + 1).trim();
    const values = name === 'search_path' ? raw.split(',').map((v) => v.trim()).filter((v) => v !== '') : [raw];
    out.push({ name, values });
  }
  return out;
}

const decimalText = (v: string): boolean => v.trim() === '' || (/^\d+(\.\d+)?$/.test(v.trim()) && Number(v) > 0);

export const pgRoutineSchema = z
  .object({
    ...identitySchema,
    routine: z.enum(['function', 'procedure']),
    args: z.array(z.object({ mode: z.enum(['in', 'out', 'inOut', 'variadic']), name: z.string(), dataType: z.string(), default: z.string() })),
    returnsKind: z.enum(['type', 'table', 'none']),
    returnType: z.string(),
    setof: z.boolean(),
    returnColumns: z.array(z.object({ name: z.string(), dataType: z.string() })),
    language: z.string().refine((v) => v.trim() !== '', 'Enter the language.'),
    bodyKind: z.enum(['definition', 'return', 'atomic', 'object']),
    body: z.string(),
    file: z.string(),
    volatility: z.enum(['immutable', 'stable', 'volatile']),
    strict: z.boolean(),
    securityDefiner: z.boolean(),
    leakproof: z.boolean(),
    parallel: z.enum(['safe', 'restricted', 'unsafe']),
    cost: z.string().refine(decimalText, 'Enter a positive number.'),
    rows: z.string().refine(decimalText, 'Enter a positive number.'),
    window: z.boolean(),
    settings: z.string(),
  })
  .superRefine((v, ctx) => {
    v.args.forEach((a, i) => {
      if (a.dataType.trim() === '') custom(ctx, ['args', i, 'dataType'], 'Enter the argument type.');
      if (a.default.trim() !== '' && a.mode === 'out') custom(ctx, ['args', i, 'default'], 'OUT arguments have no default.');
    });
    const firstDefault = v.args.findIndex((a) => a.mode !== 'out' && a.default.trim() !== '');
    if (firstDefault >= 0) {
      v.args.forEach((a, i) => {
        if (i > firstDefault && (a.mode === 'in' || a.mode === 'inOut') && a.default.trim() === '') {
          custom(ctx, ['args', i, 'default'], 'Arguments after one with a default need a default too.');
        }
      });
    }
    if (v.routine === 'function') {
      if (v.returnsKind === 'type' && v.returnType.trim() === '') custom(ctx, ['returnType'], 'Enter the return type.');
      if (v.returnsKind === 'table') {
        if (v.returnColumns.length === 0) custom(ctx, ['returnColumns'], 'Add at least one column.');
        v.returnColumns.forEach((c, i) => {
          if (c.name.trim() === '') custom(ctx, ['returnColumns', i, 'name'], 'Enter the column name.');
          if (c.dataType.trim() === '') custom(ctx, ['returnColumns', i, 'dataType'], 'Enter the column type.');
        });
      }
    }
    if (v.bodyKind === 'object' && v.file.trim() === '') custom(ctx, ['file'], 'Enter the library file.');
    if (v.body.trim() === '') custom(ctx, ['body'], v.bodyKind === 'object' ? 'Enter the link symbol.' : 'Enter the body.');
    for (const s of parseSettings(v.settings)) {
      if (s.name === '' || s.values.length === 0) {
        custom(ctx, ['settings'], 'Write one setting per line as name = value.');
        break;
      }
    }
  });

function routineBody(form: PgRoutineForm): PgRoutineBody {
  switch (form.bodyKind) {
    case 'return':
      return { kind: 'return', expression: form.body.trim().replace(/;\s*$/, '') };
    case 'atomic':
      return { kind: 'atomic', statements: splitStatements(form.body, 'postgres').map((s) => s.text.replace(/;\s*$/, '')) };
    case 'object':
      return { kind: 'object', file: form.file.trim(), symbol: form.body.trim() };
    case 'definition':
      return { kind: 'definition', source: form.body };
  }
}

function routineArgs(form: PgRoutineForm): PgRoutineArg[] {
  return form.args.map((a) => ({
    mode: a.mode,
    name: textOrNull(a.name),
    dataType: a.dataType.trim(),
    default: textOrNull(a.default),
  }));
}

/** The identity signature of the form's arguments (types of IN, INOUT and VARIADIC arguments). */
export function identitySignature(args: readonly { mode: PgArgMode | string; dataType: string }[]): string {
  return args
    .filter((a) => a.mode !== 'out' && a.mode !== 'table')
    .map((a) => a.dataType.trim().toLowerCase().replace(/\s+/g, ' '))
    .join(', ');
}

/** True when the edit changes the argument types: saving creates a new overload instead of replacing the routine. */
export function createsOverload(form: PgRoutineForm, original: PgFunction): boolean {
  return identitySignature(form.args) !== identitySignature(original.arguments);
}

function createRoutine(form: PgRoutineForm, name: QualifiedName, orReplace: boolean): EngineChange {
  const cost = textOrNull(form.cost);
  const rows = textOrNull(form.rows);
  const isFunction = form.routine === 'function';
  return pg({
    object: 'routine',
    action: 'create',
    routine: form.routine,
    name,
    orReplace,
    arguments: routineArgs(form),
    returns: !isFunction
      ? null
      : form.returnsKind === 'table'
        ? { kind: 'table', columns: form.returnColumns.map((c) => ({ name: c.name.trim(), dataType: c.dataType.trim() })) }
        : { kind: 'type', dataType: form.returnType.trim(), setof: form.setof },
    language: form.language.trim(),
    body: routineBody(form),
    volatility: isFunction ? form.volatility : null,
    strict: isFunction ? form.strict : null,
    securityDefiner: form.securityDefiner,
    leakproof: isFunction ? form.leakproof : null,
    parallel: isFunction ? form.parallel : null,
    cost: isFunction && cost !== null ? Number(cost) : null,
    rows: isFunction && rows !== null && (form.setof || form.returnsKind === 'table') ? Number(rows) : null,
    window: isFunction && form.window,
    settings: parseSettings(form.settings),
    comment: null,
  });
}

/**
 * Changes for a PostgreSQL routine. A changed body, language, arguments or
 * return type is a `CREATE OR REPLACE`; attribute-only edits are `ALTER`s.
 */
export function buildPgRoutine(form: PgRoutineForm, original: PgFunction | null, database: string | null): EngineChange[] {
  const kind: ObjectKind = form.routine;
  if (!original) {
    const out = [createRoutine(form, qn(form.schema, form.name), false)];
    const signature = identitySignature(form.args);
    if (textOrNull(form.comment)) {
      out.push(commentChange(objectRef(kind, database, form.schema.trim(), form.name.trim(), { signature }), form.comment));
    }
    if (textOrNull(form.owner)) {
      out.push(
        pg({ object: 'routine', action: 'alter', routine: form.routine, name: qn(form.schema, form.name), signature, operations: [{ op: 'owner', owner: roleSpec(form.owner) }] }),
      );
    }
    return out;
  }
  const before = pgRoutineFormFrom(original, form.routine, original.schema);
  const name = qn(original.schema, original.name);
  const out: EngineChange[] = [];
  const replaced =
    !same(form.body, before.body) ||
    !same(form.file, before.file) ||
    form.bodyKind !== before.bodyKind ||
    !same(form.language, before.language) ||
    JSON.stringify(form.args) !== JSON.stringify(before.args) ||
    form.returnsKind !== before.returnsKind ||
    !same(form.returnType, before.returnType) ||
    form.setof !== before.setof ||
    JSON.stringify(form.returnColumns) !== JSON.stringify(before.returnColumns) ||
    form.window !== before.window;
  const overload = createsOverload(form, original);
  if (replaced) {
    out.push(createRoutine(form, name, !overload));
  } else {
    const ops: PgRoutineOperation[] = [];
    if (form.routine === 'function') {
      if (form.volatility !== before.volatility) ops.push({ op: 'volatility', volatility: form.volatility });
      if (form.strict !== before.strict) ops.push({ op: 'strict', strict: form.strict });
      if (form.leakproof !== before.leakproof) ops.push({ op: 'leakproof', leakproof: form.leakproof });
      if (form.parallel !== before.parallel) ops.push({ op: 'parallel', parallel: form.parallel });
      if (!same(form.cost, before.cost) && textOrNull(form.cost)) ops.push({ op: 'cost', cost: Number(form.cost) });
      if (!same(form.rows, before.rows) && textOrNull(form.rows)) ops.push({ op: 'rows', rows: Number(form.rows) });
    }
    if (form.securityDefiner !== before.securityDefiner) ops.push({ op: 'security', definer: form.securityDefiner });
    if (!same(form.settings, before.settings)) {
      const now = parseSettings(form.settings);
      const was = parseSettings(before.settings);
      for (const s of was) if (!now.some((n) => n.name === s.name)) ops.push({ op: 'reset', name: s.name });
      for (const s of now) {
        const old = was.find((w) => w.name === s.name);
        if (!old || JSON.stringify(old.values) !== JSON.stringify(s.values)) ops.push({ op: 'set', setting: s });
      }
    }
    if (ops.length > 0) out.push(pg({ object: 'routine', action: 'alter', routine: form.routine, name, signature: original.signature, operations: ops }));
  }
  // A new overload is a new object: comment and identity changes apply to it, under its own signature.
  const signature = overload ? identitySignature(form.args) : original.signature;
  if (!same(form.comment, before.comment) || (overload && textOrNull(form.comment))) {
    out.push(commentChange(objectRef(kind, database, original.schema, original.name, { signature }), form.comment));
  }
  const ops = identityOps<PgRoutineOperation & { op: 'owner' | 'setSchema' | 'rename' }>(form, overload ? { ...original, owner: '' } : { ...original, owner: original.meta.owner });
  if (ops.length > 0) out.push(pg({ object: 'routine', action: 'alter', routine: form.routine, name, signature, operations: ops }));
  return out;
}

// ---------------------------------------------------------------------------
// MySQL / MariaDB data types, functions and procedures
// ---------------------------------------------------------------------------

const MY_TYPE_ALIASES: Record<string, MyTypeName> = {
  integer: 'int',
  bool: 'boolean',
  dec: 'decimal',
  numeric: 'decimal',
  fixed: 'decimal',
  real: 'double',
  'double precision': 'double',
  'character varying': 'varchar',
  character: 'char',
  'national varchar': 'varchar',
  'national char': 'char',
};

const MY_TYPES: readonly MyTypeName[] = [
  'tinyint', 'smallint', 'mediumint', 'int', 'bigint', 'decimal', 'float', 'double', 'bit', 'boolean', 'char', 'varchar', 'binary',
  'varbinary', 'tinytext', 'text', 'mediumtext', 'longtext', 'tinyblob', 'blob', 'mediumblob', 'longblob', 'enum', 'set', 'date',
  'time', 'datetime', 'timestamp', 'year', 'json', 'geometry', 'point', 'linestring', 'polygon', 'multipoint', 'multilinestring',
  'multipolygon', 'geometrycollection', 'inet4', 'inet6', 'uuid', 'vector',
];

/** A string literal list `'a','b''c'` as in ENUM and SET types. */
function parseLiteralList(text: string): string[] | null {
  const out: string[] = [];
  let i = 0;
  while (i < text.length) {
    const c = text[i] as string;
    if (/[\s,]/.test(c)) {
      i += 1;
      continue;
    }
    if (c !== "'") return null;
    let value = '';
    let j = i + 1;
    let closed = false;
    while (j < text.length) {
      const d = text[j] as string;
      if (d === '\\' && j + 1 < text.length) {
        value += text[j + 1];
        j += 2;
      } else if (d === "'" && text[j + 1] === "'") {
        value += "'";
        j += 2;
      } else if (d === "'") {
        closed = true;
        j += 1;
        break;
      } else {
        value += d;
        j += 1;
      }
    }
    if (!closed) return null;
    out.push(value);
    i = j;
  }
  return out;
}

/**
 * Parses a MySQL/MariaDB column type as written or as `DTD_IDENTIFIER`
 * prints it: `decimal(10,2) unsigned`, `varchar(64) CHARACTER SET utf8mb4
 * COLLATE utf8mb4_bin`, `enum('a','b')`. Returns a message when it is not a
 * type the editors can send.
 */
export function parseMyDataType(text: string): MyDataType | string {
  let rest = text.trim();
  if (rest === '') return 'Enter a data type.';
  const head = /^([A-Za-z][A-Za-z ]*?)\s*(?=\(|$|\s+(unsigned|signed|zerofill|character|charset|collate)\b)/i.exec(rest);
  if (!head) return `"${text.trim()}" is not a data type.`;
  const word = (head[1] as string).toLowerCase().replace(/\s+/g, ' ');
  const name = (MY_TYPE_ALIASES[word] ?? word) as MyTypeName;
  if (!MY_TYPES.includes(name)) return `${word.toUpperCase()} is not a type this editor knows.`;
  rest = rest.slice(head[0].length).trim();
  const out: MyDataType = { name };
  if (rest.startsWith('(')) {
    let depth = 0;
    let end = -1;
    let quoted = false;
    for (let i = 0; i < rest.length; i += 1) {
      const c = rest[i];
      if (c === "'" ) quoted = !quoted;
      if (quoted) continue;
      if (c === '(') depth += 1;
      if (c === ')') {
        depth -= 1;
        if (depth === 0) {
          end = i;
          break;
        }
      }
    }
    if (end < 0) return 'The type has an unclosed parenthesis.';
    const inner = rest.slice(1, end);
    rest = rest.slice(end + 1).trim();
    if (name === 'enum' || name === 'set') {
      const values = parseLiteralList(inner);
      if (!values || values.length === 0) return `${name.toUpperCase()} needs a list of quoted values.`;
      out.values = values;
    } else {
      const m = /^\s*(\d+)\s*(?:,\s*(\d+)\s*)?$/.exec(inner);
      if (!m) return `"(${inner})" is not a valid length.`;
      out.length = Number(m[1]);
      if (m[2] !== undefined) out.scale = Number(m[2]);
    }
  } else if (name === 'enum' || name === 'set') {
    return `${name.toUpperCase()} needs a list of quoted values.`;
  }
  while (rest !== '') {
    const m = /^(unsigned|signed|zerofill|(?:character set|charset)\s+(\w+)|collate\s+(\w+))\s*/i.exec(rest);
    if (!m) return `"${rest}" is not understood.`;
    const kw = (m[1] as string).toLowerCase();
    if (kw === 'unsigned') out.unsigned = true;
    else if (kw === 'zerofill') out.zerofill = true;
    else if (m[2] !== undefined) out.charset = m[2];
    else if (m[3] !== undefined) out.collation = m[3];
    rest = rest.slice(m[0].length);
  }
  return out;
}

export interface MyParamForm {
  mode: 'in' | 'out' | 'inout';
  name: string;
  dataType: string;
}

export interface MyRoutineForm extends MyDefinerForm {
  routine: 'procedure' | 'function';
  name: string;
  params: MyParamForm[];
  returns: string;
  deterministic: boolean;
  dataAccess: '' | 'containsSql' | 'noSql' | 'readsSqlData' | 'modifiesSqlData';
  security: '' | 'definer' | 'invoker';
  comment: string;
  body: string;
}

const DATA_ACCESS: Record<string, MyRoutineForm['dataAccess']> = {
  'CONTAINS SQL': 'containsSql',
  'NO SQL': 'noSql',
  'READS SQL DATA': 'readsSqlData',
  'MODIFIES SQL DATA': 'modifiesSqlData',
};

export function myRoutineFormFrom(routine: MyRoutine | null, kind: 'procedure' | 'function'): MyRoutineForm {
  if (!routine) {
    return {
      routine: kind,
      name: '',
      params: [],
      returns: kind === 'function' ? 'int' : '',
      deterministic: false,
      dataAccess: '',
      security: '',
      comment: '',
      body: 'BEGIN\n\nEND',
      definer: 'currentUser',
      definerUser: '',
      definerHost: '%',
    };
  }
  return {
    routine: kind,
    name: routine.name,
    params: routine.parameters.map((p) => ({
      mode: (p.mode ?? 'IN').toLowerCase() === 'out' ? 'out' : (p.mode ?? 'IN').toLowerCase() === 'inout' ? 'inout' : 'in',
      name: p.name ?? '',
      dataType: p.dataType,
    })),
    returns: routine.returns ?? '',
    deterministic: routine.deterministic,
    dataAccess: DATA_ACCESS[(routine.dataAccess ?? '').toUpperCase()] ?? '',
    security: lowerOrEmpty(routine.security, ['definer', 'invoker'] as const),
    comment: routine.comment ?? '',
    body: routine.body ?? '',
    ...definerFormFrom(routine.definer),
  };
}

export const myRoutineSchema = z
  .object({
    routine: z.enum(['procedure', 'function']),
    name: z.string().refine((v) => v.trim() !== '', 'Enter a name.'),
    params: z.array(z.object({ mode: z.enum(['in', 'out', 'inout']), name: z.string(), dataType: z.string() })),
    returns: z.string(),
    deterministic: z.boolean(),
    dataAccess: z.enum(['', 'containsSql', 'noSql', 'readsSqlData', 'modifiesSqlData']),
    security: z.enum(['', 'definer', 'invoker']),
    comment: z.string(),
    body: z.string().refine((v) => v.trim() !== '', 'Enter the body.'),
    ...definerShape,
  })
  .superRefine((v, ctx) => {
    definerRefine(v, ctx);
    v.params.forEach((p, i) => {
      if (p.name.trim() === '') custom(ctx, ['params', i, 'name'], 'Enter the parameter name.');
      const type = parseMyDataType(p.dataType);
      if (typeof type === 'string') custom(ctx, ['params', i, 'dataType'], type);
    });
    if (v.routine === 'function') {
      const type = parseMyDataType(v.returns);
      if (typeof type === 'string') custom(ctx, ['returns'], type);
    }
  });

const myRoutineBody = (body: string): string => body.trim().replace(/;\s*$/, '');

/**
 * True when `form` differs from `original` at most in what `ALTER PROCEDURE|FUNCTION`
 * changes in place: comment, data access and SQL security. Name, parameters,
 * return type, determinism, definer and body are the same.
 */
function myRoutineAlterOnly(form: MyRoutineForm, original: MyRoutine, engine: Engine): boolean {
  const before = myRoutineFormFrom(original, form.routine);
  // MySQL ignores `COMMENT ''` in ALTER: removing a comment needs the routine created again.
  if (engine !== 'mariadb' && form.comment.trim() === '' && before.comment.trim() !== '') return false;
  const params = (f: MyRoutineForm): string => JSON.stringify(f.params.map((p) => [f.routine === 'function' ? 'in' : p.mode, p.name.trim(), p.dataType.trim()]));
  return (
    form.name.trim() === original.name &&
    params(form) === params(before) &&
    form.returns.trim() === before.returns.trim() &&
    form.deterministic === before.deterministic &&
    myRoutineBody(form.body) === myRoutineBody(before.body) &&
    form.definer === before.definer &&
    form.definerUser.trim() === before.definerUser.trim() &&
    form.definerHost.trim() === before.definerHost.trim()
  );
}

/**
 * Changes for a MySQL/MariaDB routine. A change of only the comment, data
 * access or SQL security is `ALTER PROCEDURE|FUNCTION`, which keeps the
 * routine and its EXECUTE / ALTER ROUTINE grants (no change at all when
 * nothing differs); removing the comment is the exception on MySQL, which
 * ignores `COMMENT ''` there. Anything else is `CREATE [OR REPLACE]` (on
 * MySQL a drop and create, which loses the grants); a renamed routine is
 * created under the new name and the old one dropped.
 */
export function buildMyRoutine(form: MyRoutineForm, original: MyRoutine | null, database: string, engine: Engine): EngineChange[] {
  if (original && myRoutineAlterOnly(form, original, engine)) {
    const before = myRoutineFormFrom(original, form.routine);
    const alter: MyAlterRoutine = { database, name: original.name, routine: form.routine };
    // Stored as typed (spaces and line breaks kept); all whitespace removes it.
    // Sent only when the comment really changed: a stored all-whitespace
    // comment and an empty field are the same, and so are CRLF and LF.
    const typed = form.comment.trim() === '' ? '' : form.comment;
    const stored = before.comment.trim() === '' ? '' : before.comment;
    if (sameLineEndings(typed) !== sameLineEndings(stored)) alter.comment = typed;
    // Blank selects mean the server defaults, which ALTER has to name.
    if (form.dataAccess !== before.dataAccess) alter.dataAccess = form.dataAccess === '' ? 'containsSql' : form.dataAccess;
    if (form.security !== before.security) alter.security = form.security === '' ? 'definer' : form.security;
    const changed = alter.comment !== undefined || alter.dataAccess !== undefined || alter.security !== undefined;
    return changed ? [my({ change: 'alterRoutine', ...alter })] : [];
  }
  const name = form.name.trim();
  const isFunction = form.routine === 'function';
  const create = my({
    change: 'createRoutine',
    database,
    name,
    routine: form.routine,
    replace: original !== null && original.name === name,
    definer: definerOf(form),
    parameters: form.params.map((p) => ({
      // Function parameters take no IN/OUT/INOUT.
      mode: isFunction ? null : p.mode,
      name: p.name.trim(),
      dataType: parseMyDataType(p.dataType) as MyDataType,
    })),
    returns: isFunction ? (parseMyDataType(form.returns) as MyDataType) : null,
    characteristics: {
      // As typed, like the in-place change; all whitespace means none.
      comment: form.comment.trim() === '' ? null : form.comment,
      deterministic: form.deterministic,
      dataAccess: form.dataAccess === '' ? null : form.dataAccess,
      security: form.security === '' ? null : form.security,
    },
    body: myRoutineBody(form.body),
  });
  if (original && original.name !== name) {
    return [create, my({ change: 'dropRoutine', database, name: original.name, routine: form.routine })];
  }
  return [create];
}

// ---------------------------------------------------------------------------
// PostgreSQL triggers
// ---------------------------------------------------------------------------

export type PgTriggerStateForm = 'enabled' | 'always' | 'replica' | 'disabled';

export interface PgTriggerForm {
  name: string;
  timing: 'before' | 'after' | 'insteadOf';
  level: 'row' | 'statement';
  insert: boolean;
  update: boolean;
  updateColumns: string[];
  delete: boolean;
  truncate: boolean;
  condition: string;
  /** The trigger function, `schema.name` (quoted parts allowed). */
  function: string;
  arguments: string[];
  constraint: boolean;
  deferrable: boolean;
  initiallyDeferred: boolean;
  oldTable: string;
  newTable: string;
  state: PgTriggerStateForm;
  comment: string;
}

const STATE_FROM_ENABLED: Record<string, PgTriggerStateForm> = { origin: 'enabled', always: 'always', replica: 'replica', disabled: 'disabled' };

export function pgTriggerFormFrom(trigger: PgTrigger | null): PgTriggerForm {
  if (!trigger) {
    return {
      name: '',
      timing: 'before',
      level: 'row',
      insert: false,
      update: true,
      updateColumns: [],
      delete: false,
      truncate: false,
      condition: '',
      function: '',
      arguments: [],
      constraint: false,
      deferrable: false,
      initiallyDeferred: false,
      oldTable: '',
      newTable: '',
      state: 'enabled',
      comment: '',
    };
  }
  const fn = trigger.function.replace(/\(.*\)$/, '');
  return {
    name: trigger.name,
    timing: trigger.timing === 'insteadOf' ? 'insteadOf' : trigger.timing === 'after' ? 'after' : 'before',
    level: trigger.level === 'statement' ? 'statement' : 'row',
    insert: trigger.events.includes('insert'),
    update: trigger.events.includes('update'),
    updateColumns: [...trigger.updateColumns],
    delete: trigger.events.includes('delete'),
    truncate: trigger.events.includes('truncate'),
    condition: trigger.condition ?? '',
    function: fn,
    arguments: triggerArguments(trigger.definition),
    constraint: trigger.constraintTrigger,
    deferrable: trigger.deferrable,
    initiallyDeferred: trigger.deferred,
    oldTable: trigger.oldTable ?? '',
    newTable: trigger.newTable ?? '',
    state: STATE_FROM_ENABLED[trigger.enabled] ?? 'enabled',
    comment: trigger.comment ?? '',
  };
}

/** Problems of a trigger form, by field; `onView` allows INSTEAD OF. */
export function pgTriggerSchema(onView: boolean) {
  return z
    .object({
      name: z.string().refine((v) => v.trim() !== '', 'Enter a name.'),
      timing: z.enum(['before', 'after', 'insteadOf']),
      level: z.enum(['row', 'statement']),
      insert: z.boolean(),
      update: z.boolean(),
      updateColumns: z.array(z.string()),
      delete: z.boolean(),
      truncate: z.boolean(),
      condition: z.string(),
      function: z.string().refine((v) => v.trim() !== '', 'Choose the trigger function.'),
      arguments: z.array(z.string()),
      constraint: z.boolean(),
      deferrable: z.boolean(),
      initiallyDeferred: z.boolean(),
      oldTable: z.string(),
      newTable: z.string(),
      state: z.enum(['enabled', 'always', 'replica', 'disabled']),
      comment: z.string(),
    })
    .superRefine((v, ctx) => {
      if (!v.insert && !v.update && !v.delete && !v.truncate) custom(ctx, ['events'], 'Pick at least one event.');
      if (v.truncate && v.level === 'row') custom(ctx, ['events'], 'TRUNCATE triggers fire FOR EACH STATEMENT only.');
      if (v.timing === 'insteadOf') {
        if (!onView) custom(ctx, ['timing'], 'INSTEAD OF triggers exist only on views.');
        if (v.level !== 'row') custom(ctx, ['level'], 'INSTEAD OF triggers fire FOR EACH ROW.');
        if (v.condition.trim() !== '') custom(ctx, ['condition'], 'INSTEAD OF triggers take no WHEN condition.');
        if (v.updateColumns.length > 0) custom(ctx, ['events'], 'INSTEAD OF UPDATE takes no column list.');
      } else if (onView && v.level === 'row') {
        custom(ctx, ['level'], 'Row-level triggers on views must be INSTEAD OF.');
      }
      if (v.condition.trim() !== '' && v.level === 'row') {
        if (v.insert && /\bOLD\b/i.test(v.condition)) custom(ctx, ['condition'], 'The condition references OLD, which INSERT does not have.');
        if (v.delete && /\bNEW\b/i.test(v.condition)) custom(ctx, ['condition'], 'The condition references NEW, which DELETE does not have.');
      }
      if (v.condition.trim() !== '' && v.level === 'statement' && /\b(OLD|NEW)\b/i.test(v.condition)) {
        custom(ctx, ['condition'], 'Statement-level conditions cannot reference OLD or NEW.');
      }
      if (v.constraint) {
        if (v.timing !== 'after' || v.level !== 'row') custom(ctx, ['constraint'], 'Constraint triggers are AFTER ... FOR EACH ROW.');
      } else if (v.deferrable || v.initiallyDeferred) {
        custom(ctx, ['deferrable'], 'Only constraint triggers can be deferrable.');
      }
      if (v.initiallyDeferred && !v.deferrable) custom(ctx, ['initiallyDeferred'], 'INITIALLY DEFERRED needs DEFERRABLE.');
      if (v.oldTable.trim() !== '' || v.newTable.trim() !== '') {
        if (v.timing !== 'after') custom(ctx, ['oldTable'], 'Transition tables need an AFTER trigger.');
        if (v.oldTable.trim() !== '' && !v.update && !v.delete) custom(ctx, ['oldTable'], 'OLD TABLE needs UPDATE or DELETE.');
        if (v.newTable.trim() !== '' && !v.update && !v.insert) custom(ctx, ['newTable'], 'NEW TABLE needs INSERT or UPDATE.');
        if (v.updateColumns.length > 0) custom(ctx, ['events'], 'Transition tables cannot be used with UPDATE OF columns.');
      }
    });
}

function triggerEvents(form: PgTriggerForm): PgTriggerEvent[] {
  const out: PgTriggerEvent[] = [];
  if (form.insert) out.push({ kind: 'insert' });
  if (form.update) out.push({ kind: 'update', columns: form.updateColumns.map((c) => c.trim()).filter((c) => c !== '') });
  if (form.delete) out.push({ kind: 'delete' });
  if (form.truncate) out.push({ kind: 'truncate' });
  return out;
}

function definitionKey(form: PgTriggerForm): string {
  return JSON.stringify([
    form.timing,
    form.level,
    triggerEvents(form),
    form.condition.trim(),
    form.function.trim(),
    form.arguments,
    form.constraint,
    form.deferrable,
    form.initiallyDeferred,
    form.oldTable.trim(),
    form.newTable.trim(),
  ]);
}

const FIRING: Record<PgTriggerStateForm, FiringState> = { enabled: 'enabled', always: 'always', replica: 'replica', disabled: 'disabled' };

/**
 * Changes for a PostgreSQL trigger on `table`. A changed definition is
 * `CREATE OR REPLACE TRIGGER` (PostgreSQL 14+, not for constraint
 * triggers), otherwise drop and create; then state, comment and rename.
 */
export function buildPgTrigger(
  form: PgTriggerForm,
  original: PgTrigger | null,
  table: { schema: string; name: string; kind: ObjectKind },
  database: string | null,
  server: ServerTarget,
): EngineChange[] {
  const tableName = qn(table.schema, table.name);
  const fn = parseQualified(form.function);
  const create = (name: string, orReplace: boolean): EngineChange =>
    pg({
      object: 'trigger',
      action: 'create',
      name,
      table: tableName,
      orReplace,
      constraint: form.constraint,
      timing: form.timing,
      events: triggerEvents(form),
      level: form.level,
      oldTable: textOrNull(form.oldTable),
      newTable: textOrNull(form.newTable),
      condition: textOrNull(form.condition),
      function: { schema: fn.schema, name: fn.name },
      arguments: form.arguments,
      deferrable: form.deferrable,
      initiallyDeferred: form.initiallyDeferred,
      comment: null,
    });
  const out: EngineChange[] = [];
  const name = form.name.trim();
  const ref = (n: string): ObjectRef => objectRef('trigger', database, table.schema, n, { parent: table.name });
  if (!original) {
    out.push(create(name, false));
    if (form.state !== 'enabled') out.push(pg({ object: 'trigger', action: 'setState', name, table: tableName, state: FIRING[form.state] }));
    if (textOrNull(form.comment)) out.push(commentChange(ref(name), form.comment));
    return out;
  }
  const before = pgTriggerFormFrom(original);
  let current = original.name;
  let recreated = false;
  if (definitionKey(form) !== definitionKey(before)) {
    if (pgAtLeast(server, 14) && !form.constraint && !before.constraint) {
      out.push(create(original.name, true));
    } else {
      out.push(pg({ object: 'trigger', action: 'drop', name: original.name, table: tableName, behavior: 'restrict' }));
      out.push(create(name, false));
      current = name;
      recreated = true;
    }
  }
  if (form.state !== before.state || (recreated && form.state !== 'enabled')) {
    out.push(pg({ object: 'trigger', action: 'setState', name: current, table: tableName, state: FIRING[form.state] }));
  }
  if (!same(form.comment, before.comment) || (recreated && textOrNull(form.comment))) out.push(commentChange(ref(current), form.comment));
  if (current !== name) out.push(pg({ object: 'trigger', action: 'rename', name: current, table: tableName, newName: name }));
  return out;
}

// ---------------------------------------------------------------------------
// PostgreSQL event triggers
// ---------------------------------------------------------------------------

export interface PgEventTriggerForm {
  name: string;
  event: PgEventTriggerEvent;
  /** Command tags, one per line or comma separated. */
  tags: string;
  function: string;
  state: PgTriggerStateForm;
  owner: string;
  comment: string;
}

const EVENT_NAMES: Record<string, PgEventTriggerEvent> = {
  ddl_command_start: 'ddlCommandStart',
  ddl_command_end: 'ddlCommandEnd',
  table_rewrite: 'tableRewrite',
  sql_drop: 'sqlDrop',
  login: 'login',
};

/** The command tags of the form, upper case, without blanks. */
export function parseTags(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((t) => t.trim().replace(/\s+/g, ' ').toUpperCase())
    .filter((t) => t !== '');
}

export function pgEventTriggerFormFrom(trigger: PgEventTrigger | null): PgEventTriggerForm {
  if (!trigger) return { name: '', event: 'ddlCommandEnd', tags: '', function: '', state: 'enabled', owner: '', comment: '' };
  return {
    name: trigger.name,
    event: EVENT_NAMES[trigger.event] ?? 'ddlCommandEnd',
    tags: trigger.tags.join('\n'),
    function: trigger.function,
    state: trigger.state,
    owner: trigger.meta.owner ?? '',
    comment: trigger.meta.comment ?? '',
  };
}

export function pgEventTriggerSchema(server: ServerTarget) {
  return z
    .object({
      name: z.string().refine((v) => v.trim() !== '', 'Enter a name.'),
      event: z.enum(['ddlCommandStart', 'ddlCommandEnd', 'tableRewrite', 'sqlDrop', 'login']),
      tags: z.string(),
      function: z.string().refine((v) => v.trim() !== '', 'Choose the event trigger function.'),
      state: z.enum(['enabled', 'always', 'replica', 'disabled']),
      owner: z.string(),
      comment: z.string(),
    })
    .superRefine((v, ctx) => {
      if (v.event === 'login' && !pgAtLeast(server, 17)) custom(ctx, ['event'], 'Login event triggers need PostgreSQL 17 or newer.');
      if (v.event === 'login' && parseTags(v.tags).length > 0) custom(ctx, ['tags'], 'Login event triggers take no command tags.');
    });
}

export function buildPgEventTrigger(form: PgEventTriggerForm, original: PgEventTrigger | null, database: string | null): EngineChange[] {
  const name = form.name.trim();
  const fn = parseQualified(form.function);
  const create = pg({ object: 'eventTrigger', action: 'create', name, event: form.event, tags: parseTags(form.tags), function: { schema: fn.schema, name: fn.name }, comment: null });
  const ref = (n: string): ObjectRef => objectRef('eventTrigger', database, null, n);
  const out: EngineChange[] = [];
  if (!original) {
    out.push(create);
    if (form.state !== 'enabled') out.push(pg({ object: 'eventTrigger', action: 'setState', name, state: FIRING[form.state] }));
    if (textOrNull(form.owner)) out.push(pg({ object: 'eventTrigger', action: 'owner', name, owner: roleSpec(form.owner) }));
    if (textOrNull(form.comment)) out.push(commentChange(ref(name), form.comment));
    return out;
  }
  const before = pgEventTriggerFormFrom(original);
  const redefine = form.event !== before.event || JSON.stringify(parseTags(form.tags)) !== JSON.stringify(parseTags(before.tags)) || !same(form.function, before.function);
  let current = original.name;
  if (redefine) {
    // Event triggers have no OR REPLACE.
    out.push(pg({ object: 'eventTrigger', action: 'drop', name: original.name }));
    out.push(create);
    current = name;
  }
  if (form.state !== before.state || (redefine && form.state !== 'enabled')) {
    out.push(pg({ object: 'eventTrigger', action: 'setState', name: current, state: FIRING[form.state] }));
  }
  if (textOrNull(form.owner) && (!same(form.owner, before.owner) || redefine)) {
    out.push(pg({ object: 'eventTrigger', action: 'owner', name: current, owner: roleSpec(form.owner) }));
  }
  if (!same(form.comment, before.comment) || (redefine && textOrNull(form.comment))) out.push(commentChange(ref(current), form.comment));
  if (current !== name) out.push(pg({ object: 'eventTrigger', action: 'rename', name: current, newName: name }));
  return out;
}

// ---------------------------------------------------------------------------
// MySQL / MariaDB triggers
// ---------------------------------------------------------------------------

export interface MyTriggerForm extends MyDefinerForm {
  name: string;
  timing: 'before' | 'after';
  insert: boolean;
  update: boolean;
  delete: boolean;
  orderKind: '' | 'follows' | 'precedes';
  orderTrigger: string;
  body: string;
}

export function myTriggerFormFrom(trigger: MyTrigger | null): MyTriggerForm {
  if (!trigger) {
    return {
      name: '',
      timing: 'before',
      insert: true,
      update: false,
      delete: false,
      orderKind: '',
      orderTrigger: '',
      body: 'BEGIN\n\nEND',
      definer: 'currentUser',
      definerUser: '',
      definerHost: '%',
    };
  }
  const events = trigger.event.toUpperCase();
  return {
    name: trigger.name,
    timing: trigger.timing.toLowerCase() === 'after' ? 'after' : 'before',
    insert: events.includes('INSERT'),
    update: events.includes('UPDATE'),
    delete: events.includes('DELETE'),
    orderKind: trigger.follows ? 'follows' : trigger.precedes ? 'precedes' : '',
    orderTrigger: trigger.follows ?? trigger.precedes ?? '',
    body: trigger.body ?? '',
    ...definerFormFrom(trigger.definer),
  };
}

/** MariaDB 10.6+ accepts several events in one trigger; MySQL takes exactly one. */
export function myTriggerSchema(engine: Engine) {
  return z
    .object({
      name: z.string().refine((v) => v.trim() !== '', 'Enter a name.'),
      timing: z.enum(['before', 'after']),
      insert: z.boolean(),
      update: z.boolean(),
      delete: z.boolean(),
      orderKind: z.enum(['', 'follows', 'precedes']),
      orderTrigger: z.string(),
      body: z.string().refine((v) => v.trim() !== '', 'Enter the trigger body.'),
      ...definerShape,
    })
    .superRefine((v, ctx) => {
      definerRefine(v, ctx);
      const count = [v.insert, v.update, v.delete].filter(Boolean).length;
      if (count === 0) custom(ctx, ['events'], 'Pick an event.');
      else if (count > 1 && engine !== 'mariadb') custom(ctx, ['events'], 'MySQL triggers fire on exactly one event.');
      if (v.orderKind !== '' && v.orderTrigger.trim() === '') custom(ctx, ['orderTrigger'], 'Choose the other trigger.');
    });
}

/** `CREATE [OR REPLACE] TRIGGER`; the backend drops and creates on MySQL and says so in the plan. */
export function buildMyTrigger(form: MyTriggerForm, original: MyTrigger | null, database: string, table: string): EngineChange[] {
  const name = form.name.trim();
  const events: ('insert' | 'update' | 'delete')[] = [];
  if (form.insert) events.push('insert');
  if (form.update) events.push('update');
  if (form.delete) events.push('delete');
  const create = my({
    change: 'createTrigger',
    database,
    table,
    name,
    replace: original !== null && original.name === name,
    definer: definerOf(form),
    timing: form.timing,
    events,
    order: form.orderKind === '' ? null : { kind: form.orderKind, trigger: form.orderTrigger.trim() },
    body: form.body.trim().replace(/;\s*$/, ''),
  });
  if (original && original.name !== name) return [create, my({ change: 'dropTrigger', database, name: original.name })];
  return [create];
}

// ---------------------------------------------------------------------------
// Sequences
// ---------------------------------------------------------------------------

export interface PgSequenceForm extends PgIdentity {
  dataType: PgSequenceType;
  increment: string;
  start: string;
  /** Blank: NO MINVALUE (the type's default). */
  min: string;
  /** Blank: NO MAXVALUE (the type's default). */
  max: string;
  cache: string;
  cycle: boolean;
  /** `schema.table.column`, or blank for none. */
  ownedBy: string;
  logged: boolean;
}

const PG_SEQ_TYPES: Record<string, PgSequenceType> = { smallint: 'smallint', integer: 'integer', bigint: 'bigint' };

/** The default limits of a sequence type and direction (exact; bigint limits are 64-bit). */
function defaultLimits(type: PgSequenceType, ascending: boolean): { min: bigint; max: bigint } {
  const top = type === 'smallint' ? 32767n : type === 'integer' ? 2147483647n : INT64_MAX;
  const bottom = type === 'smallint' ? -32768n : type === 'integer' ? -2147483648n : INT64_MIN;
  return ascending ? { min: 1n, max: top } : { min: bottom, max: -1n };
}

export function pgSequenceFormFrom(seq: PgSequence | null, schema: string): PgSequenceForm {
  if (!seq) {
    return { schema, name: '', owner: '', comment: '', dataType: 'bigint', increment: '1', start: '', min: '', max: '', cache: '1', cycle: false, ownedBy: '', logged: true };
  }
  const type = PG_SEQ_TYPES[seq.dataType] ?? 'bigint';
  // Values arrive as exact decimal text.
  const limits = defaultLimits(type, BigInt(seq.increment) > 0n);
  return {
    schema: seq.schema,
    name: seq.name,
    owner: seq.owner,
    comment: seq.comment ?? '',
    dataType: type,
    increment: seq.increment,
    start: seq.start,
    min: BigInt(seq.min) === limits.min ? '' : seq.min,
    max: BigInt(seq.max) === limits.max ? '' : seq.max,
    cache: seq.cache,
    cycle: seq.cycle,
    ownedBy: seq.ownedBy ? `${seq.ownedBy.schema}.${seq.ownedBy.table}.${seq.ownedBy.column}` : '',
    logged: !seq.unlogged,
  };
}

/** Where a sequence stands, from `last_value` and `is_called`; `null` parts are unknown (no privilege, not read). */
export interface PgSequenceState {
  /** The last value handed out; `null` when none was since it was created, restarted or set with "next call returns it". */
  current: string | null;
  /** The value the next `nextval` returns (other sessions' cached values aside). */
  next: string | null;
}

export function pgSequenceState(seq: PgSequence): PgSequenceState {
  if (seq.lastValue === null || seq.isCalled === null) return { current: null, next: null };
  if (!seq.isCalled) return { current: null, next: seq.lastValue };
  return { current: seq.lastValue, next: (BigInt(seq.lastValue) + BigInt(seq.increment)).toString() };
}

/**
 * True when a reset makes the next `nextval` return a value at or before the
 * one it returns now, so values handed out before can come again. `next` is
 * what the next `nextval` returns after the reset.
 */
export function pgSequenceGoesBack(seq: PgSequence, next: bigint): boolean {
  const now = pgSequenceState(seq).next;
  if (now === null) return false;
  return BigInt(seq.increment) > 0n ? next < BigInt(now) : next > BigInt(now);
}

/** `schema.table.column` of an OWNED BY field. */
export function parseOwnedBy(text: string): { schema: string | null; table: string; column: string } | null {
  const parts = text.trim().split('.');
  if (parts.length === 2) return { schema: null, table: parts[0] as string, column: parts[1] as string };
  if (parts.length === 3) return { schema: parts[0] as string, table: parts[1] as string, column: parts[2] as string };
  return null;
}

export const pgSequenceSchema = z
  .object({
    ...identitySchema,
    dataType: z.enum(['smallint', 'integer', 'bigint']),
    increment: z.string(),
    start: z.string(),
    min: z.string(),
    max: z.string(),
    cache: z.string(),
    cycle: z.boolean(),
    ownedBy: z.string(),
    logged: z.boolean(),
  })
  .superRefine((v, ctx) => {
    for (const [field, optional] of [['increment', false], ['start', true], ['min', true], ['max', true], ['cache', false]] as const) {
      const problem = bigIntegerProblem(v[field], optional);
      if (problem) custom(ctx, [field], problem);
    }
    if (parseBigInteger(v.increment) === 0n) custom(ctx, ['increment'], 'The increment cannot be 0.');
    const cache = parseBigInteger(v.cache);
    if (cache !== null && cache < 1n) custom(ctx, ['cache'], 'The cache is at least 1.');
    const min = parseBigInteger(v.min);
    const max = parseBigInteger(v.max);
    if (min !== null && max !== null && min >= max) custom(ctx, ['max'], 'The maximum must be above the minimum.');
    const start = parseBigInteger(v.start);
    if (start !== null && min !== null && start < min) custom(ctx, ['start'], 'The start cannot be below the minimum.');
    if (start !== null && max !== null && start > max) custom(ctx, ['start'], 'The start cannot be above the maximum.');
    if (v.ownedBy.trim() !== '' && !parseOwnedBy(v.ownedBy)) custom(ctx, ['ownedBy'], 'Write it as table.column or schema.table.column.');
  });

function limit(text: string): PgSequenceLimit {
  const v = integerText(text);
  return v === null ? { kind: 'default' } : { kind: 'value', value: v };
}

function ownedByRef(text: string): { table: QualifiedName; column: string } | null {
  const o = parseOwnedBy(text);
  return o ? { table: { schema: o.schema, name: o.table }, column: o.column } : null;
}

/** Only changed options are sent; values travel as exact decimal text. */
export function buildPgSequence(form: PgSequenceForm, original: PgSequence | null, database: string | null): EngineChange[] {
  if (!original) {
    const options: PgSequenceOptions = {
      dataType: form.dataType,
      increment: integerText(form.increment),
      start: integerText(form.start),
      minValue: textOrNull(form.min) ? limit(form.min) : null,
      maxValue: textOrNull(form.max) ? limit(form.max) : null,
      cache: integerText(form.cache),
      cycle: form.cycle,
    };
    const out: EngineChange[] = [
      pg({
        object: 'sequence',
        action: 'create',
        name: qn(form.schema, form.name),
        unlogged: !form.logged,
        options,
        ownedBy: textOrNull(form.ownedBy) ? ownedByRef(form.ownedBy) : null,
        comment: textOrNull(form.comment),
      }),
    ];
    if (textOrNull(form.owner)) {
      out.push(pg({ object: 'sequence', action: 'alter', name: qn(form.schema, form.name), operations: [{ op: 'owner', owner: roleSpec(form.owner) }] }));
    }
    return out;
  }
  const before = pgSequenceFormFrom(original, original.schema);
  const name = qn(original.schema, original.name);
  const options: PgSequenceOptions = {};
  if (form.dataType !== before.dataType) options.dataType = form.dataType;
  if (!same(form.increment, before.increment)) options.increment = integerText(form.increment);
  if (!same(form.start, before.start) && textOrNull(form.start)) options.start = integerText(form.start);
  if (!same(form.min, before.min)) options.minValue = limit(form.min);
  if (!same(form.max, before.max)) options.maxValue = limit(form.max);
  if (!same(form.cache, before.cache)) options.cache = integerText(form.cache);
  if (form.cycle !== before.cycle) options.cycle = form.cycle;
  const operations: PgSequenceOperation[] = [];
  if (Object.keys(options).length > 0) operations.push({ op: 'options', options });
  if (!same(form.ownedBy, before.ownedBy)) operations.push({ op: 'ownedBy', column: textOrNull(form.ownedBy) ? ownedByRef(form.ownedBy) : null });
  if (form.logged !== before.logged) operations.push({ op: 'logged', logged: form.logged });
  operations.push(...identityOps<PgSequenceOperation & { op: 'owner' | 'setSchema' | 'rename' }>(form, original));
  const out: EngineChange[] = [];
  if (!same(form.comment, before.comment)) out.push(commentChange(objectRef('sequence', database, original.schema, original.name), form.comment));
  // The comment goes first: the alter may rename the sequence.
  if (operations.length > 0) out.push(pg({ object: 'sequence', action: 'alter', name, operations }));
  return out;
}

/** `setval` for a PostgreSQL sequence: `isCalled` false makes the next `nextval` return `value` itself. `value` is exact decimal text. */
export function buildPgSequenceSetValue(seq: PgSequence, value: string, isCalled: boolean): EngineChange {
  return pg({ object: 'sequence', action: 'alter', name: qn(seq.schema, seq.name), operations: [{ op: 'setValue', value, isCalled }] });
}

/** `ALTER SEQUENCE ... RESTART [WITH n]` (`value` as exact decimal text). */
export function buildPgSequenceRestart(seq: PgSequence, value: string | null): EngineChange {
  return pg({ object: 'sequence', action: 'alter', name: qn(seq.schema, seq.name), operations: [{ op: 'restart', with: value }] });
}

export interface MySequenceForm {
  name: string;
  dataType: '' | 'tinyint' | 'smallint' | 'mediumint' | 'int' | 'bigint';
  unsigned: boolean;
  increment: string;
  start: string;
  min: string;
  max: string;
  cache: string;
  cycle: boolean;
  comment: string;
}

export function mySequenceFormFrom(seq: MySequence | null): MySequenceForm {
  if (!seq) return { name: '', dataType: '', unsigned: false, increment: '1', start: '', min: '', max: '', cache: '1000', cycle: false, comment: '' };
  const type = (seq.dataType ?? '').toLowerCase();
  const base = type.replace(/\s*unsigned/, '').trim();
  return {
    name: seq.name,
    dataType: (['tinyint', 'smallint', 'mediumint', 'int', 'bigint'] as const).find((t) => t === base) ?? '',
    unsigned: type.includes('unsigned'),
    increment: seq.increment,
    start: seq.start,
    min: seq.minimum,
    max: seq.maximum,
    cache: seq.cache,
    cycle: seq.cycle,
    comment: seq.comment ?? '',
  };
}

export const mySequenceSchema = z
  .object({
    name: z.string().refine((v) => v.trim() !== '', 'Enter a name.'),
    dataType: z.enum(['', 'tinyint', 'smallint', 'mediumint', 'int', 'bigint']),
    unsigned: z.boolean(),
    increment: z.string(),
    start: z.string(),
    min: z.string(),
    max: z.string(),
    cache: z.string(),
    cycle: z.boolean(),
    comment: z.string(),
  })
  .superRefine((v, ctx) => {
    for (const field of ['increment', 'start', 'min', 'max'] as const) {
      const problem = bigIntegerProblem(v[field], true, INT64_MIN, UINT64_MAX);
      if (problem) custom(ctx, [field], problem);
    }
    const cacheProblem = bigIntegerProblem(v.cache, true, 0n, UINT64_MAX);
    if (cacheProblem) custom(ctx, ['cache'], v.cache.trim().startsWith('-') ? 'The cache cannot be negative.' : cacheProblem);
    const min = parseBigInteger(v.min, INT64_MIN, UINT64_MAX);
    const max = parseBigInteger(v.max, INT64_MIN, UINT64_MAX);
    if (min !== null && max !== null && min >= max) custom(ctx, ['max'], 'The maximum must be above the minimum.');
  });

/** MariaDB sequence values range from the signed to the unsigned 64-bit limit. */
function myValue(text: string): string | null {
  return integerText(text, INT64_MIN, UINT64_MAX);
}

function myLimit(text: string): MySequenceLimit {
  const v = myValue(text);
  return v === null ? { kind: 'unbounded' } : { kind: 'value', value: v };
}

/** MariaDB sequences. Unchanged values (exact decimal text from the server) are never sent back. */
export function buildMySequence(form: MySequenceForm, original: MySequence | null, database: string): EngineChange[] {
  const fields: MySequenceFields = {};
  const before = original ? mySequenceFormFrom(original) : null;
  const changed = (key: 'increment' | 'start' | 'min' | 'max' | 'cache'): boolean =>
    before === null ? form[key].trim() !== '' : !same(form[key], before[key]);
  if (form.dataType !== '' && (before === null || form.dataType !== before.dataType || form.unsigned !== before.unsigned)) {
    fields.dataType = { name: form.dataType, unsigned: form.unsigned };
  }
  if (changed('increment')) fields.increment = myValue(form.increment);
  if (changed('start')) fields.start = myValue(form.start);
  if (changed('min')) fields.minValue = myLimit(form.min);
  if (changed('max')) fields.maxValue = myLimit(form.max);
  if (changed('cache')) fields.cache = integerText(form.cache, 0n, UINT64_MAX);
  if (before === null || form.cycle !== before.cycle) fields.cycle = form.cycle;
  if (!original) {
    return [my({ change: 'createSequence', database, name: form.name.trim(), comment: textOrNull(form.comment), ...fields })];
  }
  if (Object.keys(fields).length === 0) return [];
  return [my({ change: 'alterSequence', database, name: original.name, ...fields })];
}

/** MariaDB `SETVAL(seq, value, is_used)` (`value` as exact decimal text). */
export function buildMySequenceSetValue(seq: MySequence, value: string, isUsed: boolean): EngineChange {
  return my({ change: 'setSequenceValue', database: seq.database, name: seq.name, value, isUsed });
}

/** MariaDB `ALTER SEQUENCE ... RESTART [WITH n]` (`value` as exact decimal text). */
export function buildMySequenceRestart(seq: MySequence, value: string | null): EngineChange {
  return my({ change: 'alterSequence', database: seq.database, name: seq.name, restart: value === null ? { kind: 'start' } : { kind: 'with', value } });
}

// ---------------------------------------------------------------------------
// MySQL / MariaDB events
// ---------------------------------------------------------------------------

export const INTERVAL_UNITS: readonly { value: MyIntervalUnit; label: string }[] = [
  { value: 'second', label: 'SECOND' },
  { value: 'minute', label: 'MINUTE' },
  { value: 'hour', label: 'HOUR' },
  { value: 'day', label: 'DAY' },
  { value: 'week', label: 'WEEK' },
  { value: 'month', label: 'MONTH' },
  { value: 'quarter', label: 'QUARTER' },
  { value: 'year', label: 'YEAR' },
  { value: 'minuteSecond', label: 'MINUTE_SECOND' },
  { value: 'hourSecond', label: 'HOUR_SECOND' },
  { value: 'hourMinute', label: 'HOUR_MINUTE' },
  { value: 'daySecond', label: 'DAY_SECOND' },
  { value: 'dayMinute', label: 'DAY_MINUTE' },
  { value: 'dayHour', label: 'DAY_HOUR' },
  { value: 'yearMonth', label: 'YEAR_MONTH' },
];

function unitFrom(field: string | null): MyIntervalUnit {
  const label = (field ?? '').toUpperCase();
  return INTERVAL_UNITS.find((u) => u.label === label)?.value ?? 'hour';
}

export interface MyEventForm extends MyDefinerForm {
  name: string;
  kind: 'at' | 'every';
  /** AT: a timestamp `YYYY-MM-DD HH:MM:SS`, or blank for CURRENT_TIMESTAMP. */
  at: string;
  /** AT: optional `+ INTERVAL` quantity and unit after the base time. */
  atPlusQuantity: string;
  atPlusUnit: MyIntervalUnit;
  everyQuantity: string;
  everyUnit: MyIntervalUnit;
  starts: string;
  ends: string;
  preserve: boolean;
  status: MyEventStatus;
  comment: string;
  body: string;
}

const STATUS_FROM: Record<MyEvent['status'], MyEventStatus> = { enabled: 'enable', disabled: 'disable', slavesideDisabled: 'disableOnReplica' };

/** Quantities as information_schema prints them: `6`, `'1:30'`. */
function quantityText(value: string | null): string {
  return (value ?? '').trim().replace(/^'(.*)'$/, '$1');
}

export function myEventFormFrom(event: MyEvent | null): MyEventForm {
  const blank: MyEventForm = {
    name: '',
    kind: 'every',
    at: '',
    atPlusQuantity: '',
    atPlusUnit: 'hour',
    everyQuantity: '1',
    everyUnit: 'day',
    starts: '',
    ends: '',
    preserve: false,
    status: 'enable',
    comment: '',
    body: 'BEGIN\n\nEND',
    definer: 'currentUser',
    definerUser: '',
    definerHost: '%',
  };
  if (!event) return blank;
  const base = {
    ...blank,
    name: event.name,
    preserve: event.preserve,
    status: STATUS_FROM[event.status],
    comment: event.comment ?? '',
    body: event.body ?? '',
    ...definerFormFrom(event.definer),
  };
  if (event.schedule.type === 'at') return { ...base, kind: 'at', at: event.schedule.at ?? '' };
  return {
    ...base,
    kind: 'every',
    everyQuantity: quantityText(event.schedule.intervalValue),
    everyUnit: unitFrom(event.schedule.intervalField),
    starts: event.schedule.starts ?? '',
    ends: event.schedule.ends ?? '',
  };
}

const TIMESTAMP = /^\d{4}-\d{2}-\d{2}( \d{2}:\d{2}(:\d{2}(\.\d{1,6})?)?)?$/;
const QUANTITY = /^[0-9]+([ :.\-_][0-9]+)*$/;

export const myEventSchema = z
  .object({
    name: z.string().refine((v) => v.trim() !== '', 'Enter a name.'),
    kind: z.enum(['at', 'every']),
    at: z.string(),
    atPlusQuantity: z.string(),
    atPlusUnit: z.string(),
    everyQuantity: z.string(),
    everyUnit: z.string(),
    starts: z.string(),
    ends: z.string(),
    preserve: z.boolean(),
    status: z.enum(['enable', 'disable', 'disableOnReplica']),
    comment: z.string(),
    body: z.string().refine((v) => v.trim() !== '', 'Enter the event body.'),
    ...definerShape,
  })
  .superRefine((v, ctx) => {
    definerRefine(v, ctx);
    const ts = (field: 'at' | 'starts' | 'ends'): void => {
      if (v[field].trim() !== '' && !TIMESTAMP.test(v[field].trim())) custom(ctx, [field], 'Write it as YYYY-MM-DD HH:MM:SS.');
    };
    if (v.kind === 'at') {
      ts('at');
      if (v.atPlusQuantity.trim() !== '' && !QUANTITY.test(v.atPlusQuantity.trim())) custom(ctx, ['atPlusQuantity'], 'Enter a number, or for compound units digits like 1:30.');
    } else {
      if (!QUANTITY.test(v.everyQuantity.trim())) custom(ctx, ['everyQuantity'], 'Enter a number, or for compound units digits like 1:30.');
      else if (/^0+$/.test(v.everyQuantity.trim())) custom(ctx, ['everyQuantity'], 'The interval must be at least 1.');
      ts('starts');
      ts('ends');
      if (v.starts.trim() !== '' && v.ends.trim() !== '' && v.ends.trim() <= v.starts.trim()) custom(ctx, ['ends'], 'The end must be after the start.');
    }
  });

function eventTime(text: string): MyEventTime | null {
  const t = text.trim();
  return t === '' ? null : { base: { kind: 'timestamp', value: t } };
}

function scheduleOf(form: MyEventForm): MyEventSchedule {
  if (form.kind === 'at') {
    const plus = form.atPlusQuantity.trim() === '' ? [] : [{ quantity: form.atPlusQuantity.trim(), unit: form.atPlusUnit }];
    const base = eventTime(form.at)?.base ?? { kind: 'currentTimestamp' as const };
    return { kind: 'at', at: { base, plus } };
  }
  return {
    kind: 'every',
    interval: { quantity: form.everyQuantity.trim(), unit: form.everyUnit },
    starts: eventTime(form.starts),
    ends: eventTime(form.ends),
  };
}

function scheduleKey(form: MyEventForm): string {
  return JSON.stringify(scheduleOf(form));
}

/** `CREATE EVENT`, or one `ALTER EVENT` with only what changed (rename included). */
export function buildMyEvent(form: MyEventForm, original: MyEvent | null, database: string): EngineChange[] {
  if (!original) {
    return [
      my({
        change: 'createEvent',
        database,
        name: form.name.trim(),
        definer: definerOf(form),
        schedule: scheduleOf(form),
        onCompletionPreserve: form.preserve,
        status: form.status,
        comment: textOrNull(form.comment),
        body: form.body.trim().replace(/;\s*$/, ''),
      }),
    ];
  }
  const before = myEventFormFrom(original);
  const change: Extract<MyChange, { change: 'alterEvent' }> = { change: 'alterEvent', database, name: original.name };
  let any = false;
  if (form.definer !== before.definer || !same(form.definerUser, before.definerUser) || !same(form.definerHost, before.definerHost)) {
    change.definer = definerOf(form);
    any = true;
  }
  if (form.kind !== before.kind || scheduleKey(form) !== scheduleKey(before)) {
    change.schedule = scheduleOf(form);
    any = true;
  }
  if (form.preserve !== before.preserve) {
    change.onCompletionPreserve = form.preserve;
    any = true;
  }
  if (form.status !== before.status) {
    change.status = form.status;
    any = true;
  }
  if (!same(form.comment, before.comment)) {
    // An empty comment clears it (`COMMENT ''`).
    change.comment = form.comment.trim();
    any = true;
  }
  if (!same(form.body, before.body)) {
    change.body = form.body.trim().replace(/;\s*$/, '');
    any = true;
  }
  if (!same(form.name, before.name)) {
    change.renameTo = form.name.trim();
    any = true;
  }
  return any ? [my(change)] : [];
}

/**
 * The next run times of a recurring schedule after `from` (local wall
 * clock of the server time zone is not known here; times are shown as
 * the server prints them). Only simple units are projected.
 */
export function nextRuns(form: MyEventForm, from: Date, count: number): Date[] | null {
  if (form.kind !== 'every') return null;
  const quantity = Number(form.everyQuantity.trim());
  if (!Number.isInteger(quantity) || quantity < 1) return null;
  const startText = form.starts.trim();
  const start = startText === '' ? null : new Date(startText.replace(' ', 'T'));
  if (start && Number.isNaN(start.getTime())) return null;
  const step = (d: Date, n: number): Date => {
    const out = new Date(d.getTime());
    switch (form.everyUnit) {
      case 'second':
        out.setSeconds(out.getSeconds() + n * quantity);
        return out;
      case 'minute':
        out.setMinutes(out.getMinutes() + n * quantity);
        return out;
      case 'hour':
        out.setHours(out.getHours() + n * quantity);
        return out;
      case 'day':
        out.setDate(out.getDate() + n * quantity);
        return out;
      case 'week':
        out.setDate(out.getDate() + 7 * n * quantity);
        return out;
      case 'month':
        out.setMonth(out.getMonth() + n * quantity);
        return out;
      case 'quarter':
        out.setMonth(out.getMonth() + 3 * n * quantity);
        return out;
      case 'year':
        out.setFullYear(out.getFullYear() + n * quantity);
        return out;
      default:
        return new Date(Number.NaN);
    }
  };
  if (!start || Number.isNaN(step(start, 1).getTime())) return null;
  const ends = form.ends.trim() === '' ? null : new Date(form.ends.trim().replace(' ', 'T'));
  const out: Date[] = [];
  let n = 0;
  let t = start;
  // Skip ahead in big steps when the start is far in the past.
  while (t.getTime() <= from.getTime() && n < 1_000_000) {
    n += 1;
    t = step(start, n);
  }
  while (out.length < count) {
    if (ends && t.getTime() > ends.getTime()) break;
    out.push(t);
    n += 1;
    t = step(start, n);
  }
  return out;
}

// ---------------------------------------------------------------------------
// PostgreSQL types and domains
// ---------------------------------------------------------------------------

/** Enum edits: values are added (optionally positioned) and renamed; existing values cannot be removed or moved. */
export interface PgEnumForm extends PgIdentity {
  /** Current labels in order, each with the label it had (`null` for new ones). */
  values: { label: string; original: string | null }[];
}

export function pgEnumFormFrom(type: PgEnumType | null, schema: string): PgEnumForm {
  if (!type) return { schema, name: '', owner: '', comment: '', values: [] };
  return {
    schema: type.schema,
    name: type.name,
    owner: type.meta.owner ?? '',
    comment: type.meta.comment ?? '',
    values: type.labels.map((l) => ({ label: l, original: l })),
  };
}

export const pgEnumSchema = z
  .object({
    ...identitySchema,
    values: z.array(z.object({ label: z.string(), original: z.string().nullable() })),
  })
  .superRefine((v, ctx) => {
    const seen = new Set<string>();
    v.values.forEach((value, i) => {
      if (value.label === '') custom(ctx, ['values', i], 'A label cannot be empty.');
      else if (seen.has(value.label)) custom(ctx, ['values', i], `'${value.label}' is listed twice.`);
      else if (new TextEncoder().encode(value.label).length > 63) custom(ctx, ['values', i], 'Labels are at most 63 bytes.');
      seen.add(value.label);
    });
  });

/**
 * Enum changes: renames, then each new value with its position relative
 * to its neighbour in the form (new values must be added one statement at
 * a time; PostgreSQL commits them before use).
 */
export function buildPgEnum(form: PgEnumForm, original: PgEnumType | null, database: string | null): EngineChange[] {
  const name = original ? qn(original.schema, original.name) : qn(form.schema, form.name);
  if (!original) {
    const out: EngineChange[] = [pg({ object: 'type', action: 'createEnum', name, labels: form.values.map((v) => v.label), comment: textOrNull(form.comment) })];
    if (textOrNull(form.owner)) out.push(pg({ object: 'type', action: 'owner', name, owner: roleSpec(form.owner) }));
    return out;
  }
  const out: EngineChange[] = [];
  for (const v of form.values) {
    if (v.original !== null && v.original !== v.label) {
      out.push(pg({ object: 'type', action: 'renameEnumValue', name, value: v.original, newValue: v.label }));
    }
  }
  form.values.forEach((v, i) => {
    if (v.original !== null) return;
    // Position against the nearest existing (or already added) neighbour.
    const prev = form.values[i - 1];
    const next = form.values[i + 1];
    const position = prev ? { kind: 'after' as const, label: prev.label } : next ? { kind: 'before' as const, label: next.label } : null;
    out.push(pg({ object: 'type', action: 'addEnumValue', name, value: v.label, position }));
  });
  out.push(...typeIdentity(form, original.schema, original.name, original.meta, 'enumType', database));
  return out;
}

/** True when the form moves or removes existing labels, which PostgreSQL cannot do in place. */
export function enumNeedsRecreate(form: PgEnumForm, original: PgEnumType): string | null {
  const kept = form.values.filter((v) => v.original !== null).map((v) => v.original);
  if (kept.length < original.labels.length) return 'Existing values cannot be removed: the type would have to be recreated and every column using it converted.';
  if (JSON.stringify(kept) !== JSON.stringify(original.labels)) return 'Existing values cannot be reordered: the type would have to be recreated.';
  return null;
}

function typeIdentity(form: PgIdentity, schema: string, name: string, meta: { owner: string | null; comment: string | null }, kind: ObjectKind, database: string | null): EngineChange[] {
  const out: EngineChange[] = [];
  const target = qn(schema, name);
  const object = kind === 'domain' ? 'domain' : 'type';
  if (!same(form.comment, meta.comment)) out.push(commentChange(objectRef(kind, database, schema, name), form.comment));
  if (textOrNull(form.owner) && !same(form.owner, meta.owner)) {
    out.push(
      object === 'domain'
        ? pg({ object: 'domain', action: 'alter', name: target, operations: [{ op: 'owner', owner: roleSpec(form.owner) }] })
        : pg({ object: 'type', action: 'owner', name: target, owner: roleSpec(form.owner) }),
    );
  }
  let current = target;
  if (!same(form.schema, schema)) {
    out.push(
      object === 'domain'
        ? pg({ object: 'domain', action: 'alter', name: current, operations: [{ op: 'setSchema', schema: form.schema.trim() }] })
        : pg({ object: 'type', action: 'setSchema', name: current, schema: form.schema.trim() }),
    );
    current = { schema: form.schema.trim(), name };
  }
  if (!same(form.name, name)) {
    out.push(
      object === 'domain'
        ? pg({ object: 'domain', action: 'alter', name: current, operations: [{ op: 'rename', newName: form.name.trim() }] })
        : pg({ object: 'type', action: 'rename', name: current, newName: form.name.trim() }),
    );
  }
  return out;
}

export interface PgCompositeForm extends PgIdentity {
  attributes: { name: string; dataType: string; collation: string; original: string | null }[];
  /** Propagate attribute changes to typed tables (`CASCADE`). */
  cascade: boolean;
}

export function pgCompositeFormFrom(type: PgCompositeType | null, schema: string): PgCompositeForm {
  if (!type) return { schema, name: '', owner: '', comment: '', attributes: [], cascade: false };
  return {
    schema: type.schema,
    name: type.name,
    owner: type.meta.owner ?? '',
    comment: type.meta.comment ?? '',
    attributes: type.attributes.map((a) => ({ name: a.name, dataType: a.dataType, collation: a.collation ?? '', original: a.name })),
    cascade: false,
  };
}

export const pgCompositeSchema = z
  .object({
    ...identitySchema,
    attributes: z.array(z.object({ name: z.string(), dataType: z.string(), collation: z.string(), original: z.string().nullable() })),
    cascade: z.boolean(),
  })
  .superRefine((v, ctx) => {
    const seen = new Set<string>();
    v.attributes.forEach((a, i) => {
      if (a.name.trim() === '') custom(ctx, ['attributes', i, 'name'], 'Enter the attribute name.');
      else if (seen.has(a.name.trim())) custom(ctx, ['attributes', i, 'name'], 'Names must be unique.');
      seen.add(a.name.trim());
      if (a.dataType.trim() === '') custom(ctx, ['attributes', i, 'dataType'], 'Enter the type.');
    });
  });

export function buildPgComposite(form: PgCompositeForm, original: PgCompositeType | null, database: string | null): EngineChange[] {
  const attr = (a: PgCompositeForm['attributes'][number]): PgCompositeAttribute => ({ name: a.name.trim(), dataType: a.dataType.trim(), collation: textOrNull(a.collation) });
  if (!original) {
    const name = qn(form.schema, form.name);
    const out: EngineChange[] = [pg({ object: 'type', action: 'createComposite', name, attributes: form.attributes.map(attr), comment: textOrNull(form.comment) })];
    if (textOrNull(form.owner)) out.push(pg({ object: 'type', action: 'owner', name, owner: roleSpec(form.owner) }));
    return out;
  }
  const ops: PgCompositeOperation[] = [];
  const cascade = form.cascade;
  for (const old of original.attributes) {
    if (!form.attributes.some((a) => a.original === old.name)) ops.push({ op: 'dropAttribute', name: old.name, cascade });
  }
  for (const a of form.attributes) {
    if (a.original === null) {
      ops.push({ op: 'addAttribute', attribute: attr(a), cascade });
      continue;
    }
    const old = original.attributes.find((o) => o.name === a.original);
    if (!old) continue;
    if (!same(a.dataType, old.dataType) || !same(a.collation, old.collation)) {
      ops.push({ op: 'alterAttributeType', name: old.name, dataType: a.dataType.trim(), collation: textOrNull(a.collation), cascade });
    }
  }
  const out: EngineChange[] = [];
  const name = qn(original.schema, original.name);
  if (ops.length > 0) out.push(pg({ object: 'type', action: 'alterComposite', name, operations: ops }));
  // Renames are separate statements (RENAME ATTRIBUTE cannot be combined with other actions).
  const renames: PgCompositeOperation[] = form.attributes
    .filter((a) => a.original !== null && a.original !== a.name.trim())
    .map((a) => ({ op: 'renameAttribute', name: a.original as string, newName: a.name.trim(), cascade }));
  for (const r of renames) out.push(pg({ object: 'type', action: 'alterComposite', name, operations: [r] }));
  out.push(...typeIdentity(form, original.schema, original.name, original.meta, 'compositeType', database));
  return out;
}

/** True when attributes were reordered: PostgreSQL cannot move attributes of a composite type. */
export function compositeReordered(form: PgCompositeForm, original: PgCompositeType): boolean {
  const kept = form.attributes.filter((a) => a.original !== null).map((a) => a.original);
  const was = original.attributes.map((a) => a.name).filter((n) => kept.includes(n));
  return JSON.stringify(kept) !== JSON.stringify(was);
}

export interface PgDomainForm extends PgIdentity {
  dataType: string;
  collation: string;
  default: string;
  notNull: boolean;
  /** Check constraints; `comment` is the constraint's own comment (`COMMENT ON CONSTRAINT ... ON DOMAIN`). */
  checks: { name: string; expression: string; original: string | null; validated: boolean; comment: string }[];
  /** Add new checks NOT VALID (existing rows are not checked). */
  notValid: boolean;
}

/** `CHECK (expr)` of a domain constraint definition, without the keyword and outer parentheses. */
export function checkExpression(definition: string): string {
  const m = /^CHECK\s*\(([\s\S]*)\)\s*(NOT VALID)?\s*$/i.exec(definition.trim());
  return m ? (m[1] as string).trim() : definition.trim();
}

export function pgDomainFormFrom(domain: PgDomain | null, schema: string): PgDomainForm {
  if (!domain) return { schema, name: '', owner: '', comment: '', dataType: '', collation: '', default: '', notNull: false, checks: [], notValid: false };
  return {
    schema: domain.schema,
    name: domain.name,
    owner: domain.meta.owner ?? '',
    comment: domain.meta.comment ?? '',
    dataType: domain.baseType,
    collation: domain.collation ?? '',
    default: domain.default ?? '',
    notNull: domain.notNull,
    checks: domain.constraints
      .filter((c) => !c.notNull)
      .map((c) => ({ name: c.name, expression: checkExpression(c.definition), original: c.name, validated: c.validated, comment: c.comment ?? '' })),
    notValid: false,
  };
}

export const pgDomainSchema = z
  .object({
    ...identitySchema,
    dataType: z.string().refine((v) => v.trim() !== '', 'Enter the base type.'),
    collation: z.string(),
    default: z.string(),
    notNull: z.boolean(),
    checks: z.array(z.object({ name: z.string(), expression: z.string(), original: z.string().nullable(), validated: z.boolean(), comment: z.string() })),
    notValid: z.boolean(),
  })
  .superRefine((v, ctx) => {
    v.checks.forEach((c, i) => {
      if (c.expression.trim() === '') custom(ctx, ['checks', i, 'expression'], 'Enter the check expression (use VALUE for the value).');
      // PostgreSQL names an unnamed check itself; the comment needs the name to find it.
      if (c.name.trim() === '' && c.comment.trim() !== '') custom(ctx, ['checks', i, 'name'], 'Name the check to give it a comment.');
    });
  });

export function buildPgDomain(form: PgDomainForm, original: PgDomain | null, database: string | null): EngineChange[] {
  if (!original) {
    const name = qn(form.schema, form.name);
    const out: EngineChange[] = [
      pg({
        object: 'domain',
        action: 'create',
        name,
        dataType: form.dataType.trim(),
        collation: textOrNull(form.collation),
        default: textOrNull(form.default),
        notNull: form.notNull,
        checks: form.checks.map((c) => ({ name: textOrNull(c.name), expression: c.expression.trim() })),
        comment: textOrNull(form.comment),
      }),
    ];
    const comments: PgDomainOperation[] = form.checks
      .filter((c) => textOrNull(c.name) !== null && normalizeComment(c.comment) !== null)
      .map((c) => ({ op: 'commentConstraint', name: c.name.trim(), comment: normalizeComment(c.comment) }));
    if (comments.length > 0) out.push(pg({ object: 'domain', action: 'alter', name, operations: comments }));
    if (textOrNull(form.owner)) out.push(pg({ object: 'domain', action: 'alter', name, operations: [{ op: 'owner', owner: roleSpec(form.owner) }] }));
    return out;
  }
  const before = pgDomainFormFrom(original, original.schema);
  const ops: PgDomainOperation[] = [];
  if (!same(form.default, before.default)) ops.push(textOrNull(form.default) ? { op: 'setDefault', expression: form.default.trim() } : { op: 'dropDefault' });
  if (form.notNull !== before.notNull) ops.push(form.notNull ? { op: 'setNotNull' } : { op: 'dropNotNull' });
  // Comments go last, on the constraints as they are named after the drops, adds and renames.
  const comments: PgDomainOperation[] = [];
  for (const old of before.checks) {
    const now = form.checks.find((c) => c.original === old.original);
    if (!now) {
      ops.push({ op: 'dropConstraint', name: old.original as string });
      continue;
    }
    const finalName = textOrNull(now.name) ?? (old.original as string);
    if (!same(now.expression, old.expression)) {
      // A changed check is dropped and added again under its name; its comment goes with the drop, so it is set again.
      ops.push({ op: 'dropConstraint', name: old.original as string });
      ops.push({ op: 'addCheck', check: { name: finalName, expression: now.expression.trim() }, notValid: form.notValid });
      if (normalizeComment(now.comment) !== null) comments.push({ op: 'commentConstraint', name: finalName, comment: normalizeComment(now.comment) });
      continue;
    }
    if (!same(now.name, old.name)) {
      ops.push({ op: 'renameConstraint', name: old.original as string, newName: now.name.trim() });
    }
    if (commentChanged(now.comment, old.comment === '' ? null : old.comment)) {
      comments.push({ op: 'commentConstraint', name: finalName, comment: normalizeComment(now.comment) });
    }
  }
  for (const c of form.checks.filter((x) => x.original === null)) {
    ops.push({ op: 'addCheck', check: { name: textOrNull(c.name), expression: c.expression.trim() }, notValid: form.notValid });
    if (textOrNull(c.name) !== null && normalizeComment(c.comment) !== null) {
      comments.push({ op: 'commentConstraint', name: c.name.trim(), comment: normalizeComment(c.comment) });
    }
  }
  ops.push(...comments);
  const out: EngineChange[] = [];
  const name = qn(original.schema, original.name);
  if (ops.length > 0) out.push(pg({ object: 'domain', action: 'alter', name, operations: ops }));
  out.push(...typeIdentity(form, original.schema, original.name, original.meta, 'domain', database));
  return out;
}

/** Validates a NOT VALID domain check. */
export function buildPgDomainValidate(domain: PgDomain, constraint: string): EngineChange {
  return pg({ object: 'domain', action: 'alter', name: qn(domain.schema, domain.name), operations: [{ op: 'validateConstraint', name: constraint }] });
}

/** A base type or collation change, which PostgreSQL cannot do on an existing domain. */
export function domainProblem(form: PgDomainForm, original: PgDomain | null): string | null {
  if (!original) return null;
  if (!same(form.dataType, original.baseType)) return 'The base type of an existing domain cannot be changed. Create a new domain instead.';
  if (!same(form.collation, original.collation)) return 'The collation of an existing domain cannot be changed.';
  return null;
}

export interface PgRangeForm extends PgIdentity {
  subtype: string;
  subtypeOpclass: string;
  collation: string;
  canonical: string;
  subtypeDiff: string;
  multirange: string;
}

export function pgRangeFormFrom(type: PgRangeType | null, schema: string): PgRangeForm {
  if (!type) return { schema, name: '', owner: '', comment: '', subtype: '', subtypeOpclass: '', collation: '', canonical: '', subtypeDiff: '', multirange: '' };
  return {
    schema: type.schema,
    name: type.name,
    owner: type.meta.owner ?? '',
    comment: type.meta.comment ?? '',
    subtype: type.subtype,
    subtypeOpclass: type.subtypeOpclass,
    collation: type.collation ?? '',
    canonical: type.canonical ?? '',
    subtypeDiff: type.subtypeDiff ?? '',
    multirange: type.multirange ?? '',
  };
}

export function pgRangeSchema(server: ServerTarget) {
  return z
    .object({
      ...identitySchema,
      subtype: z.string().refine((v) => v.trim() !== '', 'Enter the subtype.'),
      subtypeOpclass: z.string(),
      collation: z.string(),
      canonical: z.string(),
      subtypeDiff: z.string(),
      multirange: z.string(),
    })
    .superRefine((v, ctx) => {
      if (v.multirange.trim() !== '' && !pgAtLeast(server, 14)) custom(ctx, ['multirange'], 'Multirange types need PostgreSQL 14 or newer.');
    });
}

/** A range type is created once; afterwards only its identity (name, schema, owner, comment) changes. */
export function buildPgRange(form: PgRangeForm, original: PgRangeType | null, database: string | null): EngineChange[] {
  if (!original) {
    const name = qn(form.schema, form.name);
    const opt = (t: string): QualifiedName | null => (textOrNull(t) ? parseQualified(t) : null);
    const out: EngineChange[] = [
      pg({
        object: 'type',
        action: 'createRange',
        name,
        subtype: form.subtype.trim(),
        subtypeOpclass: opt(form.subtypeOpclass),
        collation: textOrNull(form.collation),
        canonical: opt(form.canonical),
        subtypeDiff: opt(form.subtypeDiff),
        multirangeTypeName: opt(form.multirange),
        comment: textOrNull(form.comment),
      }),
    ];
    if (textOrNull(form.owner)) out.push(pg({ object: 'type', action: 'owner', name, owner: roleSpec(form.owner) }));
    return out;
  }
  return typeIdentity(form, original.schema, original.name, original.meta, 'rangeType', database);
}

/** Dropping any of the PostgreSQL types or a domain. */
export function buildPgTypeDrop(kind: ObjectKind, schema: string, name: string, cascade: boolean): EngineChange {
  const target = qn(schema, name);
  const behavior = cascade ? 'cascade' : 'restrict';
  return kind === 'domain' ? pg({ object: 'domain', action: 'drop', name: target, behavior }) : pg({ object: 'type', action: 'drop', name: target, behavior });
}

// ---------------------------------------------------------------------------
// Extensions
// ---------------------------------------------------------------------------

export interface ExtensionInstallForm {
  name: string;
  schema: string;
  version: string;
  cascade: boolean;
}

export const extensionInstallSchema = z.object({
  name: z.string().refine((v) => v.trim() !== '', 'Enter the extension name.'),
  schema: z.string(),
  version: z.string(),
  cascade: z.boolean(),
});

export function buildExtensionInstall(form: ExtensionInstallForm): EngineChange {
  return pg({ object: 'extension', action: 'create', name: form.name.trim(), schema: textOrNull(form.schema), version: textOrNull(form.version), cascade: form.cascade });
}

export function buildExtensionUpdate(name: string, version: string | null): EngineChange {
  return pg({ object: 'extension', action: 'update', name, version });
}

export function buildExtensionSchema(name: string, schema: string): EngineChange {
  return pg({ object: 'extension', action: 'setSchema', name, schema: schema.trim() });
}

export function buildExtensionDrop(name: string, cascade: boolean): EngineChange {
  return pg({ object: 'extension', action: 'drop', name, behavior: cascade ? 'cascade' : 'restrict' });
}

// ---------------------------------------------------------------------------
// Drops of the other kinds
// ---------------------------------------------------------------------------

/** The drop change of an object an editor shows, or `null` when the editor has no drop for it. */
export function buildDrop(engine: Engine, object: ObjectRef, cascade: boolean, options: { force?: boolean } = {}): EngineChange | null {
  const behavior = cascade ? 'cascade' : 'restrict';
  if (engine === 'postgres') {
    const name = qn(object.schema, object.name);
    switch (object.kind) {
      case 'table':
      case 'partitionedTable':
      case 'partition':
        return pg({ object: 'table', action: 'drop', name, behavior });
      case 'schema':
        return pg({ object: 'schema', action: 'drop', name: object.name, behavior });
      case 'database':
        // PostgreSQL has no CASCADE here; FORCE (13+) is a separate, explicit choice.
        return pg({ object: 'database', action: 'drop', name: object.name, ...(options.force ? { force: true } : {}) });
      case 'rule':
        return object.parent ? pg({ object: 'rule', action: 'drop', name: object.name, table: qn(object.schema, object.parent), behavior }) : null;
      case 'policy':
        // Nothing depends on a policy: no CASCADE.
        return object.parent ? pg({ object: 'policy', action: 'drop', name: object.name, table: qn(object.schema, object.parent) }) : null;
      case 'view':
        return pg({ object: 'view', action: 'drop', name, behavior });
      case 'materializedView':
        return pg({ object: 'materializedView', action: 'drop', name, behavior });
      case 'sequence':
        return pg({ object: 'sequence', action: 'drop', name, behavior });
      case 'function':
      case 'procedure':
        return pg({ object: 'routine', action: 'drop', routine: object.kind, name, signature: object.signature ?? '', behavior });
      case 'trigger':
        return pg({ object: 'trigger', action: 'drop', name: object.name, table: qn(object.schema, object.parent ?? ''), behavior });
      case 'eventTrigger':
        return pg({ object: 'eventTrigger', action: 'drop', name: object.name, behavior });
      case 'enumType':
      case 'compositeType':
      case 'rangeType':
      case 'domain':
        return buildPgTypeDrop(object.kind, object.schema ?? '', object.name, cascade);
      case 'extension':
        return buildExtensionDrop(object.name, cascade);
      default:
        return null;
    }
  }
  const database = object.database ?? '';
  switch (object.kind) {
    case 'database':
      return my({ change: 'dropDatabase', name: object.name });
    case 'table':
      return my({ change: 'dropTable', database, name: object.name });
    case 'view':
      return my({ change: 'dropView', database, name: object.name });
    case 'function':
    case 'procedure':
      return my({ change: 'dropRoutine', database, name: object.name, routine: object.kind });
    case 'trigger':
      return my({ change: 'dropTrigger', database, name: object.name });
    case 'event':
      return my({ change: 'dropEvent', database, name: object.name });
    case 'sequence':
      return my({ change: 'dropSequence', database, name: object.name });
    default:
      return null;
  }
}

// ---------------------------------------------------------------------------
// MariaDB system versioning and MySQL/MariaDB partition maintenance
// ---------------------------------------------------------------------------

export interface VersioningForm {
  /** `implicit`: MariaDB's hidden row_start/row_end; `explicit`: named columns. */
  columns: 'implicit' | 'explicit';
  start: string;
  end: string;
  /** TIMESTAMP(6) or BIGINT UNSIGNED (transaction-precise, InnoDB). */
  precision: 'timestamp' | 'transaction';
  invisible: boolean;
}

export const versioningSchema = z
  .object({
    columns: z.enum(['implicit', 'explicit']),
    start: z.string(),
    end: z.string(),
    precision: z.enum(['timestamp', 'transaction']),
    invisible: z.boolean(),
  })
  .superRefine((v, ctx) => {
    if (v.columns !== 'explicit') return;
    if (v.start.trim() === '') custom(ctx, ['start'], 'Enter the row start column name.');
    if (v.end.trim() === '') custom(ctx, ['end'], 'Enter the row end column name.');
    if (v.start.trim() !== '' && v.start.trim() === v.end.trim()) custom(ctx, ['end'], 'Use two different columns.');
  });

export function buildAddVersioning(database: string, table: string, form: VersioningForm): EngineChange {
  const type: MyDataType = form.precision === 'timestamp' ? { name: 'timestamp', length: 6 } : { name: 'bigint', unsigned: true };
  const column = (name: string, rowPeriod: 'start' | 'end') => ({ name: name.trim(), dataType: type, invisible: form.invisible, rowPeriod });
  const op: MyAlterOp = {
    op: 'addSystemVersioning',
    columns: form.columns === 'explicit' ? [column(form.start, 'start'), column(form.end, 'end')] : null,
  };
  return my({ change: 'alterTable', database, name: table, operations: [op] });
}

/** Dropping versioning deletes the history; explicit row columns are dropped with it. */
export function buildDropVersioning(database: string, table: string, rowStart: string | null, rowEnd: string | null, explicit: boolean): EngineChange {
  const columns = explicit && rowStart && rowEnd ? { start: rowStart, end: rowEnd } : null;
  return my({ change: 'alterTable', database, name: table, systemVersioned: true, operations: [{ op: 'dropSystemVersioning', columns }] });
}

export type PartitionAction =
  | { op: 'truncate'; names: string[] }
  | { op: 'drop'; names: string[] }
  | { op: 'coalesce'; count: number }
  | { op: 'addCount'; count: number }
  | { op: 'remove' };

export function buildPartitionAction(database: string, table: string, action: PartitionAction, systemVersioned: boolean): EngineChange {
  let op: MyAlterOp;
  switch (action.op) {
    case 'truncate':
      op = { op: 'truncatePartitions', names: action.names };
      break;
    case 'drop':
      op = { op: 'dropPartitions', names: action.names };
      break;
    case 'coalesce':
      op = { op: 'coalescePartitions', count: action.count };
      break;
    case 'addCount':
      op = { op: 'addPartitionCount', count: action.count };
      break;
    case 'remove':
      op = { op: 'removePartitioning' };
      break;
  }
  return my({ change: 'alterTable', database, name: table, systemVersioned, operations: [op] });
}

/** Which maintenance actions a partitioning method allows. */
export function partitionActions(method: string): { drop: boolean; coalesce: boolean; addCount: boolean } {
  const m = method.toUpperCase();
  const hashLike = m.includes('HASH') || m.includes('KEY');
  return { drop: !hashLike, coalesce: hashLike, addCount: hashLike };
}

// ---------------------------------------------------------------------------
// MariaDB system-versioned history queries (run through the SQL console path)
// ---------------------------------------------------------------------------

export type SystemTimeMode = 'asOf' | 'between' | 'fromTo' | 'all';

/** A MariaDB identifier in backticks. */
export function quoteMyIdent(name: string): string {
  return `\`${name.replace(/`/g, '``')}\``;
}

/** A MariaDB string literal (backslashes and quotes escaped). */
export function quoteMyString(value: string): string {
  return `'${value.replace(/\\/g, '\\\\').replace(/'/g, "''")}'`;
}

export interface HistoryQuery {
  mode: SystemTimeMode;
  from: string;
  to: string;
  where: string;
  limit: number;
}

export const historyQuerySchema = z
  .object({ mode: z.enum(['asOf', 'between', 'fromTo', 'all']), from: z.string(), to: z.string(), where: z.string(), limit: z.number() })
  .superRefine((v, ctx) => {
    if (v.mode !== 'all') {
      if (!TIMESTAMP.test(v.from.trim())) custom(ctx, ['from'], 'Write it as YYYY-MM-DD HH:MM:SS.');
      if (v.mode !== 'asOf' && !TIMESTAMP.test(v.to.trim())) custom(ctx, ['to'], 'Write it as YYYY-MM-DD HH:MM:SS.');
      if (v.mode !== 'asOf' && TIMESTAMP.test(v.from.trim()) && TIMESTAMP.test(v.to.trim()) && v.to.trim() < v.from.trim()) {
        custom(ctx, ['to'], 'The end must not be before the start.');
      }
    }
    if (/;/.test(v.where)) custom(ctx, ['where'], 'Write one condition without a semicolon.');
    if (!Number.isInteger(v.limit) || v.limit < 1) custom(ctx, ['limit'], 'Enter a positive number of rows.');
  });

/** `SELECT ... FOR SYSTEM_TIME ...` for the history browser. `where` is the user's own SQL condition. */
export function historySelect(database: string, table: string, rowStart: string, rowEnd: string, keyColumns: readonly string[], q: HistoryQuery): string {
  const target = `${quoteMyIdent(database)}.${quoteMyIdent(table)}`;
  let period: string;
  switch (q.mode) {
    case 'asOf':
      period = `FOR SYSTEM_TIME AS OF TIMESTAMP ${quoteMyString(q.from.trim())}`;
      break;
    case 'between':
      period = `FOR SYSTEM_TIME BETWEEN TIMESTAMP ${quoteMyString(q.from.trim())} AND TIMESTAMP ${quoteMyString(q.to.trim())}`;
      break;
    case 'fromTo':
      period = `FOR SYSTEM_TIME FROM TIMESTAMP ${quoteMyString(q.from.trim())} TO TIMESTAMP ${quoteMyString(q.to.trim())}`;
      break;
    case 'all':
      period = 'FOR SYSTEM_TIME ALL';
      break;
  }
  const order = [...keyColumns.map(quoteMyIdent), quoteMyIdent(rowStart)].join(', ');
  const where = q.where.trim() === '' ? '' : ` WHERE ${q.where.trim()}`;
  return `SELECT *, ${quoteMyIdent(rowStart)}, ${quoteMyIdent(rowEnd)} FROM ${target} ${period}${where} ORDER BY ${order} LIMIT ${q.limit}`;
}

/** `DELETE HISTORY FROM t BEFORE SYSTEM_TIME '...'`. */
export function deleteHistorySql(database: string, table: string, before: string): string {
  return `DELETE HISTORY FROM ${quoteMyIdent(database)}.${quoteMyIdent(table)} BEFORE SYSTEM_TIME TIMESTAMP ${quoteMyString(before.trim())}`;
}

/** Names of the columns that differ between two rows (by column index). */
export function changedColumns(columns: readonly string[], a: readonly string[], b: readonly string[], skip: readonly string[]): string[] {
  return columns.filter((c, i) => !skip.includes(c) && a[i] !== b[i]);
}

/** Whether a MariaDB table is versioned with explicit (named, visible in describe) row columns. */
export function explicitVersioning(columns: readonly { name: string; generated: { generatedKind: string } | null }[]): { start: string; end: string } | null {
  const start = columns.find((c) => c.generated?.generatedKind === 'rowStart');
  const end = columns.find((c) => c.generated?.generatedKind === 'rowEnd');
  return start && end ? { start: start.name, end: end.name } : null;
}
