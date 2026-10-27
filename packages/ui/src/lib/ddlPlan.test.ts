import { describe, expect, it } from 'vitest';
import type {
  MyEvent,
  MyRoutine,
  MySequence,
  MyView,
  PgDomain,
  PgEnumType,
  PgFunction,
  PgMaterializedView,
  PgSequence,
  PgTrigger,
  PgView,
} from '../api/ddl';
import {
  buildDrop,
  buildMyEvent,
  buildMyRoutine,
  buildMySequence,
  buildMyView,
  buildPgDomain,
  buildPgEnum,
  buildPgMatView,
  buildPgRoutine,
  buildPgSequence,
  buildPgSequenceSetValue,
  buildMySequenceSetValue,
  pgSequenceSchema,
  pgSequenceGoesBack,
  pgSequenceState,
  mySequenceSchema,
  buildPgTrigger,
  buildPgView,
  concurrentRefreshProblem,
  createsOverload,
  deleteHistorySql,
  enumNeedsRecreate,
  errorsOf,
  historySelect,
  integerProblem,
  myEventFormFrom,
  myEventSchema,
  myRoutineFormFrom,
  mySequenceFormFrom,
  myTriggerSchema,
  myViewFormFrom,
  nextRuns,
  parseDefiner,
  parseMyDataType,
  pgDomainFormFrom,
  pgDomainSchema,
  pgEnumFormFrom,
  pgMatViewFormFrom,
  pgRoutineFormFrom,
  pgSequenceFormFrom,
  pgTriggerFormFrom,
  pgTriggerSchema,
  pgViewFormFrom,
  pgViewSchema,
  roleSpec,
  sqlStandardBody,
} from './ddlPlan';
import type { ServerTarget } from './ddlPlan';

const pg17: ServerTarget = { engine: 'postgres', major: 17, minor: 2 };
const pg13: ServerTarget = { engine: 'postgres', major: 13, minor: 18 };
const meta = { owner: 'migrator', comment: null, extension: null, system: false };

const view: PgView = {
  schema: 'public',
  name: 'active',
  owner: 'migrator',
  definition: ' SELECT 1 AS a;',
  checkOption: null,
  securityBarrier: true,
  securityInvoker: false,
  options: [],
  comment: 'Old',
  system: false,
  columns: [],
  triggers: [],
};

describe('PostgreSQL views', () => {
  it('creates a view and sets its owner afterwards', () => {
    const form = { ...pgViewFormFrom(null, 'public'), name: 'v', query: 'SELECT 1;', owner: 'app' };
    const changes = buildPgView(form, null, 'shop', pg13);
    expect(changes).toEqual([
      {
        engine: 'postgres',
        change: { object: 'view', action: 'create', name: { schema: 'public', name: 'v' }, orReplace: false, query: 'SELECT 1', checkOption: null, securityBarrier: null, securityInvoker: null, comment: null },
      },
      { engine: 'postgres', change: { object: 'view', action: 'alter', name: { schema: 'public', name: 'v' }, operations: [{ op: 'owner', owner: { kind: 'name', name: 'app' } }] } },
    ]);
  });

  it('replaces the definition with every option, then comment, then rename', () => {
    const form = { ...pgViewFormFrom(view, 'public'), query: 'SELECT 2 AS a', comment: '', name: 'active2' };
    const changes = buildPgView(form, view, 'shop', pg17);
    expect(changes.map((c) => (c.change as { object: string; action?: string }).object)).toEqual(['view', 'comment', 'view']);
    expect(changes[0]?.change).toMatchObject({ action: 'create', orReplace: true, securityBarrier: true, securityInvoker: false });
    expect(changes[1]?.change).toMatchObject({ object: 'comment', comment: null, target: { kind: 'view', name: 'active', database: 'shop' } });
    expect(changes[2]?.change).toMatchObject({ action: 'alter', name: { schema: 'public', name: 'active' }, operations: [{ op: 'rename', newName: 'active2' }] });
  });

  it('leaves security_invoker out before PostgreSQL 15 and uses ALTER for option-only edits', () => {
    const form = { ...pgViewFormFrom(view, 'public'), securityBarrier: false, checkOption: 'local' as const };
    const changes = buildPgView(form, view, null, pg13);
    expect(changes).toHaveLength(1);
    expect(changes[0]?.change).toMatchObject({ action: 'alter', operations: [{ op: 'setOptions', checkOption: 'local', securityBarrier: false }] });
    const replaced = buildPgView({ ...pgViewFormFrom(view, 'public'), query: 'SELECT 3' }, view, null, pg13);
    expect(replaced[0]?.change).toMatchObject({ securityInvoker: null });
  });

  it('has no changes for an untouched form and validates the query', () => {
    expect(buildPgView(pgViewFormFrom(view, 'public'), view, null, pg17)).toEqual([]);
    expect(errorsOf(pgViewSchema, { ...pgViewFormFrom(view, 'public'), query: '  ' })).toEqual({ query: 'Enter the SELECT that defines the view.' });
  });
});

