import { flushPromises, mount } from '@vue/test-utils';
import type { VueWrapper } from '@vue/test-utils';
import type * as CatalogModule from '../../api/catalog';
import type * as DdlModule from '../../api/ddl';
import type { DropImpact, EngineChange, ObjectDetails, ObjectRef } from '../../api/ddl';
import type { ConnectionProfile } from '../../api/records';
import type { SessionInfo } from '../../api/sessions';
import { useTabs } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import type { WorkspaceTab } from '../../composables/useTabs';
import MyDatabaseEditor from './MyDatabaseEditor.vue';
import PgDatabaseEditor from './PgDatabaseEditor.vue';
import PolicyEditor from './PolicyEditor.vue';
import RuleEditor from './RuleEditor.vue';
import SchemaEditor from './SchemaEditor.vue';
import TableDropView from './TableDropView.vue';

const planned: EngineChange[] = [];
const applied: string[] = [];
const released: [string, string][] = [];
let details: Record<string, ObjectDetails> = {};
let impact: DropImpact = emptyImpact();
/** The backend asks for confirmation of every apply (destructive plans). */
let needConfirm = false;
const confirmations: { warnings?: string[] }[] = [];
let profile: ConnectionProfile | null = null;
const profileUpdates: unknown[] = [];
/** Order of the profile update and the tree refresh (`changed`). */
const events: string[] = [];
let profileUpdateError: unknown = null;

function emptyImpact(): DropImpact {
  return { dependents: [], dependentsTruncated: false, contents: [], sessions: [], sessionsComplete: true, blockers: [], appSessions: [], notes: [] };
}

vi.mock('../../api/ddl', async (original) => ({
  ...(await original<typeof DdlModule>()),
  catalogDescribe: vi.fn(async (_s: string, object: ObjectRef) => {
    const found = details[`${object.kind}:${object.name}`];
    if (!found) throw { kind: 'catalog', error: { kind: 'notFound', message: `${object.kind} ${object.name} not found` } };
    return found;
  }),
  editPlan: vi.fn(async (_s: string, change: EngineChange) => {
    planned.push(change);
    return { planId: `p${planned.length}`, plan: { statements: ['-- planned'], transactional: false, destructive: needConfirm, warnings: [] } };
  }),
  editApply: vi.fn(async (_s: string, planId: string, confirmed: boolean) => {
    if (needConfirm && !confirmed) throw { kind: 'session', error: { kind: 'confirmationRequired', environment: 'development', statements: ['Drop'] } };
    applied.push(planId);
    return { statements: 1, transactional: false, outputs: [], durationMs: 1 };
  }),
  catalogDropImpact: vi.fn(async () => impact),
}));
vi.mock('../../api/catalog', async (original) => ({
  ...(await original<typeof CatalogModule>()),
  catalogDatabases: vi.fn(async () => ['postgres', 'shop', 'crm'].map((name) => ({ object: { kind: 'database', database: name, schema: null, parent: null, name, signature: null } }))),
  catalogSchemas: vi.fn(async () => []),
}));
vi.mock('../tree/objectBrowser', () => ({
  useObjectBrowser: () => ({
    releaseDatabase: vi.fn(async (connectionId: string, database: string) => {
      released.push([connectionId, database]);
    }),
  }),
}));
vi.mock('../notices/useNotices', () => ({ useNotices: () => ({ refreshRestores: vi.fn(async () => undefined) }) }));
vi.mock('../../composables/useDialogs', () => ({
  useDialogs: () => ({
    askDestructive: vi.fn(async (request: { warnings?: string[] }) => {
      confirmations.push(request);
      // The tree sessions are released only after this answer.
      expect(released).toEqual([]);
      return true;
    }),
  }),
}));
vi.mock('../../composables/useWorkspace', async () => {
  const { ref: vueRef } = await import('vue');
  const records = vueRef({ connections: [], folders: [], queries: [] });
  return { useWorkspace: () => ({ records, connection: () => profile, refresh: vi.fn(async () => undefined) }) };
});
vi.mock('../../api/records', async (original) => ({
  ...(await original<object>()),
  updateConnection: vi.fn(async (id: string, input: unknown) => {
    profileUpdates.push({ id, input });
    events.push('profile');
    if (profileUpdateError) throw profileUpdateError;
    return profile;
  }),
}));

function session(engine: SessionInfo['engine'], extra: Partial<SessionInfo> = {}): SessionInfo {
  const major = engine === 'postgres' ? 18 : engine === 'mysql' ? 9 : 12;
  return {
    id: 's1',
    connectionId: 'c1',
    engine,
    server: { engine, version: `${major}.0`, major, minor: engine === 'mysql' ? 7 : 0, patch: 0 },
    environment: 'development',
    database: engine === 'postgres' ? 'postgres' : null,
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
    ...extra,
  } as SessionInfo;
}

