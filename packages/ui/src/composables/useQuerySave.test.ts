import type * as RecordsModule from '../api/records';
import type * as SessionsModule from '../api/sessions';
import type { ConnectionProfile } from '../api/records';
import type { SessionInfo } from '../api/sessions';
import { useDialogs } from './useDialogs';
import { useQuerySave } from './useQuerySave';
import { useTabs } from './useTabs';
import { useWorkspace } from './useWorkspace';

const profile = {
  id: 'p1',
  name: 'Orders',
  engine: 'postgres',
  environment: 'development',
  folderId: null,
  database: null,
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { redisDb: 0 },
  savedSecrets: [],
} as unknown as ConnectionProfile;

let sessionCount = 0;
const newSession = async (): Promise<SessionInfo> => {
  sessionCount += 1;
  return {
    id: `s${sessionCount}`,
    connectionId: 'p1',
    engine: 'postgres',
    server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 },
    environment: 'development',
    database: null,
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
};

vi.mock('../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: () => newSession(),
  openDatabaseSession: () => newSession(),
  closeSession: vi.fn(async () => undefined),
  cancel: vi.fn(async () => undefined),
}));
vi.mock('../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [profile], queries: [] })),
}));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
const container = { database: 'shop', schema: 'public', parent: null };

describe('unsaved editor tabs', () => {
  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  afterEach(async () => {
    const tabs = useTabs();
    for (const tab of [...tabs.tabs]) await tabs.closeTab(tab.id);
  });

  it('closes editor tabs without changes without asking', async () => {
    const tabs = useTabs();
    const tab = tabs.openTableDesigner(profile, null, container);
    await useQuerySave().closeWithCheck(tab.id);
    expect(tabs.tabs.some((t) => t.id === tab.id)).toBe(false);
  });

  it('asks Discard or Cancel for an editor with changes that are not applied', async () => {
    const tabs = useTabs();
    const dialogs = useDialogs();
    const tab = tabs.openTableDesigner(profile, null, container);
    tabs.setUnsaved(tab.id, true);

    const cancelled = useQuerySave().closeWithCheck(tab.id);
    await tick();
    const ask = dialogs.dialogs.find((d) => d.spec.type === 'confirm');
    expect(ask?.spec).toMatchObject({ title: 'Discard changes in “New table”?', confirmLabel: 'Discard', danger: true });
    if (ask) dialogs.close(ask.id, false);
    await cancelled;
    expect(tabs.tabs.some((t) => t.id === tab.id)).toBe(true);

    const discarded = useQuerySave().closeWithCheck(tab.id);
    await tick();
    const again = dialogs.dialogs.find((d) => d.spec.type === 'confirm');
    if (again) dialogs.close(again.id, true);
    await discarded;
    expect(tabs.tabs.some((t) => t.id === tab.id)).toBe(false);
  });

  it('asks about query tabs first (with Save) and then once about all editors, listing them', async () => {
    const tabs = useTabs();
    const dialogs = useDialogs();
    const query = tabs.openQueryTab(profile, null, 'SELECT 1');
    const a = tabs.openTableDesigner(profile, null, container);
    const b = tabs.openObjectTab(profile, { createKind: 'view', container });
    const clean = tabs.openLiveTab(profile, 'processes');
    tabs.setUnsaved(a.id, true);
    tabs.setUnsaved(b.id, true);

    const all = useQuerySave().confirmUnsavedTabs([query.id, a.id, b.id, clean.id]);
    await tick();
    const first = dialogs.dialogs.find((d) => d.spec.type === 'unsavedClose');
    expect(first).toBeDefined();
    if (first) dialogs.close(first.id, 'discard');
    await tick();
    const editors = dialogs.dialogs.find((d) => d.spec.type === 'confirm');
    expect(dialogs.dialogs).toHaveLength(1);
    expect(editors?.spec).toMatchObject({ title: 'Discard changes in 2 tabs?' });
    expect(editors?.spec.type === 'confirm' && editors.spec.message).toContain('“New table”, “New view”');
    if (editors) dialogs.close(editors.id, true);
    await expect(all).resolves.toBe(true);
  });

  it('stops before asking about editors when a query tab is cancelled', async () => {
    const tabs = useTabs();
    const dialogs = useDialogs();
    const query = tabs.openQueryTab(profile, null, 'SELECT 1');
    const editor = tabs.openTableDesigner(profile, null, container);
    tabs.setUnsaved(editor.id, true);
    const all = useQuerySave().confirmUnsavedTabs([query.id, editor.id]);
    await tick();
    const first = dialogs.dialogs.find((d) => d.spec.type === 'unsavedClose');
    if (first) dialogs.close(first.id, null);
    await expect(all).resolves.toBe(false);
    expect(dialogs.dialogs).toHaveLength(0);
  });
});
