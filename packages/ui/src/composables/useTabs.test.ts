import type * as RecordsModule from '../api/records';
import type * as SessionsModule from '../api/sessions';
import type { ConnectionProfile } from '../api/records';
import type { ExecOutput, SessionInfo } from '../api/sessions';
import { effectScope, nextTick, watch } from 'vue';
import { useDialogs } from './useDialogs';
import { useQuerySave } from './useQuerySave';
import { knownDefaultDatabase, rememberDefaultDatabase, scanBinding, showsView, TAB_KINDS, tabIdentity, useLastSession, useTabs } from './useTabs';
import type { KeysTab, ObjectTab, QueryTab, TabKind, WorkspaceTab } from './useTabs';
import { useWorkspace } from './useWorkspace';
import { fetchTree } from '../api/records';
import { NO_MULTI } from '../lib/redisMode';
import { closeSession } from '../api/sessions';

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

const myProfile = {
  ...redisProfile,
  id: 'm1',
  name: 'Store',
  engine: 'mysql',
  database: 'app',
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
  fetchTree: vi.fn(async () => ({ folders: [], connections: [redisProfile, pgProfile, myProfile], queries: [] })),
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

  /** Runs `text` in the console of a query tab and lets it succeed. */
  async function runOk(id: string, text: string): Promise<void> {
    const running = useTabs().runQuery(id, text, 0);
    await tick();
    releaseExecute({ results: [], notices: [], elapsedMs: 1 });
    await running;
  }

  it('keeps the database when a SELECT queued in MULTI is discarded in a later run', async () => {
    const tabs = useTabs();
    const tab = tabs.openQueryTab(redisProfile) as QueryTab;
    await settle();
    const live = tabs.tabs.find((t) => t.id === tab.id) as QueryTab;
    await runOk(tab.id, 'MULTI');
    expect(live.multi).toEqual({ open: true, queuedDb: null });
    await runOk(tab.id, 'SELECT 3');
    // Only queued: the session is still on the database it was on.
    expect(live.db).toBe(0);
    expect(live.multi).toEqual({ open: true, queuedDb: 3 });
    await runOk(tab.id, 'DISCARD');
    expect(live.db).toBe(0);
    expect(live.multi).toEqual(NO_MULTI);
    // Outside a block a SELECT counts at once.
    await runOk(tab.id, 'SELECT 2');
    expect(live.db).toBe(2);
    await tabs.closeTab(tab.id);
  });

  it('applies a SELECT queued in MULTI when a later run sends EXEC', async () => {
    const tabs = useTabs();
    const tab = tabs.openQueryTab(redisProfile) as QueryTab;
    await settle();
    const live = tabs.tabs.find((t) => t.id === tab.id) as QueryTab;
    await runOk(tab.id, 'MULTI');
    await runOk(tab.id, 'SELECT 3');
    expect(live.db).toBe(0);
    await runOk(tab.id, 'EXEC');
    expect(live.db).toBe(3);
    expect(live.multi).toEqual(NO_MULTI);
    await tabs.closeTab(tab.id);
  });

  it('forgets an open MULTI block when the tab gets a new session or loses its session', async () => {
    const tabs = useTabs();
    const tab = tabs.openQueryTab(redisProfile) as QueryTab;
    await settle();
    const live = tabs.tabs.find((t) => t.id === tab.id) as QueryTab;
    await runOk(tab.id, 'MULTI\nSELECT 3');
    expect(live.multi).toEqual({ open: true, queuedDb: 3 });
    await tabs.reconnect(tab.id);
    await settle();
    expect(live.multi).toEqual(NO_MULTI);
    // The block belonged to the old session: EXEC on the new one switches nothing.
    await runOk(tab.id, 'EXEC');
    expect(live.db).toBe(0);
    await runOk(tab.id, 'MULTI\nSELECT 5');
    expect(live.multi).toEqual({ open: true, queuedDb: 5 });
    tabs.markLost(tab.id, { kind: 'session', error: { kind: 'notFound', sessionId: live.session?.id ?? '' } });
    expect(live.session).toBeNull();
    expect(live.multi).toEqual(NO_MULTI);
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
    // Other MySQL tabs name their database in every statement; their session stays on the profile's.
    expect(tabs.openTableDataTab(myProfile, { kind: 'table', database: 'shop', schema: null, parent: null, name: 'orders', signature: null }).database).toBeNull();
  });

  it('opens a MySQL query tab on the database it was opened for, also after a reconnect', async () => {
    const tabs = useTabs();
    openDatabaseSession.mockClear();
    const tab = tabs.openQueryTab(myProfile, null, '', 'shop');
    expect(tab.database).toBe('shop');
    expect(tab.title).toBe('Store · shop');
    await settle();
    expect(openDatabaseSession).toHaveBeenLastCalledWith('m1', 'shop');
    await tabs.reconnect(tab.id);
    await settle();
    expect(openDatabaseSession).toHaveBeenCalledTimes(2);
    expect(openDatabaseSession).toHaveBeenLastCalledWith('m1', 'shop');
    await tabs.closeTab(tab.id);
  });

  it('keeps the old session until a reconnect has a new one, then closes the old one', async () => {
    const tabs = useTabs();
    const tab = tabs.openLiveTab(pgProfile, 'galera');
    await settle();
    const live = tabs.tabs.find((t) => t.id === tab.id) as WorkspaceTab;
    const old = live.session?.id;
    expect(old).toBeDefined();
    const seen: (string | null)[] = [];
    const scope = effectScope();
    scope.run(() => watch(() => live.session?.id ?? null, (id) => seen.push(id), { flush: 'sync' }));
    vi.mocked(closeSession).mockClear();
    const reconnecting = tabs.reconnect(tab.id);
    expect(live.status).toBe('connecting');
    expect(live.session?.id).toBe(old);
    expect(closeSession).not.toHaveBeenCalled();
    await reconnecting;
    await settle();
    // One step from the old session to the new one: a view rendering `v-if="tab.session"` stays mounted.
    expect(seen).toHaveLength(1);
    expect(seen[0]).not.toBeNull();
    expect(seen[0]).not.toBe(old);
    expect(live.status).toBe('ready');
    expect(closeSession).toHaveBeenCalledWith(old);
    scope.stop();
    await tabs.closeTab(tab.id);
  });

  it('closes the old session when a reconnect fails, and refuses runs while it connects', async () => {
    const tabs = useTabs();
    const tab = tabs.openQueryTab(redisProfile) as QueryTab;
    await settle();
    const live = tabs.tabs.find((t) => t.id === tab.id) as QueryTab;
    const old = live.session?.id;
    let fail: (error: unknown) => void = () => undefined;
    openSession.mockImplementationOnce(
      () =>
        new Promise<SessionInfo>((_resolve, reject) => {
          fail = reject;
        }),
    );
    vi.mocked(closeSession).mockClear();
    execute.mockClear();
    const reconnecting = tabs.reconnect(tab.id);
    await tick();
    await tabs.runQuery(tab.id, 'GET a', 0);
    expect(execute).not.toHaveBeenCalled();
    fail({ kind: 'internal', message: 'The server went away.' });
    await reconnecting;
    await settle();
    expect(live.status).toBe('error');
    expect(live.session).toBeNull();
    expect(closeSession).toHaveBeenCalledWith(old);
    await tabs.closeTab(tab.id);
  });

  it('says which tab is on screen', async () => {
    const tabs = useTabs();
    const a = tabs.openLiveTab(pgProfile, 'redisConfig');
    const b = tabs.openQueryTab(redisProfile);
    await settle();
    tabs.setWorkspaceShown(true);
    expect(tabs.activeId.value).toBe(b.id);
    expect(tabs.isTabVisible(b.id)).toBe(true);
    expect(tabs.isTabVisible(a.id)).toBe(false);
    tabs.activate(a.id);
    expect(tabs.isTabVisible(a.id)).toBe(true);
    // A reconnect hides a view behind the connection state; query tabs show their own.
    const live = tabs.tabs.find((t) => t.id === a.id) as WorkspaceTab;
    live.status = 'connecting';
    expect(showsView(live)).toBe(false);
    expect(tabs.isTabVisible(a.id)).toBe(false);
    live.status = 'ready';
    expect(showsView({ ...live, kind: 'query', status: 'connecting' } as WorkspaceTab)).toBe(true);
    // Activity or Settings shown: no tab is on screen.
    tabs.setWorkspaceShown(false);
    expect(tabs.isTabVisible(a.id)).toBe(false);
    await tabs.closeTab(a.id);
    await tabs.closeTab(b.id);
  });

  it('keeps the last session for a view while the tab has none', async () => {
    const tabs = useTabs();
    const tab = tabs.openLiveTab(pgProfile, 'redisModules');
    await settle();
    const live = tabs.tabs.find((t) => t.id === tab.id) as WorkspaceTab;
    const first = live.session;
    const scope = effectScope();
    const last = scope.run(() => useLastSession(() => live));
    expect(last?.value).toBe(first);
    tabs.markLost(tab.id, { kind: 'session', error: { kind: 'notFound', sessionId: first?.id ?? '' } });
    await nextTick();
    expect(live.session).toBeNull();
    expect(last?.value).toBe(first);
    await tabs.reconnect(tab.id);
    await settle();
    expect(last?.value).toBe(live.session);
    expect(last?.value).not.toBe(first);
    scope.stop();
    await tabs.closeTab(tab.id);
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
    const lost = tabs.reportTabError(tab.id, { kind: 'session', error: { kind: 'notFound', sessionId: after?.session?.id ?? '' } });
    expect(lost.kind).toBe('session');
    expect(tabs.tabs.find((t) => t.id === tab.id)?.status).toBe('error');
    await tabs.closeTab(tab.id);
  });

  it('ignores a late error of the session a reconnect replaced', async () => {
    const tabs = useTabs();
    const tab = tabs.openLiveTab(pgProfile, 'serverStatus');
    await settle();
    const live = tabs.tabs.find((t) => t.id === tab.id) as WorkspaceTab;
    const oldId = live.session?.id ?? '';
    await tabs.reconnect(tab.id);
    await settle();
    const newId = live.session?.id ?? '';
    expect(newId).not.toBe(oldId);
    vi.mocked(closeSession).mockClear();
    const lostConnection = { kind: 'session', error: { kind: 'driver', error: { kind: 'connection_lost', message: 'gone', code: null, detail: null, hint: null, position: null } } };

    // The call was sent on the old session: its failure is parsed for display and the tab keeps the new session.
    expect(tabs.reportTabError(tab.id, lostConnection, oldId).kind).toBe('session');
    tabs.reportTabError(tab.id, { kind: 'reconnectRequired', message: 'The connection was edited.' }, oldId);
    // A "session not found" names its session, so it is recognised without the caller passing the id.
    tabs.reportTabError(tab.id, { kind: 'session', error: { kind: 'notFound', sessionId: oldId } });
    tabs.markLost(tab.id, { kind: 'session', error: { kind: 'notFound', sessionId: oldId } });
    expect(live.status).toBe('ready');
    expect(live.session?.id).toBe(newId);
    expect(live.reconnectNeeded).toBeNull();
    expect(live.connectError).toBeNull();
    expect(vi.mocked(closeSession)).not.toHaveBeenCalled();

    // The same errors on the tab's own session do count.
    tabs.reportTabError(tab.id, { kind: 'reconnectRequired', message: 'The connection was edited.' }, newId);
    expect(live.reconnectNeeded).not.toBeNull();
    tabs.reportTabError(tab.id, lostConnection, newId);
    expect(live.status).toBe('error');
    expect(live.session).toBeNull();
    expect(vi.mocked(closeSession)).toHaveBeenCalledWith(newId);

    // Already lost: a second late answer of that session changes nothing while the tab connects again.
    const again = tabs.reconnect(tab.id);
    expect(live.status).toBe('connecting');
    tabs.reportTabError(tab.id, lostConnection, newId);
    expect(live.status).toBe('connecting');
    await again;
    await settle();
    expect(live.status).toBe('ready');
    await tabs.closeTab(tab.id);
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

describe('closing a connection', () => {
  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  it('ends every tab session of the connection and keeps its tabs, not connected', async () => {
    const tabs = useTabs();
    await tabs.closeConnectionTabs('r1');
    const keys = tabs.openKeysTab(redisProfile);
    const query = tabs.openQueryTab(redisProfile) as QueryTab;
    const other = tabs.openQueryTab(pgProfile);
    await settle();
    tabs.loadText(query.id, 'GET a');
    const sessions = [keys.id, query.id].map((id) => tabs.tabs.find((t) => t.id === id)?.session?.id);
    vi.mocked(closeSession).mockClear();

    await tabs.disconnectConnection('r1');
    for (const id of sessions) expect(closeSession).toHaveBeenCalledWith(id);
    for (const id of [keys.id, query.id]) {
      expect(tabs.tabs.find((t) => t.id === id)).toMatchObject({ status: 'cancelled', session: null, connectError: null, reconnectNeeded: null });
    }
    expect((tabs.tabs.find((t) => t.id === query.id) as QueryTab).sql).toBe('GET a');
    // Tabs of other connections keep their sessions.
    expect(tabs.tabs.find((t) => t.id === other.id)?.status).toBe('ready');

    // Opening a tab again connects it again; Connect again does too.
    tabs.openKeysTab(redisProfile);
    await tabs.reconnect(query.id);
    await settle();
    expect(tabs.tabs.find((t) => t.id === keys.id)?.status).toBe('ready');
    expect(tabs.tabs.find((t) => t.id === query.id)?.status).toBe('ready');
    await tabs.closeConnectionTabs('r1');
    await tabs.closeTab(other.id);
  });

  it('drops a connect that is still running and closes its session once it opens', async () => {
    const tabs = useTabs();
    let open: (info: SessionInfo) => void = () => undefined;
    openSession.mockImplementationOnce(() => new Promise<SessionInfo>((resolve) => (open = resolve)));
    const tab = tabs.openQueryTab(redisProfile);
    await tick();
    expect(tabs.tabs.find((t) => t.id === tab.id)?.status).toBe('connecting');
    await tabs.disconnectConnection('r1');
    expect(tabs.tabs.find((t) => t.id === tab.id)?.status).toBe('cancelled');
    vi.mocked(closeSession).mockClear();
    open({ ...(await openSession()), id: 'late' });
    await settle();
    expect(closeSession).toHaveBeenCalledWith('late');
    expect(tabs.tabs.find((t) => t.id === tab.id)).toMatchObject({ status: 'cancelled', session: null });
    await tabs.closeTab(tab.id);
  });
});
