import type * as RecordsModule from '../../api/records';
import type * as SessionsModule from '../../api/sessions';
import type { Capabilities, Container, KindCount, ObjectKind, ObjectSummary } from '../../api/catalog';
import type { ConnectionProfile } from '../../api/records';
import type { SessionInfo } from '../../api/sessions';
import { useWorkspace } from '../../composables/useWorkspace';
import { objectRootKey } from '../../lib/tree';
import type { ChildState, ObjNode } from '../../lib/tree';
import { useObjectBrowser } from './objectBrowser';

function profile(id: string, engine: ConnectionProfile['engine']): ConnectionProfile {
  return {
    id,
    workspaceId: 'personal',
    folderId: null,
    name: id,
    color: null,
    sortOrder: 0,
    engine,
    host: 'localhost',
    port: 5432,
    database: null,
    username: null,
    environment: 'development',
    tls: { mode: 'prefer', caPath: null, clientCertPath: null, clientKeyPath: null },
    ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
    options: {
      readOnly: false,
      confirmDestructive: true,
      statementTimeoutMs: null,
      keepaliveS: null,
      initSql: null,
      showSystemObjects: false,
      redisDb: 0,
    },
    savedSecrets: [],
    createdAt: 0,
    updatedAt: 0,
    version: 1,
  };
}

let connections = [profile('pg', 'postgres'), profile('my', 'mysql')];
let sessions = 0;

function session(connectionId: string, engine: SessionInfo['engine'], database: string | null): SessionInfo {
  sessions += 1;
  return {
    id: `s${sessions}`,
    connectionId,
    engine,
    server: { engine, version: '18.0', major: 18, minor: 0, patch: 0 },
    environment: 'development',
    database,
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
}

const openSession = vi.fn(async (id: string) => session(id, id === 'pg' ? 'postgres' : 'mysql', id === 'pg' ? 'shop' : null));
const openDatabaseSession = vi.fn(async (id: string, database: string) => session(id, 'postgres', database));
const closeSession = vi.fn(async () => undefined);

function summary(kind: ObjectKind, name: string, database: string | null = null, schema: string | null = null): ObjectSummary {
  return {
    object: { kind, database, schema, parent: null, name, signature: null },
    comment: null,
    detail: null,
    flags: [],
    hasChildren: false,
    system: false,
    rowEstimate: null,
    sizeBytes: null,
  };
}

const caps: Capabilities = { engine: 'postgres', kinds: ['database', 'schema', 'table'], editable: ['table'], features: [] };
const catalogKinds = vi.fn(async (_sessionId: string, container: Container, _includeSystem: boolean | null): Promise<KindCount[]> => {
  if (container.database === null) return [{ kind: 'database', count: 2 }];
  if (container.schema === null) return [{ kind: 'schema', count: 1 }];
  return [{ kind: 'table', count: 1 }];
});
let releaseList: (() => void) | null = null;
const catalogList = vi.fn(
  (_sessionId: string, container: Container, kind: ObjectKind) =>
    new Promise<ObjectSummary[]>((resolve) => {
      const answer = [summary(kind, 'orders', container.database, container.schema)];
      if (releaseList === null) resolve(answer);
      else releaseList = () => resolve(answer);
    }),
);
let databasesError: unknown = null;

vi.mock('../../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: (id: string) => openSession(id),
  openDatabaseSession: (id: string, database: string) => openDatabaseSession(id, database),
  closeSession: () => closeSession(),
}));
vi.mock('../../api/catalog', async (original) => ({
  ...(await original<object>()),
  catalogCapabilities: vi.fn(async () => caps),
  catalogDatabases: vi.fn(async () => {
    if (databasesError) throw databasesError;
    return [summary('database', 'shop'), summary('database', 'analytics')];
  }),
  catalogSchemas: vi.fn(async (_s: string, database: string) => [summary('schema', 'public', database)]),
  catalogKinds: (s: string, c: Container, i: boolean | null) => catalogKinds(s, c, i),
  catalogList: (s: string, c: Container, k: ObjectKind) => catalogList(s, c, k),
}));
vi.mock('../../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections, queries: [] })),
}));
vi.mock('../../composables/useConnect', () => ({
  withConnectPrompts: <T>(_profile: unknown, run: (secrets: object) => Promise<T>) => run({}),
}));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
async function settle(): Promise<void> {
  for (let i = 0; i < 6; i += 1) await tick();
}