describe('MySQL views', () => {
  const my: MyView = {
    database: 'cms',
    name: 'v',
    definition: 'select 1',
    algorithm: 'UNDEFINED',
    security: 'DEFINER',
    definer: 'cms@10.%',
    checkOption: 'NONE',
    updatable: true,
    charsetClient: 'utf8mb4',
    collationConnection: 'utf8mb4_0900_ai_ci',
    columns: [],
    system: false,
  };

  it('keeps the definer and replaces the view', () => {
    const form = { ...myViewFormFrom(my), query: 'select 2' };
    expect(form.definer).toBe('keep');
    expect(buildMyView(form, my, 'cms')).toEqual([
      {
        engine: 'mysql',
        change: {
          change: 'createView',
          database: 'cms',
          name: 'v',
          replace: true,
          algorithm: 'undefined',
          definer: { kind: 'account', user: 'cms', host: '10.%' },
          security: 'definer',
          query: 'select 2',
          checkOption: null,
        },
      },
    ]);
  });

  it('renames by creating the new view and dropping the old one', () => {
    const changes = buildMyView({ ...myViewFormFrom(my), name: 'w' }, my, 'cms');
    expect(changes.map((c) => (c.change as { change: string }).change)).toEqual(['createView', 'dropView']);
    expect(changes[0]?.change).toMatchObject({ replace: false, name: 'w' });
  });

  it('parses definers', () => {
    expect(parseDefiner('`root`@`%`')).toEqual({ user: 'root', host: '%' });
    expect(parseDefiner('nouser')).toBeNull();
  });
});

describe('materialized views', () => {
  const mv: PgMaterializedView = {
    schema: 'public',
    name: 'daily',
    owner: 'migrator',
    definition: 'SELECT 1 AS day;',
    populated: true,
    accessMethod: 'heap',
    tablespace: null,
    storageParameters: [],
    rowEstimate: 10,
    totalBytes: 100,
    sizeNote: null,
    lastRefreshNote: 'unknown',
    refreshConcurrently: true,
    comment: null,
    system: false,
    columns: [],
    indexes: [
      {
        schema: 'public',
        table: 'daily',
        name: 'daily_day_key',
        method: 'btree',
        unique: true,
        primary: false,
        valid: true,
        keys: [{ definition: 'day', expression: false, descending: false, nullsFirst: false }],
        includeColumns: [],
        predicate: null,
        sizeBytes: 8192,
        definition: 'CREATE UNIQUE INDEX daily_day_key ON public.daily USING btree (day)',
        comment: null,
      },
    ],
  };

  it('drops and recreates on a new query, carrying the indexes to the new name', () => {
    const form = { ...pgMatViewFormFrom(mv, 'public'), query: 'SELECT 2 AS day', name: 'daily2' };
    const built = buildPgMatView(form, mv, 'shop');
    expect(built.recreates).toBe(true);
    expect(built.changes.map((c) => (c.change as { action: string }).action)).toEqual(['drop', 'create', 'alter']);
    expect(built.indexes).toEqual(['CREATE UNIQUE INDEX daily_day_key ON public.daily2 USING btree (day)']);
  });

  it('refreshes for a changed WITH DATA and checks concurrent refresh', () => {
    const built = buildPgMatView({ ...pgMatViewFormFrom(mv, 'public'), withData: false }, mv, null);
    expect(built.changes[0]?.change).toMatchObject({ action: 'refresh', withData: false });
    expect(concurrentRefreshProblem(mv, true)).toBeNull();
    expect(concurrentRefreshProblem(mv, false)).toMatch(/WITH NO DATA/);
    expect(concurrentRefreshProblem({ ...mv, indexes: [] }, true)).toMatch(/unique index/);
  });
});

