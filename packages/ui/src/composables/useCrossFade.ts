import { nextTick, watch, type Ref, type WatchSource } from 'vue';
import { canAnimate, fadeIn, fadeOutAndRemove, inertCopy } from '../lib/motion';

/**
 * Cross-fades `target` whenever `source` changes: an inert copy of the old
 * content fades out on top while the new content fades in, both over
 * `duration` ms (opacity only). The copy is placed absolutely at the old
 * position, so the parent of `target` must be its containing block
 * (`position: relative`).
 */
export function useCrossFade(target: Ref<HTMLElement | null>, source: WatchSource<unknown>, duration: number): void {
  watch(
    source,
    () => {
      // Runs before the DOM update: copy the old content while it is still there.
      const el = target.value;
      if (!el || !el.isConnected || !canAnimate(el)) return;
      const box = { top: el.offsetTop, left: el.offsetLeft, width: el.offsetWidth, height: el.offsetHeight };
      const copy = inertCopy(el);
      void nextTick(() => {
        if (!el.isConnected || target.value !== el) return;
        Object.assign(copy.style, {
          position: 'absolute',
          top: `${box.top}px`,
          left: `${box.left}px`,
          width: `${box.width}px`,
          height: `${box.height}px`,
          margin: '0',
          boxSizing: 'border-box',
        });
        el.before(copy);
        fadeOutAndRemove(copy, duration);
        fadeIn(el, duration);
      });
    },
    { flush: 'pre' },
  );
}
