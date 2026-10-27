import type * as RecordsModule from '../../api/records';
import type * as SessionsModule from '../../api/sessions';
import type { Capabilities, Container, KindCount, ObjectKind, ObjectSummary } from '../../api/catalog';
import type { ConnectionProfile } from '../../api/records';
import type { SessionInfo } from '../../api/sessions';
import { useTabs } from '../../composables/useTabs';
import type { OverviewTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { createOverviewLoader } from './overviewLoader';

function profile(id: string, engine: ConnectionProfile['engine']): ConnectionProfile {
  return {
    id,
    name: id,
    engine,
    environment: 'development',
    folderId: null,
    database: null,
    ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
    options: { redisDb: 0, showSystemObjects: false },
    savedSecrets: [],
  } as unknown as ConnectionProfile;
}

const pgProfile = profile('pg', 'postgres');
const myProfile = profile('my', 'mysql');
let sessionDatabase: string | null = 'shop';
let sessions = 0;

function session(connectionId: string): SessionInfo {
  sessions += 1;
  const engine = connectionId === 'pg' ? 'postgres' : 'mysql';
  return {
    id: `s${sessions}`,
    connectionId,
    engine,
    server: { engine, version: '1', major: 1, minor: 0, patch: 0 },
    environment: 'development',
    database: connectionId === 'pg' ? sessionDatabase : 'crm',
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
}

function summary(kind: ObjectKind, name: string, system = false): ObjectSummary {
  return {
    object: { kind, database: 'shop', schema: 'public', parent: null, name, signature: null },
    comment: null,
    detail: null,
    flags: [],
    hasChildren: false,
    system,
    rowEstimate: null,
    sizeBytes: null,
  };
}

const caps: Capabilities = { engine: 'postgres', kinds: [], editable: ['table'], features: [] };

/** Answers held back until released, to order racing requests. */
const held: (() => void)[] = [];
let holdLists = false;

const catalogCapabilities = vi.fn(async () => caps);
const catalogSchemas = vi.fn(async (): Promise<ObjectSummary[]> => [
  summary('schema', 'pg_catalog', true),
  summary('schema', 'sales'),
  summary('schema', 'public'),
]);
const catalogDatabases = vi.fn(async (): Promise<ObjectSummary[]> => [summary('database', 'mysql', true), summary('database', 'shop'), summary('database', 'crm')]);
const catalogKinds = vi.fn(async (_s: string, _c: Container): Promise<KindCount[]> => [
  { kind: 'table', count: 2 },
  { kind: 'view', count: 1 },
  { kind: 'schema', count: 3 },
]);
const catalogList = vi.fn(
  (_s: string, container: Container, kind: ObjectKind): Promise<ObjectSummary[]> =>
    new Promise((resolve) => {
      const answer = [summary(kind, `${container.schema ?? container.database}.${kind}`)];
      if (holdLists) held.push(() => resolve(answer));
      else resolve(answer);
    }),
);

vi.mock('../../api/catalog', async (original) => ({
  ...(await original<object>()),
  catalogCapabilities: () => catalogCapabilities(),
  catalogSchemas: () => catalogSchemas(),
  catalogDatabases: () => catalogDatabases(),
  catalogKinds: (s: string, c: Container) => catalogKinds(s, c),
  catalogList: (s: string, c: Container, k: ObjectKind) => catalogList(s, c, k),
}));
vi.mock('../../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: async (id: string) => session(id),
  openDatabaseSession: async (id: string) => session(id),
  closeSession: vi.fn(async () => undefined),
}));
vi.mock('../../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [pgProfile, myProfile], queries: [] })),
}));
vi.mock('../../composables/useConnect', () => ({
  withConnectPrompts: <T>(_p: unknown, run: (secrets: object) => Promise<T>) => run({}),
}));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
async function settle(): Promise<void> {
  for (let i = 0; i < 6; i += 1) await tick();
}

async function openTab(p: ConnectionProfile): Promise<OverviewTab> {
  const tabs = useTabs();
  const tab = tabs.openOverviewTab(p);
  await settle();
  const ready = tabs.tabs.find((t) => t.id === tab.id);
  if (ready?.kind !== 'overview' || !ready.session) throw new Error('the overview tab did not connect');
  return ready;
}

