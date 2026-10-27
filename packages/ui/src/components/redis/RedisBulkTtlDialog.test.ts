import { flushPromises, mount } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { BulkTtlRequest, BulkTtlStatus } from '../../api/redis';
import RedisBulkTtlDialog from './RedisBulkTtlDialog.vue';

const starts: { request: BulkTtlRequest; confirmed: boolean }[] = [];
const stops: string[] = [];
const asked: { statements: string[]; warnings?: string[] }[] = [];
let statuses: BulkTtlStatus[] = [];
let confirmOnProduction = true;
let jobs: BulkTtlStatus[] = [];
let statusFails = false;
let stopFails = false;
let cluster = false;

function status(state: BulkTtlStatus['state'], changed: string): BulkTtlStatus {
  return {
    jobId: 'job-1',
    state,
    counts: { steps: '1', matched: '1200', changed, skipped: '0' },
    error: null,
    elapsedMs: 5,
    db: 2,
    action: { kind: 'expire', ttlMs: '60000' },
    filter: { pattern: 'user:*', keyType: null },
  };
}

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisBulkTtlPreview: vi.fn(async () => ({ keys: '1200', complete: true, sample: ['user:1', 'user:2'], db: 2, cluster })),
  redisBulkTtlJobs: vi.fn(async () => jobs),
  redisBulkTtlStart: vi.fn(async (_s: string, request: BulkTtlRequest, confirmed: boolean) => {
    starts.push({ request, confirmed });
    if (confirmOnProduction && !confirmed) {
      throw { kind: 'session', error: { kind: 'confirmationRequired', environment: 'production', statements: ['PEXPIRE on keys matching user:*'] } };
    }
    return { jobId: 'job-1' };
  }),
  redisBulkTtlStatus: vi.fn(async () => {
    if (statusFails) throw { kind: 'invalid', message: 'This bulk change is no longer known' };
    return statuses.shift() ?? status('complete', '1200');
  }),
  redisBulkTtlStop: vi.fn(async (_s: string, jobId: string) => {
    stops.push(jobId);
    if (stopFails) throw { kind: 'invalid', message: 'This bulk change is no longer known' };
    return status('running', '500');
  }),
}));

vi.mock('../../composables/useDialogs', () => ({
  useDialogs: () => ({
    askDestructive: vi.fn(async (request: { statements: string[]; warnings?: string[] }) => {
      asked.push(request);
      return true;
    }),
  }),
}));

const wait = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

function button(text: string): HTMLButtonElement | undefined {
  return [...document.body.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === text);
}

beforeEach(() => {
  starts.length = 0;
  stops.length = 0;
  asked.length = 0;
  statuses = [];
  confirmOnProduction = true;
  jobs = [];
  statusFails = false;
  stopFails = false;
  cluster = false;
});

describe('RedisBulkTtlDialog', () => {
  beforeEach(() => {
    starts.length = 0;
    stops.length = 0;
    asked.length = 0;
    statuses = [];
    confirmOnProduction = true;
    jobs = [];
    statusFails = false;
    stopFails = false;
    cluster = false;
  });

  it('counts first, confirms on production, runs with progress and reports the end', async () => {
    statuses = [status('running', '500'), status('complete', '1200')];
    const wrapper = mount(RedisBulkTtlDialog, {
      props: { sessionId: 's1', filter: { pattern: 'user:*', keyType: null }, connectionName: 'Cache' },
      attachTo: document.body,
    });
    await flushPromises();
    expect(document.body.textContent).toContain('1,200 keys match on database 2.');
    const seconds = document.body.querySelector<HTMLInputElement>('input[aria-label="Seconds"]');
    if (!seconds) throw new Error('no seconds field');
    seconds.value = '60';
    seconds.dispatchEvent(new Event('input'));
    await flushPromises();
    button('Set TTL')?.click();
    await flushPromises();
    expect(starts.map((s) => s.confirmed)).toEqual([false, true]);
    expect(starts[1]?.request).toEqual({ filter: { pattern: 'user:*', keyType: null }, action: { kind: 'expire', ttlMs: '60000' }, db: 2 });
    expect(asked[0]?.warnings?.[0]).toContain('1,200 keys');
    await wait(350);
    await flushPromises();
    expect(document.body.textContent).toContain('500 of 1,200 keys changed so far');
    await wait(350);
    await flushPromises();
    expect(document.body.textContent).toContain('Done: 1,200 keys changed');
    button('Close')?.click();
    expect(wrapper.emitted('close')?.[0]).toEqual([true]);
    wrapper.unmount();
  });

  it('stops a running job and keeps polling until it ends', async () => {
    confirmOnProduction = false;
    statuses = [status('running', '500'), status('stopped', '500')];
    const wrapper = mount(RedisBulkTtlDialog, {
      props: { sessionId: 's1', filter: { pattern: '', keyType: 'hash' }, connectionName: 'Cache' },
      attachTo: document.body,
    });
    await flushPromises();
    const persist = [...document.body.querySelectorAll<HTMLInputElement>('input[type="radio"]')].find((r) => r.value === 'persist');
    if (!persist) throw new Error('no persist choice');
    persist.checked = true;
    persist.dispatchEvent(new Event('change'));
    await flushPromises();
    button('Remove TTL')?.click();
    await flushPromises();
    expect(starts).toHaveLength(1);
    expect(starts[0]?.request.action).toEqual({ kind: 'persist' });
    expect(asked).toHaveLength(0);
    button('Stop')?.click();
    await flushPromises();
    expect(stops).toEqual(['job-1']);
    // Closing leaves the job running on the server.
    expect(button('Close (keeps running)')).toBeDefined();
    await wait(350);
    await flushPromises();
    await wait(350);
    await flushPromises();
    expect(document.body.textContent).toContain('Stopped: 500 keys were changed before the stop');
    wrapper.unmount();
  });
});

