import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { Ref } from 'vue';
import { useTabVisible } from './useTabs';

/**
 * `Date.now()`, updated every `ms` while the component's tab and the window
 * are on screen (for "5 s ago" labels and message rates). The timer stops
 * while either is hidden: a tab's view stays mounted with `display: none`,
 * and nobody sees the labels then. Shown again, it updates right away.
 */
export function useVisibleClock(ms = 1000): Readonly<Ref<number>> {
  const now = ref(Date.now());
  const tabVisible = useTabVisible();
  let timer: ReturnType<typeof setInterval> | null = null;
  let mounted = false;

  const visible = (): boolean => tabVisible.value && (typeof document === 'undefined' || document.visibilityState !== 'hidden');

  function sync(): void {
    const run = mounted && visible();
    if (run && timer === null) {
      now.value = Date.now();
      timer = setInterval(() => {
        now.value = Date.now();
      }, ms);
    } else if (!run && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  }

  watch(tabVisible, sync);
  onMounted(() => {
    mounted = true;
    sync();
    if (typeof document !== 'undefined') document.addEventListener('visibilitychange', sync);
  });
  onBeforeUnmount(() => {
    mounted = false;
    sync();
    if (typeof document !== 'undefined') document.removeEventListener('visibilitychange', sync);
  });
  return now;
}
