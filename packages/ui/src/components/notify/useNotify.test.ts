import { flushPromises } from '@vue/test-utils';
import type { NotifyState } from '../../api/notify';

const h = vi.hoisted(() => ({
  listens: [] as string[],
  unlistened: [] as string[],
  stopped: [] as string[],
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async (event: string) => {
    h.listens.push(event);
    return () => h.unlistened.push(event);
  }),
}));
vi.mock('../../api/notify', async (original) => ({
  ...(await original<object>()),
  notifyState: vi.fn(
    async (sessionId: string): Promise<NotifyState> => ({
      sessionId,
      listenerId: 1,
      version: 1,
      status: 'connected',
      channels: [],
      backendPid: 1,
      attempt: 0,
      retryInMs: null,
      error: null,
    } as unknown as NotifyState),
  ),
  notifyStop: vi.fn(async (sessionId: string) => {
    h.stopped.push(sessionId);
  }),
}));

describe('useNotify close', () => {
  it('stops the listener, forgets the panel and removes the event listeners with the last panel', async () => {
    const { useNotify } = await import('./useNotify');
    const notify = useNotify();
    notify.panel('t1', 's1');
    notify.panel('t2', 's2');
    await notify.refresh('t1');
    expect(h.listens).toHaveLength(2);

    await notify.close('t1');
    expect(h.stopped).toEqual(['s1']);
    expect(notify.panels.has('t1')).toBe(false);
    // Another panel still needs the listeners.
    expect(h.unlistened).toEqual([]);

    await notify.close('t2');
    await flushPromises();
    expect(notify.panels.size).toBe(0);
    expect(h.unlistened.sort()).toEqual([...h.listens].sort());

    // A panel opened afterwards gets a fresh pair.
    notify.panel('t3', 's3');
    await notify.refresh('t3');
    expect(h.listens).toHaveLength(4);
    await notify.close('t3');
    expect(h.unlistened).toHaveLength(4);
  });
});
