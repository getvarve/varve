import type * as RecordsModule from '../../api/records';
import type * as SessionsModule from '../../api/sessions';
import type { ConnectionProfile } from '../../api/records';
import type { SessionInfo } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import { useTabs } from '../../composables/useTabs';
import type { QueryTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { closeConnection, closeQuestion } from './closeConnection';
import { useConnectionStates } from './connectionState';
import { useObjectBrowser } from './objectBrowser';

const profile = {
  id: 'pg',
  name: 'Orders primary',
  engine: 'postgres',
  environment: 'production',
  folderId: null,
  database: 'shop',
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { redisDb: 0, showSystemObjects: false },
  savedSecrets: [],
  version: 1,
} as unknown as ConnectionProfile;

let sessions = 0;
const openSession = vi.fn(async (): Promise<SessionInfo> => {
  sessions += 1;
  return {
    id: `s${sessions}`,
    connectionId: 'pg',
    engine: 'postgres',
    server: { engine: 'postgres', version: '16.2', major: 16, minor: 2, patch: null },
    environment: 'production',
    database: 'shop',
    readOnly: false,
    confirmDestructive: true,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
});
const closeSession = vi.fn(async (_id: string) => undefined);
let transactions: string[] | Error = [];
let busy: string[] | Error = [];

vi.mock('../../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: () => openSession(),
  openDatabaseSession: () => openSession(),
  closeSession: (id: string) => closeSession(id),
  cancel: vi.fn(async () => undefined),
  busySessions: vi.fn(async (_connectionId: string) => {
    if (busy instanceof Error) throw busy;
    return busy;
  }),
  openTransactions: vi.fn(async () => {
    if (transactions instanceof Error) throw transactions;
    return transactions;
  }),
}));
vi.mock('../../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [profile], queries: [] })),
}));
vi.mock('../../api/catalog', async (original) => ({
  ...(await original<object>()),
  catalogCapabilities: vi.fn(async () => ({ engine: 'postgres', kinds: ['database'], editable: [], features: [] })),
  catalogDatabases: vi.fn(async () => []),
  catalogKinds: vi.fn(async () => []),
}));
vi.mock('../../composables/useConnect', () => ({
  withConnectPrompts: <T>(_profile: unknown, run: (secrets: object) => Promise<T>) => run({}),
}));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
async function settle(): Promise<void> {
  for (let i = 0; i < 6; i += 1) await tick();
}

describe('closeQuestion', () => {
  it('asks nothing when nothing would be interrupted', () => {
    expect(closeQuestion('Orders', { unsaved: [], running: [], working: 0, transactions: 0 })).toBeNull();
  });

  it('asks once about every tab, with what happens to each kind of work', () => {
    const question = closeQuestion('Orders', { unsaved: ['Query 1', 'orders · Design'], running: ['Report'], working: 0, transactions: 2 });
    expect(question).toEqual({
      title: 'Close connection “Orders”?',
      message:
        '2 transactions are open. They are rolled back. A statement is still running in “Report”. It is cancelled. “Query 1”, “orders · Design” have changes that are not saved or applied. The tabs stay open and keep them.',
      danger: true,
    });
  });

  it('is not a danger for unsaved tab work alone, which stays in its tab', () => {
    const question = closeQuestion('Orders', { unsaved: ['Query 1'], running: [], working: 0, transactions: 0 });
    expect(question?.danger).toBe(false);
    expect(question?.message).toBe('“Query 1” has changes that are not saved or applied. The tab stays open and keeps them.');
  });

  it('asks when open transactions could not be checked', () => {
    const question = closeQuestion('Orders', { unsaved: [], running: [], working: 0, transactions: null });
    expect(question?.message).toBe('Varve could not check for open transactions. Any open transaction is rolled back.');
    expect(question?.danger).toBe(true);
    expect(closeQuestion('Orders', { unsaved: [], running: [], working: 0, transactions: 1 })?.message).toBe('A transaction is open. It is rolled back.');
  });

  it('asks when a session is working on something that is not a query tab\'s statement', () => {
    const one = closeQuestion('Orders', { unsaved: [], running: [], working: 1, transactions: 0 });
    expect(one).toEqual({
      title: 'Close connection “Orders”?',
      message: 'A session is still working. Its work is cancelled.',
      danger: true,
    });
    expect(closeQuestion('Orders', { unsaved: [], running: [], working: 2, transactions: 0 })?.message).toBe(
      '2 sessions are still working. Their work is cancelled.',
    );
  });

  it('asks when working sessions could not be checked', () => {
    const question = closeQuestion('Orders', { unsaved: [], running: [], working: null, transactions: 0 });
    expect(question?.message).toBe('Varve could not check whether a session is still working. Work in progress is cancelled.');
    expect(question?.danger).toBe(true);
  });
});

