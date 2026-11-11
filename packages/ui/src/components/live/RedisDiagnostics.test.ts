import { flushPromises, mount } from '@vue/test-utils';
import type * as LiveModule from '../../api/live';
import type { SlowLog } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import RedisDiagnostics from './RedisDiagnostics.vue';

let finishSlowlog: ((log: SlowLog) => void) | null = null;

vi.mock('../../api/live', async (original) => ({
  ...(await original<typeof LiveModule>()),
  redisSlowlog: vi.fn(
    () =>
      new Promise<SlowLog>((resolve) => {
        finishSlowlog = resolve;
      }),
  ),
  redisLatency: vi.fn(async () => ({ thresholdMs: 0, events: [] })),
  redisClients: vi.fn(async () => []),
  redisInfo: vi.fn(async () => {
    throw new Error('INFO is not allowed');
  }),
}));

const session = {
  id: 's1',
  connectionId: 'r1',
  engine: 'redis',
  server: { engine: 'redis', version: '8.0.0', major: 8, minor: 0, patch: 0 },
  environment: 'development',
  database: null,
  readOnly: false,
  confirmDestructive: false,
  purpose: 'user',
  openedAt: 0,
  encryption: { tls: false, mode: 'disable', passwordUnencrypted: true },
} as unknown as SessionInfo;

describe('RedisDiagnostics focus', () => {
  const scrolled: Element[] = [];
  const original = HTMLElement.prototype.scrollIntoView;
  beforeAll(() => {
    HTMLElement.prototype.scrollIntoView = function scrollIntoView(this: Element) {
      scrolled.push(this);
    };
  });
  afterAll(() => {
    HTMLElement.prototype.scrollIntoView = original;
  });

  it('scrolls to the section it was opened for once the data is in, not before', async () => {
    const wrapper = mount(RedisDiagnostics, { props: { session, connectionName: 'Cache', focus: 'clients' }, attachTo: document.body });
    await flushPromises();
    // The slow log is still loading: its height is not known yet.
    expect(scrolled).toEqual([]);
    expect(finishSlowlog).not.toBeNull();
    finishSlowlog?.({ entries: [], length: 0, thresholdUs: null, maxLen: null });
    await flushPromises();
    expect(scrolled).toHaveLength(1);
    expect(scrolled[0]?.getAttribute('aria-label')).toBe('Client list');
    wrapper.unmount();
  });
});
