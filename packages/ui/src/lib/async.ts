/**
 * Wraps an async action so calls while one is running are ignored, for
 * example a second Enter in a form while the first save is still running.
 */
export function singleFlight<A extends unknown[]>(action: (...args: A) => Promise<void>): {
  run: (...args: A) => Promise<void>;
  busy: () => boolean;
} {
  let running = false;
  return {
    busy: () => running,
    run: async (...args: A) => {
      if (running) return;
      running = true;
      try {
        await action(...args);
      } finally {
        running = false;
      }
    },
  };
}

/**
 * Hands out request tokens so only the newest of overlapping requests may
 * write its result: a slow answer for a previous key or filter is dropped.
 */
export function latestOnly(): { next: () => number; isCurrent: (token: number) => boolean } {
  let current = 0;
  return {
    next: () => {
      current += 1;
      return current;
    },
    isCurrent: (token: number) => token === current,
  };
}
