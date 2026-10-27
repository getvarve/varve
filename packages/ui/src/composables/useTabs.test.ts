import type * as RecordsModule from '../api/records';
import type * as SessionsModule from '../api/sessions';
import type { ConnectionProfile } from '../api/records';
import type { ExecOutput, SessionInfo } from '../api/sessions';
import { useDialogs } from './useDialogs';
import { useQuerySave } from './useQuerySave';
import { knownDefaultDatabase, rememberDefaultDatabase, scanBinding, TAB_KINDS, tabIdentity, useTabs } from './useTabs';
import type { KeysTab, ObjectTab, QueryTab, TabKind } from './useTabs';
import { useWorkspace } from './useWorkspace';
import { fetchTree } from '../api/records';

const redisProfile = {
  id: 'r1',
  name: 'Cache',
  engine: 'redis',
  environment: 'development',
  folderId: null,
  database: null,
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { redisDb: 0 },
  savedSecrets: [],
} as unknown as ConnectionProfile;

const pgProfile = {
  ...redisProfile,
  id: 'p1',
  name: 'Orders',
  engine: 'postgres',
  database: 'shop',
} as unknown as ConnectionProfile;

let sessionCount = 0;
const openSession = vi.fn(async (): Promise<SessionInfo> => {
  sessionCount += 1;
  return { id: `s${sessionCount}`, connectionId: 'r1', engine: 'redis', server: { engine: 'redis', version: '7.4.1', major: 7, minor: 4, patch: 1 }, environment: 'development', database: null, readOnly: false, confirmDestructive: true, purpose: 'user', openedAt: 0, encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false } };
});
let releaseExecute: (output: ExecOutput) => void = () => undefined;
const execute = vi.fn(
  () =>
    new Promise<ExecOutput>((resolve) => {
      releaseExecute = resolve;
    }),
);
const redisSelectDb = vi.fn(async () => undefined);
const openDatabaseSession = vi.fn(async (_id: string, database: string): Promise<SessionInfo> => {
  sessionCount += 1;
  return { id: `s${sessionCount}`, connectionId: 'p1', engine: 'postgres', server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 }, environment: 'development', database, readOnly: false, confirmDestructive: false, purpose: 'user', openedAt: 0, encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false } };
});
const updateQuery = vi.fn();

vi.mock('../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: () => openSession(),
  openDatabaseSession: (id: string, database: string) => openDatabaseSession(id, database),
  execute: () => execute(),
  closeSession: vi.fn(async () => undefined),
  cancel: vi.fn(async () => undefined),
}));
vi.mock('../api/redis', async (original) => ({
  ...(await original<object>()),
  redisSelectDb: (...args: unknown[]) => redisSelectDb(...(args as [])),
}));
vi.mock('../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [redisProfile, pgProfile], queries: [] })),
  updateQuery: (...args: unknown[]) => updateQuery(...args),
}));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
async function settle(): Promise<void> {
  for (let i = 0; i < 5; i += 1) await tick();
}

describe('tab sessions', () => {
  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  it('restores the chosen database on reconnect and clears key state from the old session', async () => {
    const tabs = useTabs();
    const tab = tabs.openKeysTab(redisProfile) as KeysTab;
    await settle();
    await tabs.selectDb(tab.id, 3);
    const keys = tabs.tabs.find((t) => t.id === tab.id) as KeysTab;
    keys.scanFor = scanBinding(keys);
    keys.selected = 'abc';
    keys.cursor = '42';
    keys.scanned = true;
    await tabs.reconnect(tab.id);
    await settle();
    const after = tabs.tabs.find((t) => t.id === tab.id) as KeysTab;
    expect(redisSelectDb).toHaveBeenLastCalledWith(after.session?.id, 3);
    expect(after.db).toBe(3);
    expect(after.title).toBe('Keys · Cache db3');
    expect(after.cursor).toBeNull();
    expect(after.selected).toBeNull();
    expect(after.scanned).toBe(false);
    expect(after.scanFor).toBeNull();
    await tabs.closeTab(tab.id);
  });

  it('drops a query result when the session was replaced while it ran', async () => {
    const tabs = useTabs();
    const tab = tabs.openQueryTab(redisProfile) as QueryTab;
    await settle();
    const running = tabs.runQuery(tab.id, 'GET a', 0);
    await tick();
    await tabs.reconnect(tab.id);
    await settle();
    releaseExecute({ results: [], notices: [], elapsedMs: 1 });
    await running;
    const after = tabs.tabs.find((t) => t.id === tab.id) as QueryTab;
    expect(after.run).toBeNull();
    await tabs.closeTab(tab.id);
  });
});

