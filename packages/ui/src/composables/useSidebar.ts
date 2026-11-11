import { reactive, readonly } from 'vue';
import { readStored, writeStored } from '../lib/storage';

/** The sidebar's width range, the same as in the other apps of the family. */
export const SIDEBAR_MIN = 190;
export const SIDEBAR_IDEAL = 220;
export const SIDEBAR_MAX = 300;
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
    /** Reads the stored state again (tests). */
    reload(): void {
      Object.assign(state, load());
    },
  };
}
