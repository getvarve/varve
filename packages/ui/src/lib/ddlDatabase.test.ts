import type { MyDatabase, PgDatabase, PgSchema } from '../api/ddl';
import type { ServerInfo } from '../api/sessions';
import {
  buildMyDatabase,
  buildPgDatabase,
  buildPgDatabaseDrop,
  buildPgSchema,
  connectionLimitProblem,
  mariadbAt,
  myDatabaseErrors,
  myDatabaseFeatures,
  myDatabaseFormFrom,
  mysqlAt,
  needsDatabaseToItself,
  pgDatabaseErrors,
  pgDatabaseFormFrom,
  pgNameProblem,
  pgSchemaErrors,
  pgSchemaFormFrom,
  settingsProblem,
} from './ddlDatabase';

const pg13: ServerInfo = { engine: 'postgres', version: '13.20', major: 13, minor: 20, patch: null };
const pg18: ServerInfo = { engine: 'postgres', version: '18.1', major: 18, minor: 1, patch: null };
const my80: ServerInfo = { engine: 'mysql', version: '8.0.15', major: 8, minor: 0, patch: 15 };
const my97: ServerInfo = { engine: 'mysql', version: '9.7.0', major: 9, minor: 7, patch: 0 };
const ma106: ServerInfo = { engine: 'mariadb', version: '10.6.20-MariaDB', major: 10, minor: 6, patch: 20 };

function pgDb(extra: Partial<PgDatabase> = {}): PgDatabase {
  return {
    name: 'shop',
    owner: 'app',
    encoding: 'UTF8',
    collation: 'C',
    ctype: 'C',
    localeProvider: 'libc',
    locale: null,
    icuRules: null,
    collationVersion: null,
    isTemplate: false,
    allowConnections: true,
    connectionLimit: -1,
    tablespace: 'pg_default',
    sizeBytes: 8_000_000,
    settings: [
      { role: null, name: 'work_mem', value: '64MB' },
      { role: 'app', name: 'search_path', value: 'app' },
    ],
    comment: null,
    system: false,
    ...extra,
  };
}