describe('saving query tabs', () => {
  it('runs one save at a time and asks before closing a tab with unsaved text', async () => {
    const tabs = useTabs();
    const save = useQuerySave();
    const tab = tabs.openQueryTab(redisProfile) as QueryTab;
    await settle();
    tabs.loadText(tab.id, 'GET a');
    const dialogs = useDialogs();

    const first = save.saveQueryTab(tab.id, 'saveAs');
    const second = save.saveQueryTab(tab.id, 'save');
    await tick();
    expect(dialogs.dialogs.filter((d) => d.spec.type === 'saveQuery')).toHaveLength(1);
    const saveDialog = dialogs.dialogs.find((d) => d.spec.type === 'saveQuery');
    if (saveDialog) dialogs.close(saveDialog.id, null);
    await expect(first).resolves.toBe(false);
    await expect(second).resolves.toBe(false);

    const closing = save.closeWithCheck(tab.id);
    await tick();
    const ask = dialogs.dialogs.find((d) => d.spec.type === 'unsavedClose');
    expect(ask).toBeDefined();
    if (ask) dialogs.close(ask.id, null);
    await closing;
    expect(tabs.tabs.some((t) => t.id === tab.id)).toBe(true);

    const discarding = save.closeWithCheck(tab.id);
    await tick();
    const again = dialogs.dialogs.find((d) => d.spec.type === 'unsavedClose');
    if (again) dialogs.close(again.id, 'discard');
    await discarding;
    expect(tabs.tabs.some((t) => t.id === tab.id)).toBe(false);
  });
});

describe('unsaved tabs before closing, deleting or quitting', () => {
  it('asks once per tab, stops on Cancel and goes on after Discard', async () => {
    const tabs = useTabs();
    const save = useQuerySave();
    const dialogs = useDialogs();
    const a = tabs.openQueryTab(redisProfile) as QueryTab;
    const b = tabs.openQueryTab(redisProfile) as QueryTab;
    await settle();
    tabs.loadText(a.id, 'GET a');
    tabs.loadText(b.id, 'GET b');

    const cancelled = save.confirmUnsavedTabs([a.id, b.id]);
    await tick();
    const first = dialogs.dialogs.find((d) => d.spec.type === 'unsavedClose');
    if (first) dialogs.close(first.id, null);
    await expect(cancelled).resolves.toBe(false);
    expect(dialogs.dialogs).toHaveLength(0);

    const discarded = save.confirmUnsavedTabs([a.id, b.id]);
    for (let i = 0; i < 2; i += 1) {
      await tick();
      const open = dialogs.dialogs.find((d) => d.spec.type === 'unsavedClose');
      if (open) dialogs.close(open.id, 'discard');
    }
    await expect(discarded).resolves.toBe(true);

    // Two close requests for the same tab show one question.
    const one = save.closeWithCheck(a.id);
    const two = save.closeWithCheck(a.id);
    await tick();
    expect(dialogs.dialogs.filter((d) => d.spec.type === 'unsavedClose')).toHaveLength(1);
    const ask = dialogs.dialogs.find((d) => d.spec.type === 'unsavedClose');
    if (ask) dialogs.close(ask.id, 'discard');
    await Promise.all([one, two]);
    expect(tabs.tabs.some((t) => t.id === a.id)).toBe(false);
    await tabs.closeTab(b.id);
  });
});

