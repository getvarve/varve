import { describe, expect, it, vi } from 'vitest';
import { AutoRefresh } from './liveRefresh';
import type { RefreshState, RefreshTimers } from './liveRefresh';

/** Manual timers: `fire()` runs the pending tick. */
function fakeTimers(): RefreshTimers & { pending: () => number[]; fire: () => void } {
  let next = 1;
  const handles = new Map<number, { run: () => void; ms: number }>();
  return {
    set: (run, ms) => {
      const id = next;
      next += 1;
      handles.set(id, { run, ms });
      return id;
    },
    clear: (handle) => {
      handles.delete(handle as number);
    },
    pending: () => [...handles.values()].map((h) => h.ms),
    fire: () => {
      const [id, h] = [...handles.entries()][0] ?? [];
      if (id === undefined || !h) throw new Error('no timer pending');
      handles.delete(id);
      h.run();
    },
  };
}

/** A fetch whose calls resolve only when the test says so. */
function controlledFetch() {
  const calls: { resolve: () => void; reject: (e: unknown) => void }[] = [];
  const fetch = vi.fn(
    () =>
      new Promise<void>((resolve, reject) => {
        calls.push({ resolve, reject });
      }),
  );
  return { fetch, calls };
}

const flush = () => new Promise<void>((r) => setTimeout(r, 0));

describe('AutoRefresh', () => {
  it('loads on start and schedules the next tick only after the fetch finished', async () => {
    const timers = fakeTimers();
    const { fetch, calls } = controlledFetch();
    const loop = new AutoRefresh({ fetch, intervalMs: 5000, timers, now: () => 42 });
    const started = loop.start();
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(loop.snapshot().loading).toBe(true);
    expect(timers.pending()).toEqual([]);
    calls[0]?.resolve();
    await started;
    expect(loop.snapshot()).toMatchObject({ loading: false, lastAt: 42, failed: false });
    expect(timers.pending()).toEqual([5000]);
    timers.fire();
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(timers.pending()).toEqual([]);
  });

  it('never overlaps: a refresh asked during a fetch runs once right after it', async () => {
    const timers = fakeTimers();
    const { fetch, calls } = controlledFetch();
    const loop = new AutoRefresh({ fetch, intervalMs: null, timers });
    const first = loop.start();
    const second = loop.refreshNow();
    const third = loop.refreshNow();
    expect(fetch).toHaveBeenCalledTimes(1);
    calls[0]?.resolve();
    await flush();
    expect(fetch).toHaveBeenCalledTimes(2);
    calls[1]?.resolve();
    await Promise.all([first, second, third]);
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it('pause stops ticks, refresh now still works while paused, resume restarts them', async () => {
    const timers = fakeTimers();
    const fetch = vi.fn(async () => undefined);
    const loop = new AutoRefresh({ fetch, intervalMs: 2000, timers });
    await loop.start();
    expect(timers.pending()).toEqual([2000]);
    loop.pause();
    expect(timers.pending()).toEqual([]);
    expect(loop.snapshot().paused).toBe(true);
    await loop.refreshNow();
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(timers.pending()).toEqual([]);
    loop.resume();
    expect(timers.pending()).toEqual([2000]);
  });

  it('changing the interval reschedules; off schedules nothing', async () => {
    const timers = fakeTimers();
    const loop = new AutoRefresh({ fetch: async () => undefined, intervalMs: 5000, timers });
    await loop.start();
    loop.setInterval(10_000);
    expect(timers.pending()).toEqual([10_000]);
    loop.setInterval(null);
    expect(timers.pending()).toEqual([]);
    expect(loop.snapshot().intervalMs).toBeNull();
  });

  it('reports a failed fetch and keeps refreshing', async () => {
    const timers = fakeTimers();
    const onError = vi.fn();
    let fail = true;
    const loop = new AutoRefresh({
      fetch: async () => {
        if (fail) throw new Error('down');
      },
      intervalMs: 1000,
      timers,
      onError,
    });
    await loop.start();
    expect(onError).toHaveBeenCalledWith(new Error('down'));
    expect(loop.snapshot()).toMatchObject({ failed: true, lastAt: null, loading: false });
    expect(timers.pending()).toEqual([1000]);
    fail = false;
    timers.fire();
    await flush();
    expect(loop.snapshot().failed).toBe(false);
  });

  it('skips ticks while hidden without losing the schedule', async () => {
    const timers = fakeTimers();
    const fetch = vi.fn(async () => undefined);
    let visible = false;
    const loop = new AutoRefresh({ fetch, intervalMs: 1000, timers, visible: () => visible });
    await loop.start();
    timers.fire();
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(timers.pending()).toEqual([1000]);
    visible = true;
    timers.fire();
    expect(fetch).toHaveBeenCalledTimes(2);
  });

  it('after dispose a running fetch schedules nothing and reports nothing', async () => {
    const timers = fakeTimers();
    const { fetch, calls } = controlledFetch();
    const states: RefreshState[] = [];
    const onError = vi.fn();
    const loop = new AutoRefresh({ fetch, intervalMs: 1000, timers, onError, onChange: (s) => states.push(s) });
    const started = loop.start();
    loop.dispose();
    const seen = states.length;
    calls[0]?.reject(new Error('late'));
    await started;
    expect(loop.isDisposed()).toBe(true);
    expect(onError).not.toHaveBeenCalled();
    expect(states).toHaveLength(seen);
    expect(timers.pending()).toEqual([]);
    await loop.refreshNow();
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});