describe('PostgreSQL database form', () => {
  it('creates with only the options set, and settings as a second change', () => {
    const form = { ...pgDatabaseFormFrom(null), name: 'm2_ed_db', owner: 'app', template: 'template0', encoding: 'UTF8', localeProvider: 'icu' as const, icuLocale: 'nl-NL', connectionLimit: '5', comment: 'Shop data', settings: 'work_mem = 64MB' };
    expect(pgDatabaseErrors(form, true, pg18)).toEqual({});
    expect(buildPgDatabase(form, null)).toEqual([
      {
        engine: 'postgres',
        change: {
          object: 'database',
          action: 'create',
          name: 'm2_ed_db',
          owner: { kind: 'name', name: 'app' },
          template: 'template0',
          encoding: 'UTF8',
          localeProvider: 'icu',
          icuLocale: 'nl-NL',
          connectionLimit: 5,
          comment: 'Shop data',
        },
      },
      { engine: 'postgres', change: { object: 'database', action: 'alter', name: 'm2_ed_db', operations: [{ op: 'set', setting: { name: 'work_mem', values: ['64MB'] } }] } },
    ]);
  });

  it('refuses options the server version lacks and owner keywords on create', () => {
    const form = { ...pgDatabaseFormFrom(null), name: 'x', owner: 'current_user', strategy: 'fileCopy' as const, localeProvider: 'icu' as const, icuLocale: 'en' };
    const errors = pgDatabaseErrors(form, true, pg13);
    expect(errors.owner).toMatch(/role name/);
    expect(errors.strategy).toMatch(/15/);
    expect(errors.localeProvider).toMatch(/15/);
    expect(pgDatabaseErrors({ ...pgDatabaseFormFrom(null), name: 'x', localeProvider: 'builtin' }, true, pg18).builtinLocale).toMatch(/needs a locale/);
    expect(pgDatabaseErrors({ ...pgDatabaseFormFrom(null), name: 'x', icuLocale: 'en' }, true, pg18).icuLocale).toMatch(/icu provider/);
  });

  it('alters options, settings, tablespace, comment, then renames last', () => {
    const original = pgDb();
    const form = { ...pgDatabaseFormFrom(original), owner: 'CURRENT_USER', connectionLimit: '10', allowConnections: false, settings: 'statement_timeout = 5s', tablespace: 'fast', comment: 'c', name: 'store' };
    expect(pgDatabaseErrors(form, false, pg18)).toEqual({});
    expect(buildPgDatabase(form, original).map((c) => c.change)).toEqual([
      {
        object: 'database',
        action: 'alter',
        name: 'shop',
        operations: [
          { op: 'owner', owner: { kind: 'currentUser' } },
          { op: 'connectionLimit', limit: 10 },
          { op: 'allowConnections', allow: false },
          { op: 'set', setting: { name: 'statement_timeout', values: ['5s'] } },
          { op: 'reset', name: 'work_mem' },
        ],
      },
      { object: 'database', action: 'alter', name: 'shop', operations: [{ op: 'tablespace', tablespace: 'fast' }] },
      { object: 'comment', target: { kind: 'database', database: 'shop', schema: null, parent: null, name: 'shop', signature: null }, comment: 'c' },
      { object: 'database', action: 'alter', name: 'shop', operations: [{ op: 'rename', newName: 'store' }] },
    ]);
    expect(needsDatabaseToItself(form, original)).toBe(true);
    expect(needsDatabaseToItself(pgDatabaseFormFrom(original), original)).toBe(false);
  });

  it('keeps an unchanged form and per-role settings out of the plan', () => {
    const original = pgDb();
    expect(buildPgDatabase(pgDatabaseFormFrom(original), original)).toEqual([]);
    expect(pgDatabaseFormFrom(original).settings).toBe('work_mem = 64MB');
    // A blank owner keeps the owner.
    expect(buildPgDatabase({ ...pgDatabaseFormFrom(original), owner: '' }, original)).toEqual([]);
  });

  it('drops with FORCE only when chosen', () => {
    expect(buildPgDatabaseDrop('shop', false).change).toEqual({ object: 'database', action: 'drop', name: 'shop' });
    expect(buildPgDatabaseDrop('shop', true).change).toEqual({ object: 'database', action: 'drop', name: 'shop', force: true });
  });

  it('checks names, limits and settings', () => {
    expect(pgNameProblem('')).toMatch(/Enter/);
    expect(pgNameProblem('é'.repeat(32))).toMatch(/63 bytes/);
    expect(pgNameProblem('a'.repeat(63))).toBeNull();
    expect(connectionLimitProblem('-2')).toMatch(/-1/);
    expect(connectionLimitProblem('2147483648')).toMatch(/at most/);
    expect(connectionLimitProblem('x')).toMatch(/whole number/);
    expect(connectionLimitProblem('-1')).toBeNull();
    expect(settingsProblem('work_mem')).toMatch(/name = value/);
    expect(settingsProblem('work_mem = 1MB\nWORK_MEM = 2MB')).toMatch(/twice/);
    expect(settingsProblem('search_path = "$user", public')).toBeNull();
  });
});

describe('PostgreSQL schema form', () => {
  const original: PgSchema = { name: 'sales', owner: 'app', extension: null, comment: 'old', system: false };

  it('creates with owner and comment', () => {
    const form = { ...pgSchemaFormFrom(null), name: 'm2_ed_s', owner: 'app', comment: 'Sales' };
    expect(pgSchemaErrors(form, null)).toEqual({});
    expect(buildPgSchema(form, null, 'shop')[0]?.change).toEqual({ object: 'schema', action: 'create', name: 'm2_ed_s', owner: { kind: 'name', name: 'app' }, comment: 'Sales' });
    expect(buildPgSchema({ ...form, owner: '', comment: '' }, null, 'shop')[0]?.change).toEqual({ object: 'schema', action: 'create', name: 'm2_ed_s' });
  });

  it('alters owner and comment before renaming', () => {
    const form = { name: 'sales2', owner: 'SESSION_USER', comment: '' };
    expect(buildPgSchema(form, original, 'shop').map((c) => c.change)).toEqual([
      { object: 'schema', action: 'alter', name: 'sales', operations: [{ op: 'owner', owner: { kind: 'sessionUser' } }] },
      { object: 'comment', target: { kind: 'schema', database: 'shop', schema: null, parent: null, name: 'sales', signature: null }, comment: null },
      { object: 'schema', action: 'alter', name: 'sales', operations: [{ op: 'rename', newName: 'sales2' }] },
    ]);
  });

  it('refuses new pg_ names', () => {
    expect(pgSchemaErrors({ name: 'pg_x', owner: '', comment: '' }, null).name).toMatch(/reserved/);
  });
});