describe('RedisBulkTtlDialog when progress cannot be read', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('retries with backoff, then lets the user close, stop and check again', async () => {
    vi.useFakeTimers();
    confirmOnProduction = false;
    statusFails = true;
    stopFails = true;
    const wrapper = mount(RedisBulkTtlDialog, {
      props: { sessionId: 's1', filter: { pattern: 'user:*', keyType: null }, connectionName: 'Cache' },
      attachTo: document.body,
    });
    await flushPromises();
    const seconds = document.body.querySelector<HTMLInputElement>('input[aria-label="Seconds"]');
    if (!seconds) throw new Error('no seconds field');
    seconds.value = '60';
    seconds.dispatchEvent(new Event('input'));
    await flushPromises();
    button('Set TTL')?.click();
    await flushPromises();
    await vi.advanceTimersByTimeAsync(400);
    expect(document.body.textContent).toContain('Could not read the progress, trying again');
    await vi.advanceTimersByTimeAsync(20_000);
    expect(document.body.textContent).toContain('Lost track of the job');
    // Stop fails too, and does not stay "Stopping…".
    button('Stop')?.click();
    await flushPromises();
    expect(button('Stop')).toBeDefined();
    // Check again finds the job's real state.
    jobs = [{ ...status('stopped', '400'), jobId: 'job-1' }];
    button('Check again')?.click();
    await flushPromises();
    expect(document.body.textContent).toContain('Stopped: 400 keys');
    button('Close')?.click();
    expect(wrapper.emitted('close')?.[0]).toEqual([true]);
    wrapper.unmount();
  });

  it('follows a job that still runs when the dialog opens again, and warns on a cluster', async () => {
    cluster = true;
    jobs = [status('running', '300')];
    statuses = [status('complete', '1200')];
    const wrapper = mount(RedisBulkTtlDialog, {
      props: { sessionId: 's1', filter: { pattern: 'user:*', keyType: null }, connectionName: 'Cache' },
      attachTo: document.body,
    });
    await flushPromises();
    expect(document.body.textContent).toContain('300 of 1,200 keys changed so far');
    expect(document.body.textContent).toContain('cluster mode');
    expect(button('Stop')).toBeDefined();
    await wait(350);
    await flushPromises();
    expect(document.body.textContent).toContain('Done: 1,200 keys changed');
    expect(starts).toHaveLength(0);
    wrapper.unmount();
  });
});

describe('RedisBulkTtlDialog while a job runs', () => {
  it('closes and leaves the job running', async () => {
    confirmOnProduction = false;
    statuses = [status('running', '500'), status('running', '600')];
    const wrapper = mount(RedisBulkTtlDialog, {
      props: { sessionId: 's1', filter: { pattern: 'user:*', keyType: null }, connectionName: 'Cache' },
      attachTo: document.body,
    });
    await flushPromises();
    const persist = [...document.body.querySelectorAll<HTMLInputElement>('input[type="radio"]')].find((r) => r.value === 'persist');
    if (!persist) throw new Error('no persist choice');
    persist.checked = true;
    persist.dispatchEvent(new Event('change'));
    await flushPromises();
    button('Remove TTL')?.click();
    await flushPromises();
    await wait(350);
    await flushPromises();
    button('Close (keeps running)')?.click();
    expect(wrapper.emitted('close')?.[0]).toEqual([true]);
    expect(stops).toEqual([]);
    wrapper.unmount();
  });
});
