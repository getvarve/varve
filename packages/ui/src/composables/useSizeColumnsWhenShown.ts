import { onBeforeUnmount, watch } from 'vue';
import type { WatchSource } from 'vue';
import { useTabVisible } from './useTabs';

/** What `useSizeColumnsWhenShown` needs of an AG Grid API. */
export interface SizableGrid {
  isDestroyed: () => boolean;
  autoSizeAllColumns: (skipHeader?: boolean) => void;
}

/**
 * For a grid with `autoSizeStrategy: fitCellContents` inside a tab's view.
 * While the tab is hidden the grid is `display: none` and measures nothing,
 * so rows or columns that arrive then (`sources` change, or the grid is
 * created hidden) leave the columns at their fallback widths. When the tab is
 * shown again the grid sizes its columns to their content once, after the
 * browser has laid it out and the grid has rendered its cells.
 */
export function useSizeColumnsWhenShown(grid: () => SizableGrid | null, sources: WatchSource[]): void {
  const tabVisible = useTabVisible();
  let pending = !tabVisible.value;
  let frame: number | null = null;

  watch(sources, () => {
    if (!tabVisible.value) pending = true;
  });

  watch(
    tabVisible,
    (shown) => {
      if (!shown || !pending) return;
      pending = false;
      cancel();
      // Two frames: one for the layout, one for the grid's own render on its new size.
      frame = requestAnimationFrame(() => {
        frame = requestAnimationFrame(() => {
          frame = null;
          // Hidden again before the grid was laid out: it would measure nothing. Size it the next time it is shown.
          if (!tabVisible.value) {
            pending = true;
            return;
          }
          const api = grid();
          if (api && !api.isDestroyed()) api.autoSizeAllColumns(false);
        });
      });
    },
    { flush: 'post' },
  );

  function cancel(): void {
    if (frame !== null) cancelAnimationFrame(frame);
    frame = null;
  }

  onBeforeUnmount(cancel);
}

/** What `useKeepGridScroll` needs of an AG Grid API. */
export interface ScrollKeptGrid {
  isDestroyed: () => boolean;
  getDisplayedRowCount: () => number;
  getDisplayedRowAtIndex: (index: number) => { rowTop: number | null } | undefined;
  ensureIndexVisible: (index: number, position: 'top') => void;
}

/** The element of an AG Grid that scrolls its rows and columns (both directions). */
const GRID_VIEWPORT = '.ag-grid-viewport';

/** The displayed row that lies at `top` pixels from the start of the rows; `null` for a grid without rows. */
function rowAtPixel(api: ScrollKeptGrid, top: number): number | null {
  const count = api.getDisplayedRowCount();
  if (count === 0) return null;
  let low = 0;
  let high = count - 1;
  while (low < high) {
    const mid = Math.ceil((low + high) / 2);
    const rowTop = api.getDisplayedRowAtIndex(mid)?.rowTop ?? null;
    if (rowTop !== null && rowTop <= top) low = mid;
    else high = mid - 1;
  }
  return low;
}

/**
 * Keeps the scroll position of a grid inside a tab's view. While another
 * page (Activity, Settings) is shown the whole workspace is taken out of the
 * document, and a tab that is not the active one is `display: none`; either
 * way the grid's scrolling element can come back at the top left, with the
 * grid still holding the rows of the old position. The position is
 * remembered while the tab is shown (pass `noteScroll` the `top` and `left`
 * of the grid's `bodyScroll` events) and, when the tab is shown again and
 * the element lost it, put back: the grid scrolls to the row that was at
 * the top and draws its rows there, then the exact offsets are set, which
 * the grid follows like any scroll.
 *
 * `root` is an element around the grid. Call this after
 * `useSizeColumnsWhenShown`, so the columns have their widths first.
 */
export function useKeepGridScroll(grid: () => ScrollKeptGrid | null, root: () => HTMLElement | null): { noteScroll: (top: number, left: number) => void } {
  const tabVisible = useTabVisible();
  let saved: { top: number; left: number } | null = null;
  /** True from the moment the tab is shown until the position is back: what the grid reports meanwhile is not where the user scrolled to. */
  let settling = false;
  let frame: number | null = null;

  function noteScroll(top: number, left: number): void {
    if (tabVisible.value && !settling) saved = { top, left };
  }

  function restore(): void {
    const api = grid();
    const viewport = root()?.querySelector<HTMLElement>(GRID_VIEWPORT) ?? null;
    if (!saved || !api || api.isDestroyed() || !viewport) return;
    if (viewport.scrollTop === saved.top && viewport.scrollLeft === saved.left) return;
    const row = rowAtPixel(api, saved.top);
    if (row !== null) api.ensureIndexVisible(row, 'top');
    viewport.scrollTop = saved.top;
    viewport.scrollLeft = saved.left;
  }

  watch(
    tabVisible,
    (shown) => {
      if (!shown) return;
      settling = true;
      cancel();
      // Two frames, as for the column sizes: the layout, then the grid's own work on its new size.
      frame = requestAnimationFrame(() => {
        frame = requestAnimationFrame(() => {
          frame = null;
          // Hidden again meanwhile: the position stays remembered for the next time.
          if (!tabVisible.value) return;
          restore();
          settling = false;
        });
      });
    },
    { flush: 'post' },
  );

  function cancel(): void {
    if (frame !== null) cancelAnimationFrame(frame);
    frame = null;
  }

  onBeforeUnmount(cancel);

  return { noteScroll };
}