const ref = (kind: ObjectRef['kind'], name: string, extra: Partial<ObjectRef> = {}): ObjectRef => ({ kind, database: 'shop', schema: null, parent: null, name, signature: null, ...extra });

const stubs = { SqlCode: true, DdlView: true };

/** Waits for the debounced plan (400 ms) and the promises after it. */
async function settle(): Promise<void> {
  await flushPromises();
  await new Promise((resolve) => setTimeout(resolve, 450));
  await flushPromises();
}

function input(wrapper: VueWrapper, label: string) {
  const el = wrapper.findAll('label').find((l) => l.text().startsWith(label));
  const id = el?.attributes('for');
  if (!id) throw new Error(`no field ${label}`);
  return wrapper.find(`[id="${id}"]`);
}

function button(wrapper: VueWrapper, text: string) {
  const found = wrapper.findAll('button').find((b) => b.text() === text);
  if (!found) throw new Error(`no button ${text}`);
  return found;
}

const lastChange = (): unknown => planned.at(-1)?.change;

beforeEach(() => {
  needConfirm = false;
  confirmations.length = 0;
  profile = null;
  profileUpdates.length = 0;
  events.length = 0;
  profileUpdateError = null;
  useTabs().tabs.splice(0);
  planned.length = 0;
  applied.length = 0;
  released.length = 0;
  impact = emptyImpact();
  details = {
    'database:shop': {
      engine: 'postgres',
      object: {
        kind: 'database',
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
        sizeBytes: 1024,
        settings: [],
        comment: null,
        system: false,
      },
    },
  };
});

describe('PostgreSQL database editor', () => {
  it('plans an alter from the form', async () => {
    const wrapper = mount(PgDatabaseEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: ref('database', 'shop') }, global: { stubs } });
    await settle();
    await input(wrapper, 'Connection limit').setValue('20');
    await settle();
    expect(lastChange()).toEqual({ object: 'database', action: 'alter', name: 'shop', operations: [{ op: 'connectionLimit', limit: 20 }] });
  });

  it('drops with the sessions listed, FORCE as a separate choice, and releases the tree sessions first', async () => {
    impact = { ...emptyImpact(), sessions: [{ id: '4242', user: 'app', application: 'psql', client: '10.0.0.5', state: 'idle', varve: false }] };
    const wrapper = mount(PgDatabaseEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: ref('database', 'shop'), intent: 'drop' }, global: { stubs } });
    await settle();
    expect(lastChange()).toEqual({ object: 'database', action: 'drop', name: 'shop' });
    expect(wrapper.text()).toContain('PostgreSQL refuses to drop a database while other sessions are connected');
    expect(wrapper.text()).toContain('4242');
    expect(wrapper.text()).not.toContain('CASCADE');
    await wrapper.find('label.force input').setValue(true);
    await settle();
    expect(lastChange()).toEqual({ object: 'database', action: 'drop', name: 'shop', force: true });
    expect(wrapper.text()).toContain('WITH (FORCE) terminates the 1 other session');
    await button(wrapper, 'Drop shop').trigger('click');
    await settle();
    expect(released).toEqual([['c1', 'shop']]);
    expect(applied).toHaveLength(1);
    expect(wrapper.emitted('dropped')?.[0]).toEqual([ref('database', 'shop')]);
  });

  it('runs a drop of its own database from another one', async () => {
    const wrapper = mount(PgDatabaseEditor, {
      props: { session: session('postgres', { database: 'shop' }), connectionName: 'Shop', object: ref('database', 'shop'), intent: 'drop' },
      global: { stubs },
    });
    await settle();
    expect(wrapper.text()).toContain('This tab is connected to shop');
    expect((button(wrapper, 'Drop shop').element as HTMLButtonElement).disabled).toBe(true);
    await input(wrapper, 'Run from').setValue('crm');
    await button(wrapper, 'Reconnect this tab there').trigger('click');
    expect(wrapper.emitted('switchDatabase')?.[0]).toEqual(['crm']);
  });

  it('refuses to run on a read-only session', async () => {
    const wrapper = mount(PgDatabaseEditor, { props: { session: session('postgres', { readOnly: true }), connectionName: 'Shop', object: ref('database', 'shop') }, global: { stubs } });
    await settle();
    await input(wrapper, 'Connection limit').setValue('20');
    await settle();
    expect(wrapper.text()).toContain('This connection is read only');
    expect((button(wrapper, 'Save').element as HTMLButtonElement).disabled).toBe(true);
    expect((button(wrapper, 'Drop').element as HTMLButtonElement).disabled).toBe(true);
  });
});