describe('PostgreSQL routines', () => {
  const fn: PgFunction = {
    schema: 'public',
    name: 'order_total',
    signature: 'p_order_id bigint',
    arguments: [{ name: 'p_order_id', mode: 'in', dataType: 'bigint', default: null }],
    result: 'numeric',
    returnType: 'numeric',
    returnsSet: false,
    window: false,
    language: 'plpgsql',
    volatility: 'stable',
    strict: true,
    securityDefiner: false,
    leakproof: false,
    parallel: 'safe',
    cost: 100,
    rows: null,
    config: ['search_path=public, pg_temp'],
    source: 'BEGIN RETURN 1; END;',
    library: null,
    sqlStandardBody: false,
    definition: '',
    meta,
  };

  it('alters attributes without recreating the function', () => {
    const form = { ...pgRoutineFormFrom(fn, 'function', 'public'), volatility: 'immutable' as const, settings: 'work_mem = 64MB' };
    const changes = buildPgRoutine(form, fn, 'shop');
    expect(changes).toHaveLength(1);
    expect(changes[0]?.change).toMatchObject({
      object: 'routine',
      action: 'alter',
      signature: 'p_order_id bigint',
      operations: [
        { op: 'volatility', volatility: 'immutable' },
        { op: 'reset', name: 'search_path' },
        { op: 'set', setting: { name: 'work_mem', values: ['64MB'] } },
      ],
    });
  });

  it('replaces on a body change and creates an overload on new argument types', () => {
    const body = buildPgRoutine({ ...pgRoutineFormFrom(fn, 'function', 'public'), body: 'BEGIN RETURN 2; END;' }, fn, null);
    expect(body[0]?.change).toMatchObject({ action: 'create', orReplace: true, settings: [{ name: 'search_path', values: ['public', 'pg_temp'] }] });
    const form = pgRoutineFormFrom(fn, 'function', 'public');
    form.args = [{ mode: 'in', name: 'p_order_id', dataType: 'integer', default: '' }];
    expect(createsOverload(form, fn)).toBe(true);
    expect(buildPgRoutine(form, fn, null)[0]?.change).toMatchObject({ action: 'create', orReplace: false });
  });

  it('reads SQL-standard bodies', () => {
    expect(sqlStandardBody('CREATE FUNCTION f()\n RETURNS integer\n LANGUAGE sql\nRETURN (1 + 1)\n')).toEqual({ kind: 'return', body: '(1 + 1)' });
    expect(sqlStandardBody('CREATE FUNCTION f()\n LANGUAGE sql\nBEGIN ATOMIC\n SELECT 1;\n SELECT 2;\nEND\n')).toEqual({ kind: 'atomic', body: 'SELECT 1;\nSELECT 2;' });
  });
});

