import { markRaw, reactive, watch } from 'vue';
import type { ObjectKind } from '../../api/catalog';
import { useTabs } from '../../composables/useTabs';
import { DEFAULT_SORT } from './overview';
import type { SortState } from './overview';
import { createOverviewLoader } from './overviewLoader';
import type { OverviewLoader } from './overviewLoader';

/**
 * Everything one overview tab shows besides the tab itself: its loaded
 * objects and what the user did with them. It lives as long as the tab, so
 * switching away and back keeps search, filters, sort, the active row and
 * the scroll position, and loads nothing again. Each tab has its own.
 */
export interface OverviewView {
  loader: OverviewLoader;
  search: string;
  kinds: Set<ObjectKind>;
  sort: SortState;
  activeKey: string | null;
  scrollTop: number;
  /** The session the loaded data was read on; a new session (reconnect) reads everything again. */
  sessionId: string | null;
}

const views = new Map<string, OverviewView>();

/** The view state of an overview tab, created on first use. */
export function overviewView(tabId: string): OverviewView {
  let view = views.get(tabId);
  if (!view) {
    view = reactive({
      loader: markRaw(createOverviewLoader(tabId)),
      search: '',
      kinds: new Set<ObjectKind>(),
      sort: { ...DEFAULT_SORT },
      activeKey: null,
      scrollTop: 0,
      sessionId: null,
    }) as OverviewView;
    views.set(tabId, view);
  }
  return view;
}

/** True while a tab has view state (for tests and cleanup). */
export function hasOverviewView(tabId: string): boolean {
  return views.has(tabId);
}

// A closed tab's state goes with it.
watch(
  () => useTabs().tabs.map((t) => t.id),
  (ids) => {
    const open = new Set(ids);
    for (const id of [...views.keys()]) if (!open.has(id)) views.delete(id);
  },
);
