import type { CmdError } from '../api/errors';
import { RECORDS_CHANGED, SETTINGS_CHANGED } from '../api/mcp';
import type { ConnectionProfile, WorkspaceRecords } from '../api/records';
import { rememberTypedSecrets, typedSecretsOf } from './useConnect';
import { installExternalChanges, useExternalChanges } from './useExternalChanges';
import { useToasts } from './useToasts';

const handlers = new Map<string, () => void>();
const unlisten = vi.fn();
const refreshInfo = vi.fn(async () => undefined);

const profile: ConnectionProfile = {
  id: 'p1',
  workspaceId: 'personal',
  folderId: null,
  name: 'Orders',
  color: null,
  sortOrder: 0,
  engine: 'postgres',
  host: 'db',
  port: 5432,
  database: null,
  username: 'kim',
  environment: 'production',
  tls: { mode: 'prefer', caPath: null, clientCertPath: null, clientKeyPath: null },
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { readOnly: false, confirmDestructive: true, statementTimeoutMs: null, keepaliveS: null, initSql: null, showSystemObjects: false, redisDb: 0 },
  savedSecrets: [],
  createdAt: 0,
  updatedAt: 0,
  version: 1,
};

const workspace = vi.hoisted(() => ({
  records: { value: { folders: [], connections: [], queries: [] } as WorkspaceRecords },
  status: { loaded: true, loading: false, error: null as string | null },
  next: null as WorkspaceRecords | null,
  failNext: false,
}));
async function reload(): Promise<void> {
  if (workspace.failNext) {
    workspace.status.error = 'app.db is locked';
    return;
  }
  workspace.status.error = null;
  if (workspace.next) workspace.records.value = workspace.next;
}
const refresh = vi.fn(reload);
interface TestTab {
  id: string;
  connectionId: string;
  session: { id: string } | null;
  status: 'ready' | 'error';
  connectError: CmdError | null;
}
const tabs = vi.hoisted(() => ({ list: [] as TestTab[] }));
const closeTab = vi.fn(async (id: string) => {
  tabs.list = tabs.list.filter((t) => t.id !== id);
});
const markLost = vi.fn((id: string, error: CmdError) => {
  const tab = tabs.list.find((t) => t.id === id);
  if (!tab) return;
  tab.status = 'error';
  tab.connectError = error;
  tab.session = null;
});

/** A connected tab. */
function tab(id: string, connectionId: string): TestTab {
  return { id, connectionId, session: { id: `s-${id}` }, status: 'ready', connectError: null };
}
const confirmUnsavedTabs = vi.fn(async (_ids: readonly string[]) => true);

vi.mock('@tauri-apps/api/core', () => ({ isTauri: () => true }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: async (event: string, handler: () => void) => {
    handlers.set(event, handler);
    return unlisten;
  },
}));
vi.mock('./useWorkspace', () => ({
  useWorkspace: () => ({ records: workspace.records, status: workspace.status, refresh }),
}));
vi.mock('./useUpdates', () => ({ useUpdates: () => ({ refreshInfo }) }));
vi.mock('./useTabs', () => ({
  useTabs: () => ({
    get tabs() {
      return tabs.list;
    },
    closeTab,
    markLost,
  }),
}));
vi.mock('./useQuerySave', () => ({ useQuerySave: () => ({ confirmUnsavedTabs }) }));