describe('MySQL data types and routines', () => {
  it('parses column types', () => {
    expect(parseMyDataType('decimal(10,2) unsigned')).toEqual({ name: 'decimal', length: 10, scale: 2, unsigned: true });
    expect(parseMyDataType("enum('a','b''c')")).toEqual({ name: 'enum', values: ['a', "b'c"] });
    expect(parseMyDataType('varchar(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin')).toEqual({ name: 'varchar', length: 64, charset: 'utf8mb4', collation: 'utf8mb4_bin' });
    expect(parseMyDataType('INTEGER')).toEqual({ name: 'int' });
    expect(parseMyDataType('money')).toMatch(/not a type/);
    expect(parseMyDataType('enum')).toMatch(/list of quoted values/);
  });

  it('builds a procedure with modes and a function without', () => {
    const routine: MyRoutine = {
      database: 'cms',
      name: 'f',
      parameters: [{ position: 1, mode: null, name: 'x', dataType: 'int' }],
      returns: 'int',
      deterministic: true,
      dataAccess: 'READS SQL DATA',
      security: 'INVOKER',
      definer: 'root@%',
      sqlMode: '',
      comment: null,
      body: 'RETURN x',
      created: null,
      altered: null,
      charsetClient: null,
      collationConnection: null,
      databaseCollation: null,
    };
    const form = myRoutineFormFrom(routine, 'function');
    const change = buildMyRoutine({ ...form, body: 'RETURN x + 1;' }, routine, 'cms', 'mysql')[0]?.change;
    expect(change).toMatchObject({
      change: 'createRoutine',
      replace: true,
      parameters: [{ mode: null, name: 'x', dataType: { name: 'int' } }],
      returns: { name: 'int' },
      characteristics: { deterministic: true, dataAccess: 'readsSqlData', security: 'invoker' },
      body: 'RETURN x + 1',
    });
  });

  it('alters only the comment, data access or security in place, keeping the grants', () => {
    const routine: MyRoutine = {
      database: 'cms',
      name: 'p',
      parameters: [{ position: 1, mode: 'IN', name: 'a', dataType: 'int' }],
      returns: null,
      deterministic: false,
      dataAccess: 'CONTAINS SQL',
      security: 'DEFINER',
      definer: 'root@%',
      sqlMode: '',
      comment: 'old',
      body: 'BEGIN SELECT a; END',
      created: null,
      altered: null,
      charsetClient: null,
      collationConnection: null,
      databaseCollation: null,
    };
    const form = myRoutineFormFrom(routine, 'procedure');
    for (const engine of ['mysql', 'mariadb'] as const) {
      expect(buildMyRoutine(form, routine, 'cms', engine)).toEqual([]);
      // Stored as typed: leading and trailing spaces and line breaks are kept.
      expect(buildMyRoutine({ ...form, comment: ' new \n' }, routine, 'cms', engine).map((c) => c.change)).toEqual([
        { change: 'alterRoutine', database: 'cms', name: 'p', routine: 'procedure', comment: ' new \n' },
      ]);
      expect(buildMyRoutine({ ...form, comment: 'old ' }, routine, 'cms', engine).map((c) => c.change)).toEqual([
        { change: 'alterRoutine', database: 'cms', name: 'p', routine: 'procedure', comment: 'old ' },
      ]);
      expect(buildMyRoutine({ ...form, dataAccess: 'readsSqlData', security: 'invoker', body: 'BEGIN SELECT a; END;' }, routine, 'cms', engine).map((c) => c.change)).toEqual([
        { change: 'alterRoutine', database: 'cms', name: 'p', routine: 'procedure', dataAccess: 'readsSqlData', security: 'invoker' },
      ]);
      // Anything ALTER cannot change recreates the routine.
      for (const changed of [{ body: 'BEGIN SELECT a + 1; END' }, { deterministic: true }, { name: 'p2' }, { params: [{ mode: 'in' as const, name: 'a', dataType: 'bigint' }] }, { definer: 'currentUser' as const }]) {
        expect(buildMyRoutine({ ...form, comment: 'new', ...changed }, routine, 'cms', engine)[0]?.change).toMatchObject({ change: 'createRoutine', characteristics: { comment: 'new' } });
      }
    }
    // A recreate keeps the comment as typed too (CM-R2).
    expect(buildMyRoutine({ ...form, comment: ' new \n', body: 'BEGIN SELECT a + 1; END' }, routine, 'cms', 'mysql')[0]?.change).toMatchObject({
      change: 'createRoutine',
      characteristics: { comment: ' new \n' },
    });
    // Only security changed: no COMMENT even though the stored comment is all whitespace (CM-R3) or CRLF (CM-R4).
    for (const comment of ['  ', 'a\r\nb']) {
      const stored = { ...routine, comment };
      const typed = { ...myRoutineFormFrom(stored, 'procedure'), comment: comment === '  ' ? '' : 'a\nb', security: 'invoker' as const };
      expect(buildMyRoutine(typed, stored, 'cms', 'mysql').map((c) => c.change)).toEqual([
        { change: 'alterRoutine', database: 'cms', name: 'p', routine: 'procedure', security: 'invoker' },
      ]);
    }
    // Removing the comment: in place on MariaDB; MySQL ignores COMMENT '' in ALTER, so it recreates.
    expect(buildMyRoutine({ ...form, comment: '' }, routine, 'cms', 'mariadb').map((c) => c.change)).toEqual([
      { change: 'alterRoutine', database: 'cms', name: 'p', routine: 'procedure', comment: '' },
    ]);
    expect(buildMyRoutine({ ...form, comment: '' }, routine, 'cms', 'mysql')[0]?.change).toMatchObject({ change: 'createRoutine', replace: true });
  });
});

