import type * as RecordsModule from '../../api/records';
import type * as SessionsModule from '../../api/sessions';
import type { ConnectionProfile } from '../../api/records';
import type { ExecOutput, SessionInfo } from '../../api/sessions';
import { useTabs } from '../../composables/useTabs';
import type { QueryTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { canRun, runOnTab, runStep } from './runOnTab';
import type { RunDeps, RunnableTab } from './runOnTab';

const profile = {
  id: 'p1',
  name: 'Orders',
  engine: 'postgres',
  environment: 'development',
  folderId: null,
  database: 'shop',
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { redisDb: 0 },
  savedSecrets: [],
} as unknown as ConnectionProfile;

let sessionCount = 0;
const openDatabaseSession = vi.fn(async (_id: string, database: string): Promise<SessionInfo> => {
  sessionCount += 1;
  return { id: `s${sessionCount}`, connectionId: 'p1', engine: 'postgres', server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 }, environment: 'development', database, readOnly: false, confirmDestructive: false, purpose: 'user', openedAt: 0, encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false } };
});
const execute = vi.fn(async (_sessionId: string, _text: string): Promise<ExecOutput> => ({ results: [], notices: [], elapsedMs: 1 }));

vi.mock('../../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: (id: string) => openDatabaseSession(id, 'shop'),
  openDatabaseSession: (id: string, database: string) => openDatabaseSession(id, database),
  execute: (sessionId: string, text: string) => execute(sessionId, text),
  closeSession: vi.fn(async () => undefined),
  cancel: vi.fn(async () => undefined),
}));
vi.mock('../../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [profile], queries: [] })),
}));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
async function settle(): Promise<void> {
  for (let i = 0; i < 5; i += 1) await tick();
}

const session = { id: 's' } as RunnableTab['session'];

describe('runStep and canRun', () => {
  it('runs on a connected tab, connects a tab without a session first, and waits otherwise', () => {
    expect(runStep({ status: 'ready', session, running: false })).toBe('run');
    expect(runStep({ status: 'cancelled', session: null, running: false })).toBe('connect');
    expect(runStep({ status: 'error', session: null, running: false })).toBe('connect');
    expect(runStep({ status: 'connecting', session: null, running: false })).toBe('wait');
    // A reconnect keeps the old session until the new one is ready: not a moment to run.
    expect(runStep({ status: 'connecting', session, running: false })).toBe('wait');
    expect(runStep({ status: 'ready', session, running: true })).toBe('wait');
  });

  it('keeps Run usable on a tab that is not connected, but not without its connection', () => {
    expect(canRun({ status: 'cancelled', session: null, running: false }, true)).toBe(true);
    expect(canRun({ status: 'error', session: null, running: false }, false)).toBe(false);
    expect(canRun({ status: 'connecting', session: null, running: false }, true)).toBe(false);
    expect(canRun({ status: 'ready', session, running: false }, true)).toBe(true);
  });
});