describe('tab registration', () => {
  const orders = { kind: 'table' as const, database: 'analytics', schema: 'public', parent: null, name: 'orders', signature: null };

  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  it('lists every tab kind once', () => {
    const all: Record<TabKind, true> = {
      overview: true,
      query: true,
      keys: true,
      object: true,
      ddl: true,
      tableData: true,
      tableDesigner: true,
      explain: true,
      live: true,
      redisKey: true,
      notify: true,
      pubsub: true,
    };
    expect([...TAB_KINDS].sort()).toEqual(Object.keys(all).sort());
  });

  it('opens an object tab once per object and view, on the object database', async () => {
    const tabs = useTabs();
    const first = tabs.openObjectTab(pgProfile, { object: orders });
    const again = tabs.openObjectTab(pgProfile, { object: orders, intent: 'edit' });
    expect(again.id).toBe(first.id);
    expect((again as ObjectTab).intent).toBe('edit');
    const data = tabs.openTableDataTab(pgProfile, orders);
    const ddl = tabs.openDdlTab(pgProfile, orders);
    expect(new Set([first.id, data.id, ddl.id]).size).toBe(3);
    expect(tabs.openTableDataTab(pgProfile, { ...orders, schema: 'archive' }).id).not.toBe(data.id);
    await settle();
    expect(openDatabaseSession).toHaveBeenCalledWith('p1', 'analytics');
    const after = tabs.tabs.find((t) => t.id === first.id);
    expect(after?.database).toBe('analytics');
    expect(after?.session?.database).toBe('analytics');
    expect(after?.title).toBe('orders');
    for (const tab of [...tabs.tabs]) await tabs.closeTab(tab.id);
  });

  it('opens new objects, explains and query tabs anew every time', () => {
    const tabs = useTabs();
    const container = { database: 'shop', schema: 'public', parent: null };
    const a = tabs.openObjectTab(pgProfile, { createKind: 'view', container });
    const b = tabs.openObjectTab(pgProfile, { createKind: 'view', container });
    expect(a.id).not.toBe(b.id);
    expect(a.title).toBe('New view');
    expect(tabIdentity(a)).toBeNull();
    const e1 = tabs.openExplainTab(pgProfile, 'SELECT 1');
    const e2 = tabs.openExplainTab(pgProfile, 'SELECT 1');
    expect(e1.id).not.toBe(e2.id);
    const designer = tabs.openTableDesigner(pgProfile, null, container);
    expect(designer.title).toBe('New table');
  });

  it('keeps one live panel, notification and Pub/Sub tab per connection', () => {
    const tabs = useTabs();
    expect(tabs.openLiveTab(pgProfile, 'processes').id).toBe(tabs.openLiveTab(pgProfile, 'processes').id);
    expect(tabs.openLiveTab(pgProfile, 'variables').id).not.toBe(tabs.openLiveTab(pgProfile, 'processes').id);
    expect(tabs.openNotifyTab(pgProfile).id).toBe(tabs.openNotifyTab(pgProfile).id);
    expect(tabs.openPubSubTab(redisProfile).id).toBe(tabs.openPubSubTab(redisProfile).id);
    const key0 = tabs.openRedisKeyTab(redisProfile, 'user:1', 0);
    expect(tabs.openRedisKeyTab(redisProfile, 'user:1', 0).id).toBe(key0.id);
    expect(tabs.openRedisKeyTab(redisProfile, 'user:1', 2).id).not.toBe(key0.id);
    expect(tabs.openRedisKeyTab(redisProfile, 'user:1', 2).db).toBe(2);
  });

  it('only PostgreSQL tabs open on another database', () => {
    const tabs = useTabs();
    const redisQuery = tabs.openQueryTab(redisProfile, null, '', 'analytics');
    expect(redisQuery.database).toBeNull();
    const pgQuery = tabs.openQueryTab(pgProfile, null, '', 'analytics');
    expect(pgQuery.database).toBe('analytics');
    expect(pgQuery.title).toBe('Orders · analytics');
  });

  it('marks a tab for reconnect on reconnectRequired and clears it on reconnect', async () => {
    const tabs = useTabs();
    const tab = tabs.openLiveTab(pgProfile, 'serverStatus');
    await settle();
    const cmd = tabs.reportTabError(tab.id, { kind: 'reconnectRequired', message: 'The connection was edited.' });
    expect(cmd.kind).toBe('reconnectRequired');
    expect(tabs.tabs.find((t) => t.id === tab.id)?.reconnectNeeded).toEqual(cmd);
    await tabs.reconnect(tab.id);
    await settle();
    const after = tabs.tabs.find((t) => t.id === tab.id);
    expect(after?.reconnectNeeded).toBeNull();
    expect(after?.status).toBe('ready');
    const lost = tabs.reportTabError(tab.id, { kind: 'session', error: { kind: 'notFound', sessionId: 'x' } });
    expect(lost.kind).toBe('session');
    expect(tabs.tabs.find((t) => t.id === tab.id)?.status).toBe('error');
  });

  it('switches an open key browser to the database asked for', async () => {
    const tabs = useTabs();
    const keys = tabs.openKeysTab(redisProfile, 4);
    await settle();
    expect(redisSelectDb).toHaveBeenLastCalledWith(tabs.tabs.find((t) => t.id === keys.id)?.session?.id, 4);
    tabs.openKeysTab(redisProfile, 5);
    await settle();
    expect(tabs.tabs.find((t) => t.id === keys.id)?.db).toBe(5);
    await tabs.closeTab(keys.id);
  });

  it('points a new-object tab at the object it created', () => {
    const tabs = useTabs();
    const container = { database: 'shop', schema: 'public', parent: null };
    const designer = tabs.openTableDesigner(pgProfile, null, container);
    const created = { kind: 'table' as const, database: 'shop', schema: 'public', parent: null, name: 'invoices', signature: null };
    tabs.setTabObject(designer.id, created);
    const after = tabs.tabs.find((t) => t.id === designer.id);
    expect(after?.title).toBe('invoices · Design');
    expect(after && tabIdentity(after)).not.toBeNull();
    // Opening the designer of that table now finds this tab.
    expect(tabs.openTableDesigner(pgProfile, created, container).id).toBe(designer.id);
    const view = tabs.openObjectTab(pgProfile, { createKind: 'view', container });
    tabs.setTabObject(view.id, { ...created, kind: 'view', name: 'v' });
    const viewAfter = tabs.tabs.find((t) => t.id === view.id) as ObjectTab;
    expect(viewAfter.intent).toBe('view');
    expect(viewAfter.title).toBe('v');
  });

  it('keeps one overview per PostgreSQL database and switches its schema', () => {
    const tabs = useTabs();
    const first = tabs.openOverviewTab(pgProfile);
    expect(tabs.openOverviewTab(pgProfile).id).toBe(first.id);
    const analytics = tabs.openOverviewTab(pgProfile, { database: 'analytics', schema: 'sales' });
    expect(analytics.id).not.toBe(first.id);
    expect(analytics.database).toBe('analytics');
    expect(analytics.kind === 'overview' && analytics.scope).toEqual({ database: null, schema: 'sales' });
    const again = tabs.openOverviewTab(pgProfile, { database: 'analytics', schema: 'audit' });
    expect(again.id).toBe(analytics.id);
    expect(again.kind === 'overview' && again.scope.schema).toBe('audit');
    expect(analytics.title).toBe('analytics · Orders');
  });

  it('opens one overview for the default PostgreSQL database, from the connection or its database node', async () => {
    const tabs = useTabs();
    // pgProfile names database "shop".
    const fromConnection = tabs.openOverviewTab(pgProfile);
    expect(tabs.openOverviewTab(pgProfile, { database: 'shop' }).id).toBe(fromConnection.id);
    await tabs.closeTab(fromConnection.id);
    const fromTree = tabs.openOverviewTab(pgProfile, { database: 'shop' });
    expect(tabs.openOverviewTab(pgProfile).id).toBe(fromTree.id);
    expect(tabs.openOverviewTab(pgProfile, { database: 'analytics' }).id).not.toBe(fromTree.id);
    for (const tab of tabs.tabs.filter((t) => t.kind === 'overview')) await tabs.closeTab(tab.id);
  });

  it('learns the default database from a connected session when the profile names none', async () => {
    const tabs = useTabs();
    const noDb = { ...pgProfile, database: null } as unknown as ConnectionProfile;
    const fromConnection = tabs.openOverviewTab(noDb);
    await settle();
    const connected = tabs.tabs.find((t) => t.id === fromConnection.id);
    if (connected?.session) connected.session = { ...connected.session, database: 'postgres' };
    expect(tabs.openOverviewTab(noDb, { database: 'postgres' }).id).toBe(fromConnection.id);
    for (const tab of tabs.tabs.filter((t) => t.kind === 'overview')) await tabs.closeTab(tab.id);
  });

  it('keeps one overview when the database node was opened before the connection (profile names no database)', async () => {
    const tabs = useTabs();
    const noDb = { ...pgProfile, database: null } as unknown as ConnectionProfile;
    const fromTree = tabs.openOverviewTab(noDb, { database: 'postgres' });
    await settle();
    openSession.mockImplementationOnce(async () => {
      sessionCount += 1;
      return { id: `s${sessionCount}`, connectionId: 'p1', engine: 'postgres', server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 }, environment: 'development', database: 'postgres', readOnly: false, confirmDestructive: false, purpose: 'user', openedAt: 0, encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false } };
    });
    tabs.openOverviewTab(noDb);
    await settle();
    const overviews = tabs.tabs.filter((t) => t.kind === 'overview' && t.connectionId === 'p1');
    expect(overviews.map((t) => t.id)).toEqual([fromTree.id]);
    expect(tabs.activeId.value).toBe(fromTree.id);
    for (const tab of overviews) await tabs.closeTab(tab.id);
  });

  it('reuses the connection overview while it still connects, once the default database is known', async () => {
    const tabs = useTabs();
    const noDb = { ...pgProfile, database: null, version: 7 } as unknown as ConnectionProfile;
    // The object tree learnt the default database when it connected.
    rememberDefaultDatabase(noDb, 'postgres');
    const fromConnection = tabs.openOverviewTab(noDb);
    expect(fromConnection.status).toBe('connecting');
    expect(tabs.openOverviewTab(noDb, { database: 'postgres' }).id).toBe(fromConnection.id);
    await settle();
    for (const tab of tabs.tabs.filter((t) => t.kind === 'overview')) await tabs.closeTab(tab.id);
  });

  it('keeps the working overview when the other one for the same database failed', async () => {
    const tabs = useTabs();
    // A version no session has reported a default database for, so the match happens after connecting.
    const noDb = { ...pgProfile, database: null, version: 9 } as unknown as ConnectionProfile;
    openDatabaseSession.mockRejectedValueOnce({ kind: 'session', error: { kind: 'driver', error: { kind: 'connect', message: 'refused' } } });
    const fromTree = tabs.openOverviewTab(noDb, { database: 'postgres' });
    await settle();
    expect(tabs.tabs.find((t) => t.id === fromTree.id)?.status).toBe('error');
    openSession.mockImplementationOnce(async () => {
      sessionCount += 1;
      return { id: `s${sessionCount}`, connectionId: 'p1', engine: 'postgres', server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 }, environment: 'development', database: 'postgres', readOnly: false, confirmDestructive: false, purpose: 'user', openedAt: 0, encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false } };
    });
    const fromConnection = tabs.openOverviewTab(noDb);
    await settle();
    const overviews = tabs.tabs.filter((t) => t.kind === 'overview' && t.connectionId === 'p1');
    expect(overviews.map((t) => t.id)).toEqual([fromConnection.id]);
    expect(overviews[0]?.status).toBe('ready');
    for (const tab of overviews) await tabs.closeTab(tab.id);
  });

  it('keeps the ready overview over one for the same database that is still connecting', async () => {
    const tabs = useTabs();
    const noDb = { ...pgProfile, database: null, version: 11 } as unknown as ConnectionProfile;
    // The tree's tab on database postgres hangs while connecting.
    let failTree: (error: unknown) => void = () => undefined;
    openDatabaseSession.mockImplementationOnce(
      () =>
        new Promise<SessionInfo>((_resolve, reject) => {
          failTree = reject;
        }),
    );
    const fromTree = tabs.openOverviewTab(noDb, { database: 'postgres' });
    await settle();
    expect(tabs.tabs.find((t) => t.id === fromTree.id)?.status).toBe('connecting');
    openSession.mockImplementationOnce(async () => {
      sessionCount += 1;
      return { id: `s${sessionCount}`, connectionId: 'p1', engine: 'postgres', server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 }, environment: 'development', database: 'postgres', readOnly: false, confirmDestructive: false, purpose: 'user', openedAt: 0, encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false } };
    });
    const fromConnection = tabs.openOverviewTab(noDb);
    await settle();
    // The ready one stays; the one still connecting goes.
    let overviews = tabs.tabs.filter((t) => t.kind === 'overview' && t.connectionId === 'p1');
    expect(overviews.map((t) => t.id)).toEqual([fromConnection.id]);
    expect(overviews[0]?.status).toBe('ready');
    // Its connect failing later changes nothing.
    failTree({ kind: 'session', error: { kind: 'driver', error: { kind: 'connect', message: 'refused' } } });
    await settle();
    overviews = tabs.tabs.filter((t) => t.kind === 'overview' && t.connectionId === 'p1');
    expect(overviews.map((t) => t.status)).toEqual(['ready']);
    for (const tab of overviews) await tabs.closeTab(tab.id);
  });

  it('forgets a remembered default database when the connection is edited or deleted', async () => {
    const v3 = { ...pgProfile, database: null, version: 3 } as unknown as ConnectionProfile;
    vi.mocked(fetchTree).mockResolvedValueOnce({ folders: [], connections: [redisProfile, v3], queries: [] });
    await useWorkspace().refresh();
    rememberDefaultDatabase(v3, 'postgres');
    expect(knownDefaultDatabase(v3)).toBe('postgres');
    // Edited: a new version of the connection.
    const v4 = { ...v3, version: 4 } as unknown as ConnectionProfile;
    vi.mocked(fetchTree).mockResolvedValueOnce({ folders: [], connections: [redisProfile, v4], queries: [] });
    await useWorkspace().refresh();
    await settle();
    expect(knownDefaultDatabase(v3)).toBeNull();
    rememberDefaultDatabase(v4, 'postgres');
    // Deleted.
    vi.mocked(fetchTree).mockResolvedValueOnce({ folders: [], connections: [redisProfile], queries: [] });
    await useWorkspace().refresh();
    await settle();
    expect(knownDefaultDatabase(v4)).toBeNull();
    await useWorkspace().refresh();
  });

  it('tracks unsaved editor changes', () => {
    const tabs = useTabs();
    const tab = tabs.openTableDesigner(pgProfile, null, { database: 'shop', schema: 'public', parent: null });
    tabs.setUnsaved(tab.id, true);
    expect(tabs.tabs.find((t) => t.id === tab.id)?.unsaved).toBe(true);
  });
});
