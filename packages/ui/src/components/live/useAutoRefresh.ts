import { onBeforeUnmount, onMounted, readonly, ref, watch } from 'vue';
import type { Ref } from 'vue';
import { useTabVisible } from '../../composables/useTabs';
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
 * a time. Nothing is fetched while the window or the panel's tab is hidden
 * (the tab's view stays mounted with `display: none`); when both are shown
 * again after a skipped tick (or a first load that waited), it loads once
 * right away. `onError` receives failed fetches (the panel shows them); the
 * loop keeps going. `sessionId` is the session `fetch` reads from: the panel
 * stays mounted across a reconnect, so when the id changes it loads once,
 * also with auto refresh off or paused (the rows on screen are the old
 * session's). `fetch` is given the session id to send its calls on, read
 * when the fetch starts, and `onError` gets that same id, so a failure that
 * arrives after a reconnect is reported for the session it was sent on.
 */
export function useAutoRefresh(
  fetch: (sessionId: string) => Promise<void>,
  intervalMs: number | null,
  onError: (error: unknown, sessionId: string) => void,
  sessionId: () => string,
): UseAutoRefresh {
  const tabVisible = useTabVisible();
  // One fetch runs at a time, so this is the session of the fetch that fails.
  let sentOn = sessionId();
  const state = ref<RefreshState>({ intervalMs, paused: false, loading: false, lastAt: null, failed: false });
  const loop = new AutoRefresh({
    fetch: () => {
      sentOn = sessionId();
      return fetch(sentOn);
    },
    intervalMs,
    onChange: (s) => {
      state.value = s;
    },
    onError: (error) => onError(error, sentOn),
    visible: () => tabVisible.value && (typeof document === 'undefined' || document.visibilityState !== 'hidden'),
  });
  const wake = (): void => {
    void loop.wake();
  };
  watch(tabVisible, (shown) => {
    if (shown) wake();
  });
  watch(sessionId, () => {
    void loop.reload();
  });
  onMounted(() => {
    void loop.start();
    if (typeof document !== 'undefined') document.addEventListener('visibilitychange', wake);
  });
  onBeforeUnmount(() => {
    loop.dispose();
    if (typeof document !== 'undefined') document.removeEventListener('visibilitychange', wake);
  });
  return {
    state: readonly(state) as Readonly<Ref<RefreshState>>,
    setInterval: (ms) => loop.setInterval(ms),
    pause: () => loop.pause(),
    resume: () => loop.resume(),
    refreshNow: () => loop.refreshNow(),
    alive: () => !loop.isDisposed(),
  };
}
