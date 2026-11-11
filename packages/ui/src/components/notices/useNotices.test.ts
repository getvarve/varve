import { flushPromises, mount } from '@vue/test-utils';
import { defineComponent, onMounted } from 'vue';
import type * as NoticesModule from '../../api/notices';
import type { PendingRestore } from '../../api/notices';
import { useDialogs } from '../../composables/useDialogs';
import { useWorkspace } from '../../composables/useWorkspace';
import { restoreKey, useNotices } from './useNotices';

const entry: PendingRestore = {
  server: 'mariadb:uid|db|3306|/var/lib/mysql|1',
  route: 'db.internal:3306',
  database: 'shop',
  collation: 'utf8mb4_general_ci',
  temporaries: ['utf8mb4_bin'],
  verified: false,
  connectionId: 'c1',
  recordedAt: 0,
};

const restoreCalls: { sessionId: string; database: string; confirmed: boolean }[] = [];
let restoreAnswers: unknown[] = [];
let pending: PendingRestore[] = [entry];
const dismissCalls: [string, string][] = [];
let keychain: NoticesModule.PendingDeletion[] = [];

vi.mock('../../api/notices', async (original) => ({
  ...(await original<typeof NoticesModule>()),
  ddlPendingRestores: vi.fn(async () => pending),
  ddlRestoreCollation: vi.fn(async (sessionId: string, database: string, confirmed: boolean) => {
    restoreCalls.push({ sessionId, database, confirmed });
    const answer = restoreAnswers.shift();
    if (answer !== undefined) throw answer;
  }),
  ddlDismissPendingRestore: vi.fn(async (server: string, database: string) => {
    dismissCalls.push([server, database]);
  }),
  pendingKeychainDeletions: vi.fn(async () => keychain),
  retryKeychainCleanup: vi.fn(async () => ['still locked']),
}));

/** Answers the newest open dialog and returns its spec. */
function answer(value: unknown) {
  const dialogs = useDialogs();
  const open = dialogs.dialogs.at(-1);
  if (!open) throw new Error('no dialog open');
  dialogs.close(open.id, value);
  return open.spec;
}

beforeEach(() => {
  restoreCalls.length = 0;
  dismissCalls.length = 0;
  restoreAnswers = [];
  pending = [entry];
});

describe('useNotices restore', () => {
  it('shows the unverified-server statement verbatim and runs again with confirmed after a yes', async () => {
    const notices = useNotices();
    await notices.refreshRestores();
    expect(notices.state.restores).toHaveLength(1);
    const statement = 'Varve cannot prove that this is the server the restore was recorded on (connection Shop, 2025-10-09T08:53:20Z).';
    restoreAnswers = [{ kind: 'session', error: { kind: 'confirmationRequired', environment: 'production', statements: [statement] } }];
    const run = notices.restore(entry, { sessionId: 's1' });
    await flushPromises();
    expect(notices.state.busy.has(restoreKey(entry))).toBe(true);
    const spec = answer(true);
    // The destructive dialog with the environment, so production asks for the connection name.
    expect(spec.type).toBe('destructive');
    expect(spec.type === 'destructive' ? [spec.environment, spec.statements] : null).toEqual(['production', [statement]]);
    pending = [];
    expect(await run).toBe(true);
    expect(restoreCalls).toEqual([
      { sessionId: 's1', database: 'shop', confirmed: false },
      { sessionId: 's1', database: 'shop', confirmed: true },
    ]);
    expect(notices.state.restores).toEqual([]);
    expect(notices.state.busy.has(restoreKey(entry))).toBe(false);
  });

  it('does not run again when the confirmation is declined', async () => {
    const notices = useNotices();
    restoreAnswers = [{ kind: 'session', error: { kind: 'confirmationRequired', environment: 'development', statements: ['x'] } }];
    const run = notices.restore(entry, { sessionId: 's1' });
    await flushPromises();
    answer(false);
    expect(await run).toBe(false);
    expect(restoreCalls).toHaveLength(1);
  });

  it('offers Dismiss for a stale entry and dismisses after a yes', async () => {
    const notices = useNotices();
    await notices.refreshRestores();
    restoreAnswers = [{ kind: 'invalid', message: 'The database is at another collation now.' }];
    await notices.restore(entry, { sessionId: 's1' });
    expect(notices.state.problems.get(restoreKey(entry))?.offerDismiss).toBe(true);

    const dismissed = notices.dismiss(entry);
    await flushPromises();
    const spec = answer(true);
    expect(spec.type === 'confirm' ? spec.message : '').toContain('keeps its current collation');
    pending = [];
    expect(await dismissed).toBe(true);
    expect(dismissCalls).toEqual([[entry.server, 'shop']]);
    expect(notices.state.problems.has(restoreKey(entry))).toBe(false);
  });

  it('says the tab is on another server for notFound', async () => {
    const notices = useNotices();
    await notices.refreshRestores();
    restoreAnswers = [{ kind: 'notFound', message: 'Nothing pending' }];
    await notices.restore(entry, { sessionId: 's9' });
    expect(notices.state.problems.get(restoreKey(entry))?.message).toContain('another server');
  });
});

describe('useNotices keychain', () => {
  it('shows the messages of entries that still fail after a retry', async () => {
    keychain = [{ connectionId: 'c1', kind: 'db_password' }];
    const notices = useNotices();
    await notices.refreshKeychain();
    expect(notices.state.keychain).toHaveLength(1);
    await notices.retryKeychain();
    expect(notices.state.keychainMessages).toEqual(['still locked']);
    expect(notices.state.retrying).toBe(false);
    // Once the backend removed them, the notice and its messages go away.
    keychain = [];
    await notices.refreshKeychain();
    expect(notices.state.keychainMessages).toEqual([]);
  });
});

describe('useNotices start', () => {
  it('keeps watching the connections after the component that started it unmounted, and reloads on every mount', async () => {
    const { pendingKeychainDeletions, ddlPendingRestores } = await import('../../api/notices');
    const reads = vi.mocked(pendingKeychainDeletions);
    const restoreReads = vi.mocked(ddlPendingRestores);
    keychain = [];
    const Host = defineComponent({
      setup() {
        onMounted(() => useNotices().start());
        return () => null;
      },
    });
    const first = mount(Host);
    await flushPromises();
    first.unmount();
    reads.mockClear();
    restoreReads.mockClear();

    // The records change while no host is mounted (another page is shown): the list is still reloaded.
    useWorkspace().records.value = { folders: [], connections: [], queries: [] };
    await flushPromises();
    expect(reads).toHaveBeenCalledTimes(1);

    // Coming back reloads both lists.
    const second = mount(Host);
    await flushPromises();
    expect(reads).toHaveBeenCalledTimes(2);
    expect(restoreReads).toHaveBeenCalledTimes(1);
    second.unmount();
  });
});