describe('overview loading', () => {
  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  beforeEach(async () => {
    const tabs = useTabs();
    for (const t of [...tabs.tabs]) await tabs.closeTab(t.id);
    for (const fn of [catalogCapabilities, catalogSchemas, catalogDatabases, catalogKinds, catalogList]) fn.mockClear();
    held.length = 0;
    holdLists = false;
    sessionDatabase = 'shop';
  });

  it('starts PostgreSQL on schema public and lists each kind once', async () => {
    const tab = await openTab(pgProfile);
    const loader = createOverviewLoader(tab.id);
    await loader.refresh();
    expect(tab.scope.schema).toBe('public');
    expect(loader.state.choices.map((c) => c.object.name)).toEqual(['sales', 'public']);
    expect(catalogKinds).toHaveBeenCalledTimes(1);
    expect(catalogKinds.mock.calls[0]?.[1]).toEqual({ database: 'shop', schema: 'public', parent: null });
    // One list per kind present, never one per object, and never for schemas.
    expect(catalogList.mock.calls.map((c) => c[2])).toEqual(['table', 'view']);
    expect(loader.state.rows.map((r) => r.name)).toEqual(['public.table', 'public.view']);
    expect(loader.state.capabilities).toEqual(caps);
  });

  it('starts MySQL on the session default database', async () => {
    const tab = await openTab(myProfile);
    const loader = createOverviewLoader(tab.id);
    await loader.refresh();
    expect(tab.scope.database).toBe('crm');
    expect(catalogSchemas).not.toHaveBeenCalled();
    expect(catalogKinds.mock.calls[0]?.[1]).toEqual({ database: 'crm', schema: null, parent: null });
  });

  it('never asks for the same container twice, also while the first list is on its way', async () => {
    const tab = await openTab(pgProfile);
    const loader = createOverviewLoader(tab.id);
    await loader.prepare();
    await Promise.all([loader.load(), loader.load()]);
    await loader.load();
    expect(catalogKinds).toHaveBeenCalledTimes(1);
    // A refresh asks again, once.
    await loader.refresh();
    expect(catalogKinds).toHaveBeenCalledTimes(2);
  });

  it('keeps the rows of the newest container when an older list answers last', async () => {
    const tab = await openTab(pgProfile);
    const loader = createOverviewLoader(tab.id);
    await loader.prepare();
    holdLists = true;
    const first = loader.load();
    await tick();
    useTabs().setOverviewScope(tab.id, { schema: 'sales' });
    const second = loader.load();
    await tick();
    expect(held.length).toBe(4);
    // Release the second container's lists first, then the first's.
    for (const release of held.splice(2)) release();
    await second;
    for (const release of held.splice(0)) release();
    await first;
    expect(loader.state.rows.map((r) => r.name)).toEqual(['sales.table', 'sales.view']);
    expect(loader.state.loading).toBe(false);
  });

  it('keeps a failed picker read visible after the list succeeded, and the capabilities of a later read', async () => {
    const tab = await openTab(pgProfile);
    useTabs().setOverviewScope(tab.id, { schema: 'public' });
    const loader = createOverviewLoader(tab.id);
    catalogSchemas.mockRejectedValueOnce({ kind: 'catalog', error: { kind: 'unexpected', message: 'schemas failed' } });
    await loader.refresh();
    expect(loader.state.rows.length).toBe(2);
    expect(loader.state.prepareError).toBe('schemas failed');
    // A list after a prepare never throws away what the prepare read.
    const again = loader.prepare();
    await loader.load(true);
    await again;
    expect(loader.state.capabilities).toEqual(caps);
    expect(loader.state.prepareError).toBeNull();
  });

  it('does not take a database list for schemas when the PostgreSQL session names no database', async () => {
    sessionDatabase = null;
    const tab = await openTab(pgProfile);
    const loader = createOverviewLoader(tab.id);
    await loader.refresh();
    expect(catalogDatabases).not.toHaveBeenCalled();
    expect(tab.scope.schema).toBeNull();
    expect(loader.state.prepareError).toMatch(/did not report its database/);
    expect(catalogKinds).not.toHaveBeenCalled();
  });

  it('never shows rows of the old container after a failed list, and drops the error when switching back', async () => {
    const tab = await openTab(pgProfile);
    const loader = createOverviewLoader(tab.id);
    await loader.refresh();
    expect(loader.state.rows.map((r) => r.name)).toEqual(['public.table', 'public.view']);
    useTabs().setOverviewScope(tab.id, { schema: 'sales' });
    catalogKinds.mockRejectedValueOnce({ kind: 'catalog', error: { kind: 'unexpected', message: 'kinds failed' } });
    await loader.load();
    expect(loader.state.rows).toEqual([]);
    expect(loader.state.loadError).toBe('kinds failed');
    useTabs().setOverviewScope(tab.id, { schema: 'public' });
    await loader.load();
    expect(loader.state.loadError).toBeNull();
    expect(loader.state.rows.map((r) => r.name)).toEqual(['public.table', 'public.view']);
  });

  it('keeps the capabilities when the picker list fails', async () => {
    const tab = await openTab(pgProfile);
    const loader = createOverviewLoader(tab.id);
    catalogSchemas.mockRejectedValueOnce({ kind: 'catalog', error: { kind: 'unexpected', message: 'schemas failed' } });
    await loader.prepare();
    expect(loader.state.prepareError).toBe('schemas failed');
    expect(loader.state.capabilities).toEqual(caps);
  });

  it('starts on a database the picker shows when the session default is a hidden system database', async () => {
    catalogDatabases.mockResolvedValueOnce([summary('database', 'mysql', true), summary('database', 'shop'), summary('database', 'crm')]);
    const tab = await openTab(myProfile);
    if (tab.session) tab.session.database = 'mysql';
    const loader = createOverviewLoader(tab.id);
    await loader.prepare();
    expect(tab.scope.database).toBe('shop');
    expect(loader.state.choices.map((c) => c.object.name)).toContain(tab.scope.database);
  });

  it('returns rows only for the container the tab shows now', async () => {
    const tab = await openTab(pgProfile);
    const loader = createOverviewLoader(tab.id);
    await loader.refresh();
    expect(loader.rows().map((r) => r.name)).toEqual(['public.table', 'public.view']);
    useTabs().setOverviewScope(tab.id, { schema: 'sales' });
    // Not listed yet: nothing, never public's rows under sales.
    expect(loader.rows()).toEqual([]);
    await loader.load();
    expect(loader.rows().map((r) => r.name)).toEqual(['sales.table', 'sales.view']);
  });
});

