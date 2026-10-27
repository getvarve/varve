import { flushPromises, mount } from '@vue/test-utils';
import type * as LiveModule from '../../api/live';
import type { MemoryAnalysis } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import RedisMemory from './RedisMemory.vue';

const runs: (string | null)[] = [];
const stops: [string, string][] = [];
let finish: ((analysis: MemoryAnalysis) => void) | null = null;

const partial: MemoryAnalysis = {
  scannedKeys: 120,
  dbSize: 50_000,
  complete: false,
  stoppedBy: 'stopped',
  elapsedMs: 150,
  totalBytes: 12_000,
  prefixes: [],
  otherPrefixes: null,
  types: [],
};

vi.mock('../../api/live', async (original) => ({
  ...(await original<typeof LiveModule>()),
  redisInfo: vi.fn(() => new Promise(() => undefined)),
  redisMemoryAnalysis: vi.fn(
    (_s: string, _o: unknown, runId: string | null) =>
      new Promise<MemoryAnalysis>((resolve) => {
        runs.push(runId);
        finish = resolve;
      }),
  ),
  redisMemoryAnalysisStop: vi.fn(async (sessionId: string, runId: string) => {
    stops.push([sessionId, runId]);
    finish?.(partial);
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

function button(wrapper: ReturnType<typeof mount>, text: string) {
  return wrapper.findAll('button').find((b) => b.text() === text);
}

describe('RedisMemory Stop', () => {
  beforeEach(() => {
    runs.length = 0;
    stops.length = 0;
    finish = null;
  });

  it('stops the running analysis by its run id and shows the partial result', async () => {
    const wrapper = mount(RedisMemory, { props: { session, connectionName: 'Cache' }, attachTo: document.body });
    await flushPromises();
    expect(button(wrapper, 'Stop')).toBeUndefined();
    await wrapper.find('form[aria-label="Memory analysis"]').trigger('submit');
    await flushPromises();
    expect(runs).toHaveLength(1);
    const runId = runs[0];
    expect(runId).toBeTruthy();
    await button(wrapper, 'Stop')?.trigger('click');
    await flushPromises();
    expect(stops).toEqual([['s1', runId]]);
    expect(wrapper.text()).toContain('stopped by you, so this is a partial result');
    expect(button(wrapper, 'Stop')).toBeUndefined();
    wrapper.unmount();
  });

  it('stops a running analysis when the panel closes', async () => {
    const wrapper = mount(RedisMemory, { props: { session, connectionName: 'Cache' }, attachTo: document.body });
    await flushPromises();
    await wrapper.find('form[aria-label="Memory analysis"]').trigger('submit');
    await flushPromises();
    const runId = runs[0];
    wrapper.unmount();
    await flushPromises();
    expect(stops).toEqual([['s1', runId]]);
  });
});