describe('database drop safety (audit fixes)', () => {
  it('names the tabs FORCE ends in the confirmation and releases tree sessions only after the yes', async () => {
    needConfirm = true;
    useTabs().tabs.push({ id: 't1', title: 'Shop query', session: { id: 'tab-1' } } as unknown as WorkspaceTab);
    impact = { ...emptyImpact(), appSessions: [{ sessionId: 'tab-1', busy: false, openTransaction: true, certain: true, standby: false }] };
    const wrapper = mount(PgDatabaseEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: ref('database', 'shop'), intent: 'drop' }, global: { stubs } });
    await settle();
    expect(wrapper.text()).toContain('refuses the drop until they are closed: "Shop query" (open transaction)');
    await wrapper.find('label.force input').setValue(true);
    await settle();
    await button(wrapper, 'Drop shop').trigger('click');
    await settle();
    expect(confirmations[0]?.warnings?.[0]).toMatch(/WITH \(FORCE\) also ends 1 tab .*"Shop query" \(open transaction\).*rolled back/);
    expect(released).toEqual([['c1', 'shop']]);
    expect(applied, wrapper.text()).toHaveLength(1);
  });

  it('warns about dropping the connection own database', async () => {
    profile = { id: 'c1', name: 'Shop', engine: 'postgres', database: 'shop', version: 1 } as unknown as ConnectionProfile;
    const wrapper = mount(PgDatabaseEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: ref('database', 'shop'), intent: 'drop' }, global: { stubs } });
    await settle();
    expect(wrapper.text()).toContain("shop is this connection's own database: after the drop");
  });

  it('points the connection at a renamed own database when chosen', async () => {
    profile = { id: 'c1', name: 'Shop', engine: 'postgres', database: 'shop', version: 1, folderId: null, color: null, host: 'h', port: 5432, username: 'u', environment: 'development', tls: {}, ssh: {}, options: {} } as unknown as ConnectionProfile;
    const wrapper = mount(PgDatabaseEditor, {
      props: { session: session('postgres'), connectionName: 'Shop', object: ref('database', 'shop') },
      attrs: { onChanged: () => events.push('changed') },
      global: { stubs },
    });
    await settle();
    await input(wrapper, 'Name').setValue('store');
    await settle();
    expect(wrapper.text()).toContain('reconnects fail until the connection uses the new name');
    await wrapper.findAll('label.check').find((l) => l.text().startsWith('Point the connection'))?.find('input').setValue(true);
    await button(wrapper, 'Save').trigger('click');
    await settle();
    expect(profileUpdates).toEqual([{ id: 'c1', input: expect.objectContaining({ database: 'store' }) }]);
    // The connection names the new database before the tree refreshes (else the refresh opens the old name).
    expect(events).toEqual(['profile', 'changed']);
  });

  it('reports a keychain error after switching the connection as a warning', async () => {
    profile = { id: 'c1', name: 'Shop', engine: 'postgres', database: 'shop', version: 1, folderId: null, color: null, host: 'h', port: 5432, username: 'u', environment: 'development', tls: {}, ssh: {}, options: {} } as unknown as ConnectionProfile;
    profileUpdateError = { kind: 'keychain', message: 'keychain locked' };
    const toasts = useToasts();
    const before = toasts.toasts.length;
    const wrapper = mount(PgDatabaseEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: ref('database', 'shop') }, global: { stubs } });
    await settle();
    await input(wrapper, 'Name').setValue('store');
    await settle();
    await wrapper.findAll('label.check').find((l) => l.text().startsWith('Point the connection'))?.find('input').setValue(true);
    await button(wrapper, 'Save').trigger('click');
    await settle();
    const added = toasts.toasts.slice(before);
    expect(added.map((t) => t.tone)).toEqual(['warning']);
    expect(added[0]?.title).toBe('The connection now uses store');
  });

  it('keeps the typed form and the drop in progress across a new session', async () => {
    const wrapper = mount(PgDatabaseEditor, { props: { session: session('postgres'), connectionName: 'Shop', object: ref('database', 'shop') }, global: { stubs } });
    await settle();
    await input(wrapper, 'Connection limit').setValue('7');
    await wrapper.setProps({ session: session('postgres', { id: 's2' }) });
    await settle();
    expect((input(wrapper, 'Connection limit').element as HTMLInputElement).value).toBe('7');
    await button(wrapper, 'Drop').trigger('click');
    await settle();
    await wrapper.setProps({ session: session('postgres', { id: 's3' }) });
    await settle();
    expect(wrapper.findAll('button').some((b) => b.text() === 'Drop shop')).toBe(true);
  });

  it('turns READ ONLY off as its own step before a MySQL read-only database can be dropped', async () => {
    details['database:ro'] = {
      engine: 'mysql',
      object: { kind: 'database', name: 'ro', charset: 'utf8mb4', collation: 'utf8mb4_bin', encryption: false, readOnly: true, comment: null, sizeBytes: 0, tableCount: 0, system: false },
    };
    const wrapper = mount(MyDatabaseEditor, { props: { session: session('mysql'), connectionName: 'Shop', object: ref('database', 'ro'), intent: 'drop' }, global: { stubs } });
    await settle();
    expect(wrapper.text()).toContain('MySQL refuses to drop it until that is turned off');
    expect((button(wrapper, 'Drop ro').element as HTMLButtonElement).disabled).toBe(true);
    await button(wrapper, 'Make it writable first').trigger('click');
    await settle();
    expect(lastChange()).toEqual({ change: 'alterDatabase', name: 'ro', readOnly: false });
  });
});

