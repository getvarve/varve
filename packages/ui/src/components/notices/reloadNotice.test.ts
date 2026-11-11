import { flushPromises } from '@vue/test-utils';
import type * as SessionsModule from '../../api/sessions';
import type { ConnectionProfile } from '../../api/records';

const reloadReport = vi.fn(async () => ({ sessions: [{ connectionId: 'c1', inTransaction: true }], opening: [] }));
vi.mock('../../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  reloadReport: () => reloadReport(),
}));

/** A fresh module per test: the report is module state, read once per page load. */
async function load() {
  vi.resetModules();
  const { useReloadNotice } = await import('./reloadNotice');
  const { useWorkspace } = await import('../../composables/useWorkspace');
  return { notice: useReloadNotice(), workspace: useWorkspace() };
}

const shop = { id: 'c1', name: 'Shop', folderId: null } as unknown as ConnectionProfile;

beforeEach(() => {
  reloadReport.mockClear();
});

describe('the reload notice', () => {
  it('waits for the connections before naming them', async () => {
    const { notice, workspace } = await load();
    await notice.load();
    expect(notice.text.value).toBeNull();
    workspace.records.value = { folders: [], connections: [shop], queries: [] };
    workspace.status.loaded = true;
    expect(notice.text.value).toContain('Shop');
    expect(notice.text.value).not.toContain('a removed connection');
  });

  it('reads the report once and keeps it across mounts until dismissed', async () => {
    const { notice, workspace } = await load();
    workspace.status.loaded = true;
    await notice.load();
    await notice.load();
    await flushPromises();
    expect(reloadReport).toHaveBeenCalledTimes(1);
    expect(notice.text.value).toMatch(/^The window was reloaded/);
    notice.dismiss();
    expect(notice.text.value).toBeNull();
    await notice.load();
    expect(notice.text.value).toBeNull();
  });

  it('names a connection that is gone once the connections could not be loaded', async () => {
    const { notice, workspace } = await load();
    workspace.status.error = 'offline';
    await notice.load();
    expect(notice.text.value).toContain('a removed connection');
  });
});