describe('closeConnection', () => {
  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  beforeEach(() => {
    transactions = [];
    busy = [];
    closeSession.mockClear();
  });

  it('ends the sessions of the tabs and the object browser, keeps the tabs, and the row shows not connected', async () => {
    const tabs = useTabs();
    const objects = useObjectBrowser();
    const states = useConnectionStates();
    const overview = tabs.openOverviewTab(profile);
    const query = tabs.openQueryTab(profile);
    await objects.browse('pg');
    await settle();
    expect(states.state('pg')).toMatchObject({ connected: true, tabs: 2, sessions: 3 });

    await expect(closeConnection(profile)).resolves.toBe(true);
    expect(closeSession).toHaveBeenCalledTimes(3);
    expect(states.state('pg')).toMatchObject({ connected: false, tabs: 2, sessions: 0 });
    expect(objects.browsing('pg')).toBe(false);
    for (const id of [overview.id, query.id]) {
      const tab = tabs.tabs.find((t) => t.id === id);
      expect(tab).toMatchObject({ status: 'cancelled', session: null, connectError: null });
    }

    // Opening a tab again connects it again.
    tabs.openOverviewTab(profile);
    await settle();
    expect(tabs.tabs.find((t) => t.id === overview.id)).toMatchObject({ status: 'ready' });
    expect(states.state('pg').connected).toBe(true);
    await tabs.closeConnectionTabs('pg');
  });

  it('asks one question first and keeps everything when it is cancelled', async () => {
    const tabs = useTabs();
    const dialogs = useDialogs();
    const a = tabs.openQueryTab(profile) as QueryTab;
    const b = tabs.openQueryTab(profile) as QueryTab;
    await settle();
    tabs.loadText(a.id, 'select 1');
    tabs.loadText(b.id, 'select 2');
    transactions = ['pg', 'other'];

    const cancelled = closeConnection(profile);
    await settle();
    const asked = dialogs.dialogs.filter((d) => d.spec.type === 'confirm');
    expect(asked).toHaveLength(1);
    const spec = asked[0]?.spec;
    expect(spec?.type === 'confirm' && spec.message).toContain('A transaction is open. It is rolled back.');
    if (asked[0]) dialogs.close(asked[0].id, false);
    await expect(cancelled).resolves.toBe(false);
    expect(closeSession).not.toHaveBeenCalled();
    expect(tabs.tabs.filter((t) => t.connectionId === 'pg').every((t) => t.status === 'ready' && t.session !== null)).toBe(true);

    const closed = closeConnection(profile);
    await settle();
    const again = dialogs.dialogs.find((d) => d.spec.type === 'confirm');
    if (again) dialogs.close(again.id, true);
    await expect(closed).resolves.toBe(true);
    expect(closeSession).toHaveBeenCalledTimes(2);
    // The text stays in the tabs.
    expect(tabs.tabs.filter((t): t is QueryTab => t.kind === 'query' && t.connectionId === 'pg').map((t) => t.sql)).toEqual([
      'select 1',
      'select 2',
    ]);
    await tabs.closeConnectionTabs('pg');
  });

  it('asks before it cuts off a designer apply or grid save, which no query tab shows as running', async () => {
    const tabs = useTabs();
    const dialogs = useDialogs();
    const overview = tabs.openOverviewTab(profile);
    await settle();
    const session = tabs.tabs.find((t) => t.id === overview.id)?.session?.id ?? '';
    expect(session).not.toBe('');
    busy = [session];

    const cancelled = closeConnection(profile);
    await settle();
    const asked = dialogs.dialogs.filter((d) => d.spec.type === 'confirm');
    expect(asked).toHaveLength(1);
    const spec = asked[0]?.spec;
    expect(spec?.type === 'confirm' && spec.message).toBe('A session is still working. Its work is cancelled.');
    expect(spec?.type === 'confirm' && spec.danger).toBe(true);
    if (asked[0]) dialogs.close(asked[0].id, false);
    await expect(cancelled).resolves.toBe(false);
    expect(closeSession).not.toHaveBeenCalled();

    // When the check itself fails, it asks rather than closing in silence.
    busy = new Error('backend gone');
    const unchecked = closeConnection(profile);
    await settle();
    const again = dialogs.dialogs.find((d) => d.spec.type === 'confirm');
    expect(again?.spec.type === 'confirm' && again.spec.message).toContain('Varve could not check whether a session is still working.');
    if (again) dialogs.close(again.id, true);
    await expect(unchecked).resolves.toBe(true);
    expect(closeSession).toHaveBeenCalledTimes(1);
    await tabs.closeConnectionTabs('pg');
  });

  it('does not count a query tab\'s running statement twice', async () => {
    const tabs = useTabs();
    const dialogs = useDialogs();
    const query = tabs.openQueryTab(profile) as QueryTab;
    await settle();
    const tab = tabs.tabs.find((t): t is QueryTab => t.id === query.id && t.kind === 'query');
    if (!tab?.session) throw new Error('the tab did not connect');
    tab.running = true;
    busy = [tab.session.id];

    const closed = closeConnection(profile);
    await settle();
    const asked = dialogs.dialogs.find((d) => d.spec.type === 'confirm');
    expect(asked?.spec.type === 'confirm' && asked.spec.message).toBe(`A statement is still running in “${tab.title}”. It is cancelled.`);
    if (asked) dialogs.close(asked.id, true);
    await expect(closed).resolves.toBe(true);
    tab.running = false;
    await tabs.closeConnectionTabs('pg');
  });

  it('closes once when asked twice in a row, and waits for the object browser\'s sessions', async () => {
    const tabs = useTabs();
    const objects = useObjectBrowser();
    tabs.openOverviewTab(profile);
    await objects.browse('pg');
    await settle();
    let release: () => void = () => undefined;
    const held = new Promise<undefined>((resolve) => {
      release = () => resolve(undefined);
    });
    closeSession.mockImplementation(() => held);

    const first = closeConnection(profile);
    // The second click lands while the first still checks or closes.
    await expect(closeConnection(profile)).resolves.toBe(false);
    await settle();
    let done = false;
    void first.then(() => {
      done = true;
    });
    await settle();
    expect(closeSession).toHaveBeenCalledTimes(2);
    expect(done).toBe(false);
    release();
    await expect(first).resolves.toBe(true);
    closeSession.mockImplementation(async () => undefined);
    expect(useDialogs().dialogs.filter((d) => d.spec.type === 'confirm')).toHaveLength(0);

    // A later request for the same connection works again.
    await expect(closeConnection(profile)).resolves.toBe(true);
    await tabs.closeConnectionTabs('pg');
  });
});