describe('MySQL database editor', () => {
  it('plans a new database', async () => {
    const wrapper = mount(MyDatabaseEditor, { props: { session: session('mysql'), connectionName: 'Shop', object: null, createKind: 'database' }, global: { stubs } });
    await settle();
    await input(wrapper, 'Name').setValue('m2_ed_db');
    await input(wrapper, 'Character set').setValue('utf8mb4');
    await settle();
    expect(lastChange()).toEqual({ change: 'createDatabase', name: 'm2_ed_db', charset: 'utf8mb4' });
  });

  it('drops with the connections and contents listed, without CASCADE', async () => {
    details['database:crm'] = {
      engine: 'mysql',
      object: { kind: 'database', name: 'crm', charset: 'utf8mb4', collation: 'utf8mb4_bin', encryption: null, readOnly: null, comment: null, sizeBytes: 0, tableCount: 2, system: false },
    };
    impact = { ...emptyImpact(), notes: ['Routines were searched only as far as this account can read them.'], contents: [{ label: 'tables', count: '2' }], sessions: [{ id: '17', user: 'app', application: null, client: '10.0.0.5:5000', state: 'Sleep', varve: false }] };
    const wrapper = mount(MyDatabaseEditor, { props: { session: session('mysql'), connectionName: 'Shop', object: ref('database', 'crm'), intent: 'drop' }, global: { stubs } });
    await settle();
    expect(lastChange()).toEqual({ change: 'dropDatabase', name: 'crm' });
    expect(wrapper.text()).toContain('Goes with it: 2 tables.');
    expect(wrapper.text()).toContain('Routines were searched only as far as this account can read them');
    expect(wrapper.text()).toContain('MySQL and MariaDB do not refuse');
    expect(wrapper.text()).not.toContain('CASCADE');
  });
});

describe('schema editor', () => {
  it('plans a new schema in the container database', async () => {
    const wrapper = mount(SchemaEditor, {
      props: { session: session('postgres', { database: 'shop' }), connectionName: 'Shop', object: null, createKind: 'schema', container: { database: 'shop', schema: null, parent: null } },
      global: { stubs },
    });
    await settle();
    await input(wrapper, 'Name').setValue('m2_ed_s');
    await settle();
    expect(lastChange()).toEqual({ object: 'schema', action: 'create', name: 'm2_ed_s' });
  });

  it('drops with CASCADE and shows what depends on it', async () => {
    details['schema:sales'] = { engine: 'postgres', object: { kind: 'schema', name: 'sales', owner: 'app', extension: null, comment: null, system: false } };
    impact = { ...emptyImpact(), contents: [{ label: 'tables', count: '3' }], dependents: [{ description: 'view public.sales_totals', effect: 'blocks' }] };
    const wrapper = mount(SchemaEditor, { props: { session: session('postgres', { database: 'shop' }), connectionName: 'Shop', object: ref('schema', 'sales'), intent: 'drop' }, global: { stubs } });
    await settle();
    expect(lastChange()).toEqual({ object: 'schema', action: 'drop', name: 'sales', behavior: 'restrict' });
    expect(wrapper.text()).toContain('view public.sales_totals');
    expect(wrapper.text()).toContain('the drop fails unless you choose CASCADE');
    await wrapper.find('.drop input[type="checkbox"]').setValue(true);
    await settle();
    expect(lastChange()).toEqual({ object: 'schema', action: 'drop', name: 'sales', behavior: 'cascade' });
  });
});

