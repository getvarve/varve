/** Timer functions, injectable for tests. */
export interface RefreshTimers {
  set: (run: () => void, ms: number) => unknown;
  clear: (handle: unknown) => void;
}

const DEFAULT_TIMERS: RefreshTimers = {
  set: (run, ms) => setTimeout(run, ms),
  clear: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout>),
};

/** What the panel shows about the refresh loop. */
export interface RefreshState {
  /** `null`: auto refresh off. */
  intervalMs: number | null;
  paused: boolean;
  /** A fetch is running. */
  loading: boolean;
  /** Unix ms of the last successful fetch. */
  lastAt: number | null;
  /** The last fetch failed (the loop keeps going). */
  failed: boolean;
}

export interface RefreshConfig {
  /** Loads the panel's data. Rejections are passed to `onError`; the loop keeps running. */
  fetch: () => Promise<void>;
  intervalMs: number | null;
  onChange?: (state: RefreshState) => void;
  onError?: (error: unknown) => void;
  timers?: RefreshTimers;
  /**
   * Fetches are skipped while this returns false (a hidden window or tab);
   * default always visible. Call `wake()` when it may have turned true.
   */
  visible?: () => boolean;
  now?: () => number;
}

/**
 * Auto refresh for a live panel. One fetch at a time: the next tick is
 * scheduled when the previous fetch has finished, so a slow server never
 * gets overlapping requests. Pause stops the ticks but keeps the interval;
 * Refresh now works while paused. A refresh asked for while one runs runs
 * once more right after it (the data must reflect what happened since, for
 * example a kill), never in parallel. While not `visible` nothing is
 * fetched; `wake()` then loads once if a load was skipped meanwhile.
 * `reload()` is for data that no longer belongs to what the panel is
 * connected to (another session): it loads once whatever the interval or
 * pause says.
 */
export class AutoRefresh {
  private readonly config: RefreshConfig;
  private readonly timers: RefreshTimers;
  private handle: unknown = null;
  private inFlight: Promise<void> | null = null;
  private again = false;
  private disposed = false;
  /** A load was skipped while not visible: `wake()` makes up for it. */
  private missed = false;
  /** The skipped load is a `reload()`: `wake()` runs it also while paused. */
  private stale = false;
  private state: RefreshState;

  constructor(config: RefreshConfig) {
    this.config = config;
    this.timers = config.timers ?? DEFAULT_TIMERS;
    this.state = { intervalMs: config.intervalMs, paused: false, loading: false, lastAt: null, failed: false };
  }

  /** The current state (a copy). */
  snapshot(): RefreshState {
    return { ...this.state };
  }

  /** Loads once now and starts the ticks; while not visible, the load waits for `wake()`. */
  start(): Promise<void> {
    if (this.hidden()) {
      this.missed = true;
      this.reschedule();
      return Promise.resolve();
    }
    return this.refreshNow();
  }

  /**
   * Visibility may have changed: when visible again and a load was skipped
   * while hidden (a tick, or the first load), loads once now. Paused, it
   * waits for `resume()`.
   */
  wake(): Promise<void> {
    if (this.disposed || !this.missed || this.hidden()) return Promise.resolve();
    if (this.state.paused && this.state.lastAt !== null && !this.stale) return Promise.resolve();
    return this.refreshNow();
  }

  /**
   * What is on screen was read from something else (the panel's session was
   * replaced): loads once now, also with auto refresh off or paused, since
   * the old data is not a paused view of the new session. The interval and
   * the pause stay as they are. While not visible the load waits for `wake()`.
   */
  reload(): Promise<void> {
    if (this.disposed) return Promise.resolve();
    if (this.hidden()) {
      this.missed = true;
      this.stale = true;
      return Promise.resolve();
    }
    return this.refreshNow();
  }

  /** Changes the interval (`null` turns auto refresh off) and reschedules from now. */
  setInterval(ms: number | null): void {
    this.update({ intervalMs: ms });
    this.reschedule();
  }

  pause(): void {
    this.update({ paused: true });
    this.cancelTimer();
  }

  resume(): void {
    if (!this.state.paused) return;
    this.update({ paused: false });
    if (this.missed && !this.hidden()) {
      void this.refreshNow();
      return;
    }
    this.reschedule();
  }

  /** Fetches now; when a fetch runs, resolves after the extra run that follows it. */
  refreshNow(): Promise<void> {
    if (this.disposed) return Promise.resolve();
    if (this.inFlight) {
      this.again = true;
      return this.inFlight;
    }
    this.cancelTimer();
    this.missed = false;
    this.stale = false;
    this.inFlight = this.loop().finally(() => {
      this.inFlight = null;
      this.reschedule();
    });
    return this.inFlight;
  }

  /** Stops for good: no more ticks, and a running fetch's end schedules nothing. */
  dispose(): void {
    this.disposed = true;
    this.cancelTimer();
  }

  /** True after `dispose`; a fetch that ends later must not write its result. */
  isDisposed(): boolean {
    return this.disposed;
  }

  private async loop(): Promise<void> {
    do {
      this.again = false;
      this.update({ loading: true });
      try {
        await this.config.fetch();
        if (this.disposed) return;
        this.update({ loading: false, failed: false, lastAt: (this.config.now ?? Date.now)() });
      } catch (error: unknown) {
        if (this.disposed) return;
        this.update({ loading: false, failed: true });
        this.config.onError?.(error);
      }
    } while (this.again && !this.disposed);
  }

  private reschedule(): void {
    this.cancelTimer();
    const ms = this.state.intervalMs;
    if (this.disposed || this.state.paused || ms === null || this.inFlight) return;
    this.handle = this.timers.set(() => this.tick(), ms);
  }

  private tick(): void {
    this.handle = null;
    if (this.disposed || this.state.paused) return;
    if (this.hidden()) {
      this.missed = true;
      this.reschedule();
      return;
    }
    void this.refreshNow();
  }

  private hidden(): boolean {
    return this.config.visible !== undefined && !this.config.visible();
  }

  private cancelTimer(): void {
    if (this.handle !== null) this.timers.clear(this.handle);
    this.handle = null;
  }

  private update(patch: Partial<RefreshState>): void {
    this.state = { ...this.state, ...patch };
    this.config.onChange?.(this.snapshot());
  }
}

/** Interval choices of the live panels, in milliseconds (`null`: off). */
export const REFRESH_CHOICES: readonly { label: string; ms: number | null }[] = [
  { label: 'Off', ms: null },
  { label: '2 s', ms: 2000 },
  { label: '5 s', ms: 5000 },
  { label: '10 s', ms: 10_000 },
  { label: '30 s', ms: 30_000 },
];