describe('triggers', () => {
  const trig: PgTrigger = {
    schema: 'public',
    table: 'orders',
    name: 'touch',
    timing: 'before',
    events: ['update'],
    updateColumns: ['total'],
    level: 'row',
    condition: null,
    function: 'public.touch_updated_at()',
    enabled: 'origin',
    constraintTrigger: false,
    deferrable: false,
    deferred: false,
    oldTable: null,
    newTable: null,
    inherited: false,
    definition: "CREATE TRIGGER touch BEFORE UPDATE OF total ON public.orders FOR EACH ROW EXECUTE FUNCTION touch_updated_at('x')",
    comment: null,
  };
  const table = { schema: 'public', name: 'orders', kind: 'table' as const };

  it('reads the form and replaces on PostgreSQL 14+', () => {
    const form = pgTriggerFormFrom(trig);
    expect(form.arguments).toEqual(['x']);
    expect(form.function).toBe('public.touch_updated_at');
    const changes = buildPgTrigger({ ...form, delete: true }, trig, table, 'shop', pg17);
    expect(changes[0]?.change).toMatchObject({ action: 'create', orReplace: true, function: { schema: 'public', name: 'touch_updated_at' }, arguments: ['x'] });
  });

  it('drops and creates on PostgreSQL 13, then sets the state and renames', () => {
    const changes = buildPgTrigger({ ...pgTriggerFormFrom(trig), delete: true, state: 'disabled', name: 'touch2' }, trig, table, null, pg13);
    expect(changes.map((c) => (c.change as { action: string }).action)).toEqual(['drop', 'create', 'setState']);
    expect(changes[1]?.change).toMatchObject({ name: 'touch2' });
  });

  it('validates trigger rules', () => {
    const base = pgTriggerFormFrom(trig);
    expect(errorsOf(pgTriggerSchema(false), { ...base, update: false })).toMatchObject({ events: 'Pick at least one event.' });
    expect(errorsOf(pgTriggerSchema(false), { ...base, timing: 'insteadOf' })).toMatchObject({ timing: 'INSTEAD OF triggers exist only on views.' });
    expect(errorsOf(pgTriggerSchema(false), { ...base, insert: true, condition: 'OLD.a <> NEW.a' })).toMatchObject({ condition: expect.stringContaining('OLD') });
    expect(errorsOf(myTriggerSchema('mysql'), { name: 't', timing: 'before', insert: true, update: true, delete: false, orderKind: '', orderTrigger: '', body: 'SET @a = 1', definer: 'currentUser', definerUser: '', definerHost: '%' })).toMatchObject({
      events: 'MySQL triggers fire on exactly one event.',
    });
  });
});

