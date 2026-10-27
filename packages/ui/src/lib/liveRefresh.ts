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
  /** Ticks while this returns false are skipped (a hidden window); default always visible. */
  visible?: () => boolean;
  now?: () => number;
}

/**
 * Auto refresh for a live panel. One fetch at a time: the next tick is
 * scheduled when the previous fetch has finished, so a slow server never
 * gets overlapping requests. Pause stops the ticks but keeps the interval;
 * Refresh now works while paused. A refresh asked for while one runs runs
 * once more right after it (the data must reflect what happened since, for
 * example a kill), never in parallel.
 */
export class AutoRefresh {
  private readonly config: RefreshConfig;
  private readonly timers: RefreshTimers;
  private handle: unknown = null;
  private inFlight: Promise<void> | null = null;
  private again = false;
  private disposed = false;
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

  /** Loads once now and starts the ticks. */
  start(): Promise<void> {
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
    if (this.config.visible && !this.config.visible()) {
      this.reschedule();
      return;
    }
    void this.refreshNow();
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