describe('runOnTab with a fake tab', () => {
  /** A tab whose connect behaves like `useTabs`: `connecting` at once, then `outcome`. */
  function fake(start: RunnableTab['status'], outcome: 'ready' | 'cancelled' | 'error', edit?: string) {
    const tab: RunnableTab = { status: start, session: start === 'ready' ? session : null, running: false, sql: 'select 1' };
    const ran: string[] = [];
    let finish: () => void = () => undefined;
    const deps: RunDeps = {
      find: (id) => (id === 't' ? tab : undefined),
      reconnect: vi.fn(() => {
        tab.status = 'connecting';
        return new Promise<void>((resolve) => {
          finish = () => {
            if (edit !== undefined) tab.sql = edit;
            tab.status = outcome;
            tab.session = outcome === 'ready' ? session : null;
            resolve();
          };
        });
      }),
      runQuery: vi.fn(async (_id: string, text: string) => {
        ran.push(text);
      }),
    };
    return { tab, deps, ran, finish: () => finish() };
  }

  it('runs at once on a connected tab', async () => {
    const f = fake('ready', 'ready');
    await runOnTab(f.deps, 't', { text: 'select 1', offset: 0 }, () => null);
    expect(f.deps.reconnect).not.toHaveBeenCalled();
    expect(f.ran).toEqual(['select 1']);
  });

  it('connects a tab that is not connected and then runs the statement once', async () => {
    const f = fake('cancelled', 'ready');
    const first = runOnTab(f.deps, 't', { text: 'select 1', offset: 0 }, () => null);
    // A second Run while the first one connects starts nothing.
    const second = runOnTab(f.deps, 't', { text: 'select 1', offset: 0 }, () => null);
    expect(f.deps.reconnect).toHaveBeenCalledTimes(1);
    f.finish();
    await Promise.all([first, second]);
    expect(f.ran).toEqual(['select 1']);
  });

  it('runs nothing when the connect was cancelled or failed', async () => {
    for (const outcome of ['cancelled', 'error'] as const) {
      const f = fake('error', outcome);
      const running = runOnTab(f.deps, 't', { text: 'select 1', offset: 0 }, () => null);
      f.finish();
      await running;
      expect(f.ran).toEqual([]);
      expect(f.tab.status).toBe(outcome);
    }
  });

  it('starts nothing on a tab that is already connecting', async () => {
    const f = fake('connecting', 'ready');
    await runOnTab(f.deps, 't', { text: 'select 1', offset: 0 }, () => null);
    expect(f.deps.reconnect).not.toHaveBeenCalled();
    expect(f.ran).toEqual([]);
  });

  it('reads the statement again when the editor changed while connecting', async () => {
    const f = fake('cancelled', 'ready', 'select 2');
    const running = runOnTab(f.deps, 't', { text: 'select 1', offset: 0 }, () => ({ text: 'select 2', offset: 0 }));
    f.finish();
    await running;
    expect(f.ran).toEqual(['select 2']);
  });

  it('neither connects nor runs for an empty statement', async () => {
    const f = fake('cancelled', 'ready');
    await runOnTab(f.deps, 't', { text: '  \n', offset: 0 }, () => null);
    expect(f.deps.reconnect).not.toHaveBeenCalled();
    expect(f.ran).toEqual([]);
  });
});

describe('runOnTab on a closed connection', () => {
  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  function deps(): RunDeps {
    const tabs = useTabs();
    return {
      find: (id) => {
        const tab = tabs.tabs.find((t) => t.id === id);
        return tab?.kind === 'query' ? tab : undefined;
      },
      reconnect: tabs.reconnect,
      runQuery: tabs.runQuery,
    };
  }

  it('opens a new session after Close connection and runs the statement on it, once', async () => {
    const tabs = useTabs();
    const tab = tabs.openQueryTab(profile) as QueryTab;
    await settle();
    await tabs.disconnectConnection(profile.id);
    expect(tabs.tabs.find((t) => t.id === tab.id)?.status).toBe('cancelled');
    execute.mockClear();
    openDatabaseSession.mockClear();

    await Promise.all([
      runOnTab(deps(), tab.id, { text: 'select 1', offset: 0 }, () => null),
      runOnTab(deps(), tab.id, { text: 'select 1', offset: 0 }, () => null),
    ]);
    await settle();

    const after = tabs.tabs.find((t) => t.id === tab.id) as QueryTab;
    expect(openDatabaseSession).toHaveBeenCalledTimes(1);
    expect(execute).toHaveBeenCalledTimes(1);
    expect(execute).toHaveBeenCalledWith(after.session?.id, 'select 1');
    expect(after.status).toBe('ready');
    expect(after.run?.text).toBe('select 1');
    await tabs.closeTab(tab.id);
  });

  it('runs nothing when the new session cannot be opened', async () => {
    const tabs = useTabs();
    const tab = tabs.openQueryTab(profile) as QueryTab;
    await settle();
    await tabs.disconnectConnection(profile.id);
    execute.mockClear();
    openDatabaseSession.mockRejectedValueOnce({ kind: 'connection', message: 'Connection refused' });

    await runOnTab(deps(), tab.id, { text: 'select 1', offset: 0 }, () => null);
    await settle();

    const after = tabs.tabs.find((t) => t.id === tab.id) as QueryTab;
    expect(execute).not.toHaveBeenCalled();
    expect(after.status).toBe('error');
    expect(after.session).toBeNull();
    expect(after.run).toBeNull();
    await tabs.closeTab(tab.id);
  });
});