describe('sequences', () => {
  const seq: PgSequence = {
    schema: 'public',
    name: 'inv',
    owner: 'migrator',
    dataType: 'bigint',
    // 64-bit values arrive as exact decimal text.
    start: '1',
    increment: '1',
    min: '1',
    max: '9223372036854775807',
    cache: '1',
    cycle: false,
    lastValue: '10',
    isCalled: true,
    unlogged: false,
    ownedBy: null,
    identity: null,
    comment: null,
    system: false,
  };

  it('shows type defaults as blank and sends only changed options', () => {
    const form = pgSequenceFormFrom(seq, 'public');
    expect(form.max).toBe('');
    const changes = buildPgSequence({ ...form, cache: '20' }, seq, null);
    expect(changes).toEqual([{ engine: 'postgres', change: { object: 'sequence', action: 'alter', name: { schema: 'public', name: 'inv' }, operations: [{ op: 'options', options: { cache: '20' } }] } }]);
  });

  it('reads where the sequence stands from last_value and is_called, also after RESTART or setval', () => {
    expect(pgSequenceState(seq)).toEqual({ current: '10', next: '11' });
    // After RESTART WITH 500 (or setval(.., false)): nothing handed out since, 500 comes next.
    const restarted: PgSequence = { ...seq, lastValue: '500', isCalled: false };
    expect(pgSequenceState(restarted)).toEqual({ current: null, next: '500' });
    expect(pgSequenceGoesBack(restarted, 499n)).toBe(true);
    expect(pgSequenceGoesBack(restarted, 500n)).toBe(false);
    expect(pgSequenceGoesBack(seq, 11n)).toBe(false);
    expect(pgSequenceGoesBack(seq, 5n)).toBe(true);
    const descending: PgSequence = { ...seq, increment: '-1', lastValue: '-10', isCalled: true };
    expect(pgSequenceState(descending).next).toBe('-11');
    expect(pgSequenceGoesBack(descending, -5n)).toBe(true);
    expect(pgSequenceState({ ...seq, lastValue: null, isCalled: null })).toEqual({ current: null, next: null });
  });

  it('keeps bigint limits exact, so a comment-only change saves', () => {
    const big: PgSequence = { ...seq, max: '9223372036854775806', lastValue: '9007199254740993' };
    const form = pgSequenceFormFrom(big, 'public');
    expect(form.max).toBe('9223372036854775806');
    expect(errorsOf(pgSequenceSchema, { ...form, comment: 'ids' })).toEqual({});
    expect(buildPgSequence({ ...form, comment: 'ids' }, big, null)).toEqual([
      { engine: 'postgres', change: { object: 'comment', target: { kind: 'sequence', database: null, schema: 'public', parent: null, name: 'inv', signature: null }, comment: 'ids' } },
    ]);
    const lower = buildPgSequence({ ...form, max: '9223372036854775805', start: '-9223372036854775808', min: '-9223372036854775808' }, big, null);
    expect(lower[0]?.change).toMatchObject({
      operations: [{ op: 'options', options: { maxValue: { kind: 'value', value: '9223372036854775805' }, minValue: { kind: 'value', value: '-9223372036854775808' }, start: '-9223372036854775808' } }],
    });
    expect(buildPgSequenceSetValue(big, '9007199254740993', true).change).toMatchObject({ operations: [{ op: 'setValue', value: '9007199254740993' }] });
    expect(errorsOf(pgSequenceSchema, { ...form, max: '9223372036854775808' })).toMatchObject({ max: expect.stringMatching(/9223372036854775807/) });
    // A descending bigint sequence shows its type defaults as blank.
    const down = pgSequenceFormFrom({ ...seq, increment: '-1', min: '-9223372036854775808', max: '-1' }, 'public');
    expect([down.min, down.max]).toEqual(['', '']);
  });

  it('refuses integers JavaScript cannot send exactly', () => {
    expect(integerProblem('9223372036854775807', false)).toMatch(/cannot be sent/);
    expect(integerProblem('', true)).toBeNull();
    expect(integerProblem('1.5', true)).toBe('Enter a whole number.');
  });

  it('keeps unchanged MariaDB values out of ALTER SEQUENCE', () => {
    const my: MySequence = {
      database: 'd',
      name: 's',
      dataType: 'bigint',
      start: '1',
      increment: '1',
      minimum: '1',
      maximum: '9223372036854775806',
      cache: '1000',
      cycle: false,
      nextValue: '5',
      cycleCount: '0',
      lastValue: null,
      comment: null,
      engine: 'InnoDB',
    };
    expect(buildMySequence(mySequenceFormFrom(my), my, 'd')).toEqual([]);
    expect(buildMySequence({ ...mySequenceFormFrom(my), cache: '10' }, my, 'd')).toEqual([{ engine: 'mysql', change: { change: 'alterSequence', database: 'd', name: 's', cache: '10' } }]);
  });

  it('sends MariaDB values up to the unsigned 64-bit maximum exactly', () => {
    const my: MySequence = {
      database: 'd',
      name: 's',
      dataType: 'bigint unsigned',
      start: '1',
      increment: '1',
      minimum: '1',
      maximum: '18446744073709551614',
      cache: '1000',
      cycle: false,
      nextValue: '5',
      cycleCount: '0',
      lastValue: null,
      comment: null,
      engine: 'InnoDB',
    };
    const form = mySequenceFormFrom(my);
    expect(errorsOf(mySequenceSchema, { ...form, comment: 'x' })).toEqual({});
    expect(buildMySequence({ ...form, max: '18446744073709551615' }, my, 'd')).toEqual([
      { engine: 'mysql', change: { change: 'alterSequence', database: 'd', name: 's', maxValue: { kind: 'value', value: '18446744073709551615' } } },
    ]);
    expect(errorsOf(mySequenceSchema, { ...form, max: '18446744073709551616' })).toHaveProperty('max');
    expect(errorsOf(mySequenceSchema, { ...form, cache: '-1' })).toEqual({ cache: 'The cache cannot be negative.' });
    expect(buildMySequenceSetValue(my, '9007199254740993', true).change).toMatchObject({ value: '9007199254740993' });
  });
});