describe('rule editor', () => {
  it('replaces the rule when its condition changes', async () => {
    details['rule:no_neg'] = {
      engine: 'postgres',
      object: {
        kind: 'rule',
        schema: 'app',
        table: 'orders',
        name: 'no_neg',
        event: 'insert',
        instead: true,
        enabled: 'origin',
        definition: 'CREATE RULE no_neg AS\n    ON INSERT TO app.orders\n   WHERE (new.amount < 0) DO INSTEAD NOTHING',
        comment: null,
      },
    };
    const wrapper = mount(RuleEditor, {
      props: { session: session('postgres', { database: 'shop' }), connectionName: 'Shop', object: ref('rule', 'no_neg', { schema: 'app', parent: 'orders' }) },
      global: { stubs },
    });
    await settle();
    expect((input(wrapper, 'Condition').element as HTMLTextAreaElement).value).toBe('new.amount < 0');
    await input(wrapper, 'Condition').setValue('new.amount <= 0');
    await settle();
    expect(lastChange()).toEqual({
      object: 'rule',
      action: 'create',
      name: 'no_neg',
      table: { schema: 'app', name: 'orders' },
      orReplace: true,
      event: 'insert',
      condition: 'new.amount <= 0',
      instead: true,
      actions: [],
    });
  });
});

describe('policy editor', () => {
  it('creates a policy on its table and turns row level security on', async () => {
    details['table:orders'] = { engine: 'postgres', object: { kind: 'table', rlsEnabled: false, rlsForced: false } as unknown as DdlModule.PgObject };
    const wrapper = mount(PolicyEditor, {
      props: {
        session: session('postgres', { database: 'shop' }),
        connectionName: 'Shop',
        object: null,
        createKind: 'policy',
        container: { database: 'shop', schema: 'app', parent: 'orders' },
      },
      global: { stubs },
    });
    await settle();
    await input(wrapper, 'Name').setValue('m2_ed_p');
    await input(wrapper, 'USING').setValue('owner = current_user');
    await settle();
    // Row level security stays off unless chosen, and the editor says the policy has no effect until then.
    expect(lastChange()).toEqual({ object: 'policy', action: 'create', name: 'm2_ed_p', table: { schema: 'app', name: 'orders' }, restrictive: false, command: 'all', roles: [], using: 'owner = current_user', withCheck: null });
    expect(wrapper.text()).toContain('this policy has no effect until it is enabled');
    await wrapper.findAll('label.check').find((l) => l.text().startsWith('Enabled'))?.find('input').setValue(true);
    await settle();
    expect(planned.slice(-2).map((c) => c.change)).toEqual([
      { object: 'policy', action: 'create', name: 'm2_ed_p', table: { schema: 'app', name: 'orders' }, restrictive: false, command: 'all', roles: [], using: 'owner = current_user', withCheck: null },
      { object: 'table', action: 'alter', name: { schema: 'app', name: 'orders' }, operations: [{ op: 'rowLevelSecurity', enabled: true }] },
    ]);
  });
});

describe('table drop view', () => {
  it('plans the drop, lists dependents and offers CASCADE on PostgreSQL', async () => {
    impact = { ...emptyImpact(), dependents: [{ description: 'constraint lines_order_fk on table app.lines', effect: 'blocks' }], contents: [{ label: 'triggers', count: '1' }] };
    const wrapper = mount(TableDropView, { props: { session: session('postgres', { database: 'shop' }), connectionName: 'Shop', object: ref('table', 'orders', { schema: 'app' }) }, global: { stubs } });
    await settle();
    expect(lastChange()).toEqual({ object: 'table', action: 'drop', name: { schema: 'app', name: 'orders' }, behavior: 'restrict' });
    expect(wrapper.text()).toContain('constraint lines_order_fk on table app.lines');
    expect(wrapper.text()).toContain('Goes with it: 1 trigger.');
    await wrapper.find('.drop input[type="checkbox"]').setValue(true);
    await settle();
    expect(lastChange()).toEqual({ object: 'table', action: 'drop', name: { schema: 'app', name: 'orders' }, behavior: 'cascade' });
    await button(wrapper, 'Drop app.orders').trigger('click');
    await settle();
    expect(wrapper.emitted('dropped')).toHaveLength(1);
  });

  it('drops a MySQL table without CASCADE', async () => {
    const wrapper = mount(TableDropView, { props: { session: session('mysql'), connectionName: 'Shop', object: ref('table', 'orders') }, global: { stubs } });
    await settle();
    expect(lastChange()).toEqual({ change: 'dropTable', database: 'shop', name: 'orders' });
    expect(wrapper.find('.drop input[type="checkbox"]').exists()).toBe(false);
  });
});
