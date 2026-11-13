import { reactive, readonly, ref } from 'vue';
import { readStored, writeStored } from '../lib/storage';

/** The sidebar's width range. The smallest and the default width are the same as in the other apps of the family. */
export const SIDEBAR_MIN = 190;
export const SIDEBAR_IDEAL = 220;
/** Wider than the family's 300: Varve's sidebar holds a deep tree of long object names. */
export const SIDEBAR_MAX = 480;
/** One arrow key press on the divider. */
export const SIDEBAR_STEP = 10;

const KEY = 'varve.sidebar';

interface Stored {
  collapsed: boolean;
  width: number;
}

function isStored(value: unknown): value is Stored {
  if (typeof value !== 'object' || value === null) return false;
  const v = value as Record<string, unknown>;
  return typeof v.collapsed === 'boolean' && typeof v.width === 'number' && Number.isFinite(v.width);
}

/** A width inside the allowed range, in whole pixels. */
export function clampSidebarWidth(width: number): number {
  if (!Number.isFinite(width)) return SIDEBAR_IDEAL;
  return Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, Math.round(width)));
}

function load(): Stored {
  const stored = readStored<Stored>(KEY, { collapsed: false, width: SIDEBAR_IDEAL }, isStored);
  return { collapsed: stored.collapsed, width: clampSidebarWidth(stored.width) };
}

const state = reactive<Stored>(load());

/** Counts the requests to fit the sidebar to its text (the menu bar asks; the shell, which can measure, does it). */
const fitRequests = ref(0);

function save(): void {
  writeStored(KEY, { collapsed: state.collapsed, width: state.width });
}

/**
 * Whether the sidebar is shown and how wide it is. One state for the window,
 * remembered between runs.
 */
export function useSidebar() {
  return {
    state: readonly(state),
    toggle(): void {
      state.collapsed = !state.collapsed;
      save();
    },
    /** Sets the width; `remember` is false while a drag is still going on. */
    setWidth(width: number, remember = true): void {
      state.width = clampSidebarWidth(width);
      if (remember) save();
    },
    resetWidth(): void {
      state.width = SIDEBAR_IDEAL;
      save();
    },
    /** Changes on every request to fit the sidebar to its text. */
    fitRequests: readonly(fitRequests),
    /** Asks the shell to make the sidebar as wide as its text needs, and shows it first when it is hidden. */
    requestFit(): void {
      if (state.collapsed) {
        state.collapsed = false;
        save();
      }
      fitRequests.value += 1;
    },
    /** Reads the stored state again (tests). */
    reload(): void {
      Object.assign(state, load());
    },
  };
}