function loaded(state: ChildState | undefined): ObjNode[] {
  if (state?.status !== 'loaded') throw new Error(`expected loaded children, got ${state?.status ?? 'nothing'}`);
  return state.nodes;
}

function byName(nodes: ObjNode[], name: string): ObjNode {
  const node = nodes.find((n) => n.name === name);
  if (!node) throw new Error(`no node ${name}`);
  return node;
}

describe('object browser', () => {
  beforeEach(async () => {
    connections = [profile('pg', 'postgres'), profile('my', 'mysql')];
    await useWorkspace().refresh();
    const browser = useObjectBrowser();
    browser.disconnect('pg');
    browser.disconnect('my');
    openSession.mockClear();
    openDatabaseSession.mockClear();
    closeSession.mockClear();
    catalogKinds.mockClear();
    databasesError = null;
    releaseList = null;
  });

  it('starts idle and loads the root after browse, with one session', async () => {
    const browser = useObjectBrowser();
    expect(browser.source.root('pg')).toEqual({ status: 'idle' });
    const done = browser.browse('pg');
    expect(browser.source.root('pg').status).toBe('loading');
    await done;
    const nodes = loaded(browser.source.root('pg'));
    expect(nodes.map((n) => n.name)).toEqual(['shop', 'analytics', 'Server']);
    expect(browser.capabilities('pg')).toEqual(caps);
    // Browsing again while loaded does nothing.
    await browser.browse('pg');
    expect(openSession).toHaveBeenCalledTimes(1);
  });

  it('browses another PostgreSQL database through one database session, shared by concurrent loads', async () => {
    const browser = useObjectBrowser();
    await browser.browse('pg');
    const analytics = byName(loaded(browser.source.root('pg')), 'analytics');
    const shop = byName(loaded(browser.source.root('pg')), 'shop');
    await Promise.all([browser.loadChildren(analytics), browser.loadChildren(shop)]);
    expect(openDatabaseSession).toHaveBeenCalledTimes(1);
    expect(openDatabaseSession).toHaveBeenCalledWith('pg', 'analytics');
    const schema = byName(loaded(browser.source.children(analytics.key)), 'public');
    await browser.loadChildren(schema);
    const tables = byName(loaded(browser.source.children(schema.key)), 'Tables');
    await browser.loadChildren(tables);
    expect(loaded(browser.source.children(tables.key)).map((n) => n.name)).toEqual(['orders']);
    expect(openDatabaseSession).toHaveBeenCalledTimes(1);
    // The schema's catalog calls name the database session.
    const databaseSession = (await openDatabaseSession.mock.results[0]?.value) as SessionInfo;
    const kindsCall = catalogKinds.mock.calls.find((c) => c[1].database === 'analytics' && c[1].schema === 'public');
    expect(kindsCall?.[0]).toBe(databaseSession.id);
  });

  it('uses one session for every MySQL database', async () => {
    const browser = useObjectBrowser();
    await browser.browse('my');
    const nodes = loaded(browser.source.root('my'));
    await browser.loadChildren(byName(nodes, 'analytics'));
    expect(openDatabaseSession).not.toHaveBeenCalled();
    expect(openSession).toHaveBeenCalledTimes(1);
  });

  it('drops answers that arrive after a disconnect and closes the sessions', async () => {
    const browser = useObjectBrowser();
    await browser.browse('pg');
    const shop = byName(loaded(browser.source.root('pg')), 'shop');
    await browser.loadChildren(shop);
    const schema = byName(loaded(browser.source.children(shop.key)), 'public');
    await browser.loadChildren(schema);
    const tables = byName(loaded(browser.source.children(schema.key)), 'Tables');
    releaseList = () => undefined;
    const pending = browser.loadChildren(tables);
    await tick();
    expect(browser.source.children(tables.key)?.status).toBe('loading');
    browser.disconnect('pg');
    releaseList();
    await pending;
    expect(browser.source.children(tables.key)).toBeUndefined();
    expect(browser.source.root('pg')).toEqual({ status: 'idle' });
    expect(closeSession).toHaveBeenCalled();
  });

  it('shows reconnectRequired as an error that reconnects on retry', async () => {
    const browser = useObjectBrowser();
    databasesError = { kind: 'reconnectRequired', message: 'The connection was edited.' };
    await browser.browse('pg');
    const root = browser.source.root('pg');
    expect(root).toMatchObject({ status: 'error', title: 'Reconnect needed', reconnect: true });
    databasesError = null;
    await browser.retry('pg', null);
    await settle();
    expect(loaded(browser.source.root('pg')).length).toBeGreaterThan(0);
    // The old session was closed and a new one opened.
    expect(openSession).toHaveBeenCalledTimes(2);
    expect(closeSession).toHaveBeenCalledTimes(1);
  });

  it('refresh forgets everything below a node and reloads it', async () => {
    const browser = useObjectBrowser();
    await browser.browse('pg');
    const shop = byName(loaded(browser.source.root('pg')), 'shop');
    await browser.loadChildren(shop);
    const schema = byName(loaded(browser.source.children(shop.key)), 'public');
    await browser.loadChildren(schema);
    await browser.refresh('pg', shop);
    expect(browser.source.children(schema.key)).toBeUndefined();
    expect(loaded(browser.source.children(shop.key)).map((n) => n.name)).toEqual(['public']);
  });

  it('refreshes only the container an editor changed', async () => {
    const browser = useObjectBrowser();
    await browser.browse('pg');
    const shop = byName(loaded(browser.source.root('pg')), 'shop');
    await browser.loadChildren(shop);
    const schema = byName(loaded(browser.source.children(shop.key)), 'public');
    await browser.loadChildren(schema);
    const tables = byName(loaded(browser.source.children(schema.key)), 'Tables');
    await browser.loadChildren(tables);
    const analytics = byName(loaded(browser.source.root('pg')), 'analytics');
    await browser.loadChildren(analytics);
    const before = catalogKinds.mock.calls.length;

    await browser.refreshObject('pg', { kind: 'table', database: 'shop', schema: 'public', parent: null, name: 'orders', signature: null });
    // The schema reloads its counts; the group below it is forgotten and reloads when shown.
    expect(catalogKinds.mock.calls.length).toBe(before + 1);
    expect(catalogKinds.mock.calls.at(-1)?.[1]).toEqual({ database: 'shop', schema: 'public', parent: null });
    expect(browser.source.children(tables.key)).toBeUndefined();
    expect(loaded(browser.source.children(analytics.key)).length).toBeGreaterThan(0);

    // A container that is not loaded changes nothing.
    await browser.refreshContainer('pg', { database: 'shop', schema: 'sales', parent: null });
    expect(catalogKinds.mock.calls.length).toBe(before + 1);
    // Server-wide: the whole tree.
    await browser.refreshContainer('pg', { database: null, schema: null, parent: null });
    expect(browser.source.children(shop.key)).toBeUndefined();
  });

  it('passes the system object choice to the catalog', async () => {
    const browser = useObjectBrowser();
    await browser.browse('pg');
    expect(browser.includeSystem('pg')).toBe(false);
    await browser.setIncludeSystem('pg', true);
    expect(browser.includeSystem('pg')).toBe(true);
    expect(catalogKinds.mock.calls.at(-1)?.[2]).toBe(true);
  });

  it('closes the browser of a deleted connection', async () => {
    const browser = useObjectBrowser();
    await browser.browse('my');
    expect(browser.browsing('my')).toBe(true);
    connections = [profile('pg', 'postgres')];
    await useWorkspace().refresh();
    await settle();
    expect(browser.browsing('my')).toBe(false);
    expect(browser.source.root('my')).toEqual({ status: 'idle' });
    expect(browser.source.children(`${objectRootKey('my')}/x`)).toBeUndefined();
  });
});
