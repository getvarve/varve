import { onBeforeUnmount, onMounted, readonly, ref } from 'vue';
import type { Ref } from 'vue';
import { AutoRefresh } from '../../lib/liveRefresh';
import type { RefreshState } from '../../lib/liveRefresh';

/** A live panel's refresh loop, started on mount and stopped on unmount. */
export interface UseAutoRefresh {
  state: Readonly<Ref<RefreshState>>;
  setInterval: (ms: number | null) => void;
  pause: () => void;
  resume: () => void;
  refreshNow: () => Promise<void>;
  /** False once the panel is gone: results that arrive later must be dropped. */
  alive: () => boolean;
}

/**
 * Runs `fetch` on mount and then every `intervalMs` while not paused, one at
 * a time. Ticks are skipped while the window is hidden. `onError` receives
 * failed fetches (the panel shows them); the loop keeps going.
 */
export function useAutoRefresh(fetch: () => Promise<void>, intervalMs: number | null, onError: (error: unknown) => void): UseAutoRefresh {
  const state = ref<RefreshState>({ intervalMs, paused: false, loading: false, lastAt: null, failed: false });
  const loop = new AutoRefresh({
    fetch,
    intervalMs,
    onChange: (s) => {
      state.value = s;
    },
    onError,
    visible: () => typeof document === 'undefined' || document.visibilityState !== 'hidden',
  });
  onMounted(() => {
    void loop.start();
  });
  onBeforeUnmount(() => loop.dispose());
  return {
    state: readonly(state) as Readonly<Ref<RefreshState>>,
    setInterval: (ms) => loop.setInterval(ms),
    pause: () => loop.pause(),
    resume: () => loop.resume(),
    refreshNow: () => loop.refreshNow(),
    alive: () => !loop.isDisposed(),
  };
}