describe('events', () => {
  const event: MyEvent = {
    database: 'cms',
    name: 'purge',
    definer: 'cms@%',
    timeZone: 'SYSTEM',
    schedule: { type: 'every', intervalValue: '6', intervalField: 'HOUR', starts: '2026-01-12 00:00:00', ends: null },
    status: 'enabled',
    preserve: true,
    sqlMode: '',
    comment: 'Removes sessions',
    body: 'DELETE FROM s',
    created: null,
    altered: null,
    lastExecuted: null,
    originator: 1,
    scheduler: 'on',
  };

  it('alters only the schedule when the interval changes', () => {
    const changes = buildMyEvent({ ...myEventFormFrom(event), everyQuantity: '1' }, event, 'cms');
    expect(changes).toEqual([
      {
        engine: 'mysql',
        change: {
          change: 'alterEvent',
          database: 'cms',
          name: 'purge',
          schedule: { kind: 'every', interval: { quantity: '1', unit: 'hour' }, starts: { base: { kind: 'timestamp', value: '2026-01-12 00:00:00' } }, ends: null },
        },
      },
    ]);
    expect(buildMyEvent(myEventFormFrom(event), event, 'cms')).toEqual([]);
  });

  it('validates and projects the next runs', () => {
    expect(errorsOf(myEventSchema, { ...myEventFormFrom(event), everyQuantity: '0' })).toMatchObject({ everyQuantity: 'The interval must be at least 1.' });
    const runs = nextRuns({ ...myEventFormFrom(event), everyQuantity: '1' }, new Date('2026-01-12T05:30:00'), 3);
    expect(runs?.map((d) => d.getHours())).toEqual([6, 7, 8]);
  });
});

describe('enums', () => {
  const type: PgEnumType = { schema: 'public', name: 'status', labels: ['pending', 'paid'], meta };

  it('renames and adds values with their position', () => {
    const form = pgEnumFormFrom(type, 'public');
    form.values = [{ label: 'waiting', original: 'pending' }, { label: 'paid', original: 'paid' }, { label: 'refunded', original: null }];
    expect(enumNeedsRecreate(form, type)).toBeNull();
    expect(buildPgEnum(form, type, null).map((c) => c.change)).toEqual([
      { object: 'type', action: 'renameEnumValue', name: { schema: 'public', name: 'status' }, value: 'pending', newValue: 'waiting' },
      { object: 'type', action: 'addEnumValue', name: { schema: 'public', name: 'status' }, value: 'refunded', position: { kind: 'after', label: 'paid' } },
    ]);
  });

  it('refuses removing or moving existing values', () => {
    const form = pgEnumFormFrom(type, 'public');
    form.values = [{ label: 'paid', original: 'paid' }, { label: 'pending', original: 'pending' }];
    expect(enumNeedsRecreate(form, type)).toMatch(/reordered/);
    form.values = [{ label: 'paid', original: 'paid' }];
    expect(enumNeedsRecreate(form, type)).toMatch(/removed/);
  });
});