describe('MySQL and MariaDB database form', () => {
  const original: MyDatabase = {
    name: 'shop',
    charset: 'utf8mb4',
    collation: 'utf8mb4_0900_ai_ci',
    encryption: false,
    readOnly: false,
    comment: null,
    sizeBytes: 1024,
    tableCount: 2,
    system: false,
  };

  it('knows which options each version has', () => {
    expect(mysqlAt(my80, 8, 0, 16)).toBe(false);
    expect(mysqlAt(my97, 8, 0, 22)).toBe(true);
    expect(mariadbAt(ma106, 10, 5)).toBe(true);
    expect(myDatabaseFeatures(my97)).toEqual({ encryption: true, readOnly: true, comment: false });
    expect(myDatabaseFeatures(ma106)).toEqual({ encryption: false, readOnly: false, comment: true });
  });

  it('creates with the options given', () => {
    const form = { ...myDatabaseFormFrom(null), name: 'm2_ed_db', charset: 'utf8mb4', collation: 'utf8mb4_bin' };
    expect(myDatabaseErrors(form, null, myDatabaseFeatures(my97))).toEqual({});
    expect(buildMyDatabase(form, null)[0]?.change).toEqual({ change: 'createDatabase', name: 'm2_ed_db', charset: 'utf8mb4', collation: 'utf8mb4_bin' });
    const maria = { ...myDatabaseFormFrom(null), name: 'd', comment: 'Shop' };
    expect(buildMyDatabase(maria, null)[0]?.change).toEqual({ change: 'createDatabase', name: 'd', comment: 'Shop' });
  });

  it('alters only what changed', () => {
    const form = { ...myDatabaseFormFrom(original), collation: 'utf8mb4_bin', readOnly: 'on' as const };
    expect(buildMyDatabase(form, original)[0]?.change).toEqual({ change: 'alterDatabase', name: 'shop', collation: 'utf8mb4_bin', readOnly: true });
    expect(buildMyDatabase(myDatabaseFormFrom(original), original)).toEqual([]);
  });

  it('changes a read-only database only together with READ ONLY = 0', () => {
    const ro = { ...original, readOnly: true };
    const features = myDatabaseFeatures(my97);
    expect(myDatabaseErrors({ ...myDatabaseFormFrom(ro), charset: 'latin1' }, ro, features).readOnly).toMatch(/Writable/);
    const form = { ...myDatabaseFormFrom(ro), charset: 'latin1', readOnly: 'off' as const };
    expect(myDatabaseErrors(form, ro, features)).toEqual({});
    expect(buildMyDatabase(form, ro)[0]?.change).toEqual({ change: 'alterDatabase', name: 'shop', charset: 'latin1', readOnly: false });
  });

  it('refuses renames and options the server lacks', () => {
    const features = myDatabaseFeatures(ma106);
    expect(myDatabaseErrors({ ...myDatabaseFormFrom(original), name: 'other' }, original, features).name).toMatch(/cannot rename/);
    expect(myDatabaseErrors({ ...myDatabaseFormFrom(original), encryption: 'on' }, original, features).encryption).toMatch(/8\.0\.16/);
    expect(myDatabaseErrors({ ...myDatabaseFormFrom(null), name: 'x', comment: 'c' }, null, myDatabaseFeatures(my97)).comment).toMatch(/MariaDB 10\.5/);
    expect(myDatabaseErrors({ ...myDatabaseFormFrom(null), name: 'x', charset: 'utf8; DROP' }, null, features).charset).toMatch(/character set/);
  });
});