const settle = async (): Promise<void> => {
  for (let i = 0; i < 10; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
};

function records(...connections: ConnectionProfile[]): WorkspaceRecords {
  return { folders: [], connections, queries: [] };
}

describe('installExternalChanges', () => {
  beforeEach(() => {
    refresh.mockClear();
    refresh.mockImplementation(reload);
    closeTab.mockClear();
    markLost.mockClear();
    confirmUnsavedTabs.mockReset();
    confirmUnsavedTabs.mockImplementation(async () => true);
    workspace.next = null;
    for (const toast of [...useToasts().toasts]) useToasts().dismiss(toast.id);
    workspace.failNext = false;
    workspace.status.error = null;
    tabs.list = [];
  });

  it('reloads the tree and the settings after an MCP change, until removed', async () => {
    const remove = await installExternalChanges();
    const { settingsRevision } = useExternalChanges();

    handlers.get(RECORDS_CHANGED)?.();
    await settle();
    expect(refresh).toHaveBeenCalledTimes(1);

    const before = settingsRevision.value;
    handlers.get(SETTINGS_CHANGED)?.();
    expect(settingsRevision.value).toBe(before + 1);
    expect(refreshInfo).toHaveBeenCalledTimes(1);

    remove();
    expect(unlisten).toHaveBeenCalledTimes(2);
  });

  it('drops a typed password when an MCP client moved the connection to another host', async () => {
    const kept = { ...profile, id: 'kept' };
    // Through a tunnel: the SSH secret belongs to the bastion, which stays.
    const moved = { ...profile, id: 'moved', ssh: { ...profile.ssh, enabled: true, host: 'bastion', username: 'ops' } };
    workspace.records.value = records(kept, moved);
    rememberTypedSecrets('kept', { password: 'pw-kept' }, 1);
    rememberTypedSecrets('moved', { password: 'pw-moved', sshSecret: 'ssh' }, 1);
    workspace.next = records({ ...kept, name: 'Renamed', version: 2 }, { ...moved, host: 'attacker.example', version: 2 });

    await installExternalChanges();
    handlers.get(RECORDS_CHANGED)?.();
    await settle();

    expect(typedSecretsOf('kept')).toEqual({ password: 'pw-kept' });
    expect(typedSecretsOf('moved')).toEqual({ sshSecret: 'ssh' });
  });

  it('forgets the secrets and closes the tabs of a connection an MCP client deleted', async () => {
    const gone = { ...profile, id: 'gone' };
    const other = { ...profile, id: 'other' };
    workspace.records.value = records(gone, other);
    rememberTypedSecrets('gone', { password: 'pw' }, 1);
    tabs.list = [tab('t1', 'gone'), tab('t2', 'other'), tab('t3', 'gone')];
    // The user keeps the tab with unsaved text.
    confirmUnsavedTabs.mockImplementation(async (ids) => ids[0] !== 't3');
    workspace.next = records(other);

    await installExternalChanges();
    handlers.get(RECORDS_CHANGED)?.();
    await settle();

    expect(typedSecretsOf('gone')).toEqual({});
    expect(confirmUnsavedTabs.mock.calls.map(([ids]) => ids)).toEqual([['t1'], ['t3']]);
    expect(closeTab.mock.calls.map(([id]) => id)).toEqual(['t1']);
    expect(tabs.list.map((t) => t.id)).toEqual(['t2', 't3']);
    // The kept tab has no session any more and says why.
    expect(tabs.list[1]).toMatchObject({ session: null, status: 'error', connectError: { kind: 'notFound' } });
    expect(useToasts().toasts.map((t) => t.title)).toEqual(['Orders was deleted']);

    // A later change does not ask about the kept tab again.
    handlers.get(RECORDS_CHANGED)?.();
    await settle();
    expect(confirmUnsavedTabs).toHaveBeenCalledTimes(2);
    expect(useToasts().toasts).toHaveLength(1);
  });

  it('closes the tabs of a deleted connection when another reload already showed the delete', async () => {
    const other = { ...profile, id: 'other' };
    // The records the window holds are the new ones already (the editor or
    // a user action reloaded them between the delete and this event).
    workspace.records.value = records(other);
    workspace.next = records(other);
    rememberTypedSecrets('gone', { password: 'pw' }, 1);
    tabs.list = [tab('t1', 'gone'), tab('t2', 'other')];

    await installExternalChanges();
    handlers.get(RECORDS_CHANGED)?.();
    await settle();

    expect(typedSecretsOf('gone')).toEqual({});
    expect(closeTab.mock.calls.map(([id]) => id)).toEqual(['t1']);
    expect(tabs.list.map((t) => t.id)).toEqual(['t2']);
    expect(useToasts().toasts.map((t) => t.title)).toEqual(['A connection was deleted']);
  });

  it('follows two changes close together one after the other, and asks about each tab once', async () => {
    const gone = { ...profile, id: 'gone' };
    const other = { ...profile, id: 'other' };
    workspace.records.value = records(gone, other);
    workspace.next = records(other);
    tabs.list = [tab('t1', 'gone'), tab('t2', 'other')];
    // Reloads take a while, and the user takes a while to answer.
    let running = 0;
    let overlapped = false;
    refresh.mockImplementation(async () => {
      running += 1;
      overlapped ||= running > 1;
      await new Promise((resolve) => setTimeout(resolve, 5));
      await reload();
      running -= 1;
    });
    const answers: ((keep: boolean) => void)[] = [];
    confirmUnsavedTabs.mockImplementation(() => new Promise<boolean>((resolve) => answers.push(resolve)));

    await installExternalChanges();
    handlers.get(RECORDS_CHANGED)?.();
    handlers.get(RECORDS_CHANGED)?.();
    await new Promise((resolve) => setTimeout(resolve, 30));

    // Both reloads ran, in turn, while the question of the first is open.
    expect(refresh).toHaveBeenCalledTimes(2);
    expect(overlapped).toBe(false);
    expect(answers).toHaveLength(1);
    // The user keeps the tab: the second change does not ask again.
    answers[0]?.(false);
    await settle();
    expect(confirmUnsavedTabs).toHaveBeenCalledTimes(1);
    expect(closeTab).not.toHaveBeenCalled();
    expect(markLost).toHaveBeenCalledTimes(1);
    expect(useToasts().toasts.map((t) => t.title)).toEqual(['Orders was deleted']);
  });

  it('keeps following changes after one of them failed', async () => {
    const gone = { ...profile, id: 'gone' };
    workspace.records.value = records(gone);
    workspace.next = records();
    tabs.list = [tab('t1', 'gone')];
    refresh.mockImplementationOnce(async () => {
      throw new Error('the window is closing');
    });

    await installExternalChanges();
    handlers.get(RECORDS_CHANGED)?.();
    await settle();
    expect(closeTab).not.toHaveBeenCalled();
    expect(useToasts().toasts.map((t) => t.title)).toEqual(['Could not follow a change to the connections']);

    handlers.get(RECORDS_CHANGED)?.();
    await settle();
    expect(closeTab.mock.calls.map(([id]) => id)).toEqual(['t1']);
  });

  it('forgets every typed secret when the records cannot be read after a change', async () => {
    workspace.records.value = records(profile);
    rememberTypedSecrets('p1', { password: 'pw' }, 1);
    workspace.failNext = true;

    await installExternalChanges();
    handlers.get(RECORDS_CHANGED)?.();
    await settle();

    expect(typedSecretsOf('p1')).toEqual({});
    expect(closeTab).not.toHaveBeenCalled();
  });
});