describe('drops and history', () => {
  it('builds drops per engine', () => {
    const ref = { kind: 'function' as const, database: 'shop', schema: 'public', parent: null, name: 'f', signature: 'a integer' };
    expect(buildDrop('postgres', ref, true)?.change).toMatchObject({ object: 'routine', action: 'drop', signature: 'a integer', behavior: 'cascade' });
    expect(buildDrop('mysql', { ...ref, kind: 'event', schema: null }, false)?.change).toEqual({ change: 'dropEvent', database: 'shop', name: 'f' });
    expect(buildDrop('postgres', { ...ref, kind: 'index' }, false)).toBeNull();
  });

  it('quotes names and literals in history queries', () => {
    const sql = historySelect('bill`ing', 'inv', 'row_start', 'row_end', ['id'], { mode: 'between', from: "2026-10-01 00:00:00", to: "2026-10-18 23:59:59", where: 'id = 1', limit: 50 });
    expect(sql).toBe(
      "SELECT *, `row_start`, `row_end` FROM `bill``ing`.`inv` FOR SYSTEM_TIME BETWEEN TIMESTAMP '2026-10-01 00:00:00' AND TIMESTAMP '2026-10-18 23:59:59' WHERE id = 1 ORDER BY `id`, `row_start` LIMIT 50",
    );
    expect(deleteHistorySql('d', 't', "2026-01-01' x")).toBe("DELETE HISTORY FROM `d`.`t` BEFORE SYSTEM_TIME TIMESTAMP '2026-01-01'' x'");
  });

  it('reads role names', () => {
    expect(roleSpec('current_user')).toEqual({ kind: 'currentUser' });
    expect(roleSpec(' app ')).toEqual({ kind: 'name', name: 'app' });
  });
});

describe('domain check comments', () => {
  const domain: PgDomain = {
    schema: 'shop',
    name: 'qty',
    baseType: 'integer',
    collation: null,
    default: null,
    notNull: false,
    constraints: [
      { name: 'positive', definition: 'CHECK (VALUE > 0)', validated: true, notNull: false, comment: 'no zero' },
      { name: 'small', definition: 'CHECK (VALUE < 1000)', validated: true, notNull: false, comment: null },
      { name: 'even', definition: 'CHECK (VALUE % 2 = 0)', validated: true, notNull: false, comment: 'pairs' },
    ],
    meta,
  };
  const alterOps = (changes: ReturnType<typeof buildPgDomain>): unknown[] =>
    changes.flatMap((c) => (c.engine === 'postgres' && 'operations' in c.change ? (c.change.operations as unknown[]) : []));

  it('reads each check comment and plans COMMENT ON CONSTRAINT ... ON DOMAIN after the renames', () => {
    const form = pgDomainFormFrom(domain, 'shop');
    expect(form.checks.map((c) => c.comment)).toEqual(['no zero', '', 'pairs']);
    expect(buildPgDomain(form, domain, 'db')).toEqual([]);
    const [positive, small, even] = form.checks;
    if (!positive || !small || !even) throw new Error('three checks');
    positive.comment = '';
    small.comment = 'below a thousand';
    small.name = 'tiny';
    even.expression = 'VALUE % 2 = 1';
    form.checks.push({ name: 'nonzero', expression: 'VALUE <> 0', original: null, validated: true, comment: 'new one' });
    expect(alterOps(buildPgDomain(form, domain, 'db'))).toEqual([
      { op: 'renameConstraint', name: 'small', newName: 'tiny' },
      { op: 'dropConstraint', name: 'even' },
      { op: 'addCheck', check: { name: 'even', expression: 'VALUE % 2 = 1' }, notValid: false },
      { op: 'addCheck', check: { name: 'nonzero', expression: 'VALUE <> 0' }, notValid: false },
      { op: 'commentConstraint', name: 'positive', comment: null },
      { op: 'commentConstraint', name: 'tiny', comment: 'below a thousand' },
      // A redefined check loses its comment with the drop: it is set again.
      { op: 'commentConstraint', name: 'even', comment: 'pairs' },
      { op: 'commentConstraint', name: 'nonzero', comment: 'new one' },
    ]);
  });

  it('comments named checks of a new domain, and asks for a name before commenting an unnamed one', () => {
    const form = pgDomainFormFrom(null, 'shop');
    Object.assign(form, { name: 'qty', dataType: 'integer' });
    form.checks.push({ name: 'positive', expression: 'VALUE > 0', original: null, validated: true, comment: 'no zero' });
    const changes = buildPgDomain(form, null, 'db');
    expect(changes[0]?.change).toMatchObject({ object: 'domain', action: 'create' });
    expect(changes[1]?.change).toEqual({ object: 'domain', action: 'alter', name: { schema: 'shop', name: 'qty' }, operations: [{ op: 'commentConstraint', name: 'positive', comment: 'no zero' }] });
    form.checks.push({ name: '', expression: 'VALUE < 10', original: null, validated: true, comment: 'unnamed' });
    expect(errorsOf(pgDomainSchema, form)).toMatchObject({ 'checks.1.name': 'Name the check to give it a comment.' });
  });
});
