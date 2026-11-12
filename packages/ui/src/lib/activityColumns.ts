import type { ActivityEvent, Category, Outcome } from '../api/activity';
import type { IconName } from '../components/common/Icon.vue';

/** The icon of each activity category in the list and the details panel. */
export const CATEGORY_ICONS: Record<Category, IconName> = {
  connection: 'plug',
  query: 'terminal',
  transaction: 'brackets',
  data: 'table',
  schema: 'schema',
  tool: 'transfer',
  security: 'shield',
  safety: 'warning',
  secrets: 'key',
  stats: 'chart',
  app: 'monitor',
  account: 'user',
  sync: 'cloud',
  workspace: 'layers',
  membership: 'userPlus',
  access: 'folderLock',
  record: 'file',
};

export const OUTCOME_LABELS: Record<Outcome, string> = {
  success: 'Success',
  failure: 'Failed',
  cancelled: 'Cancelled',
};

/**
 * The tone of an event's category icon: tinted when something went wrong,
 * neutral otherwise, so a failure stands out without colouring every row.
 * (The log has no warning outcome; a cancel is the user's own choice.)
 */
export function categoryTone(outcome: Outcome): 'danger' | 'neutral' {
  return outcome === 'failure' ? 'danger' : 'neutral';
}

// Sorting

export type ActivitySortKey = 'time' | 'action' | 'connection' | 'outcome' | 'duration';
export type SortDirection = 'ascending' | 'descending';
export interface ActivitySort {
  key: ActivitySortKey;
  direction: SortDirection;
}

/** The order the backend pages in (`Page` in crates/activity/src/query.rs only reads newest first). */
export const DEFAULT_SORT: ActivitySort = { key: 'time', direction: 'descending' };

/** The direction a column sorts in when it is first clicked: the order most useful to look at. */
const FIRST_DIRECTION: Record<ActivitySortKey, SortDirection> = {
  time: 'descending',
  action: 'ascending',
  connection: 'ascending',
  outcome: 'ascending',
  duration: 'descending',
};

const COLUMN_NAMES: Record<ActivitySortKey, string> = {
  time: 'time',
  action: 'action',
  connection: 'connection',
  outcome: 'outcome',
  duration: 'how long it took',
};

const DIRECTION_WORDS: Record<ActivitySortKey, Record<SortDirection, string>> = {
  time: { descending: 'newest first', ascending: 'oldest first' },
  action: { ascending: 'A to Z', descending: 'Z to A' },
  connection: { ascending: 'A to Z', descending: 'Z to A' },
  outcome: { ascending: 'failed first', descending: 'successful first' },
  duration: { descending: 'slowest first', ascending: 'fastest first' },
};

/** The sort after clicking `key`: the same column reverses, another column starts in its first direction. */
export function nextSort(current: ActivitySort, key: ActivitySortKey): ActivitySort {
  if (current.key === key) {
    return { key, direction: current.direction === 'ascending' ? 'descending' : 'ascending' };
  }
  return { key, direction: FIRST_DIRECTION[key] };
}

/**
 * The sort the list really shows. The backend only pages newest first, so any
 * other order is only correct once every matching event is loaded; before
 * that, reordering the loaded part would hide older events that belong on top.
 */
export function effectiveSort(requested: ActivitySort, allLoaded: boolean): ActivitySort {
  return allLoaded ? requested : DEFAULT_SORT;
}

/** The tooltip of a sortable column header; `current` is the sort the list shows (`effectiveSort`). */
export function sortTitle(key: ActivitySortKey, current: ActivitySort, allLoaded: boolean): string {
  const name = COLUMN_NAMES[key];
  const withinDays = key === 'time' ? '' : ' within each day';
  const needsAll = 'needs every matching event loaded. Load more, or narrow the filters.';
  if (current.key === key) {
    const now = `Sorted by ${name}${withinDays}, ${DIRECTION_WORDS[key][current.direction]}.`;
    return allLoaded ? `${now} Click to reverse.` : `${now} Reversing ${needsAll}`;
  }
  if (!allLoaded) return `Sorting by ${name} ${needsAll}`;
  return `Sort by ${name}${withinDays}, ${DIRECTION_WORDS[key][FIRST_DIRECTION[key]]}`;
}

/** The columns a narrow list leaves out (ActivityView's container query): a sort on one of them has no header to show it. */
const NARROW_HIDDEN: readonly ActivitySortKey[] = ['connection', 'duration'];

/** Whether the list is sorted on a column that a narrow list does not show. */
export function sortHiddenWhenNarrow(current: ActivitySort): boolean {
  return NARROW_HIDDEN.includes(current.key);
}

/**
 * The tooltip of the mark a narrow list shows next to the action header
 * while it is sorted on a column it has no room for. Clicking the mark goes
 * back to the default order.
 */
export function hiddenSortTitle(current: ActivitySort): string {
  const sorted = `Sorted by ${COLUMN_NAMES[current.key]} within each day, ${DIRECTION_WORDS[current.key][current.direction]}.`;
  return `${sorted} That column is hidden while the list is this narrow. Click to sort by time, newest first.`;
}

/** Problems first when ascending. */
const OUTCOME_RANK: Record<Outcome, number> = { failure: 0, cancelled: 1, success: 2 };

const collator = new Intl.Collator('en', { sensitivity: 'base', numeric: true });

type ColumnKey = Exclude<ActivitySortKey, 'time'>;

/** An event's value in a column; null when it has none (no connection, no duration). */
function columnValue(event: ActivityEvent, key: ColumnKey, connectionLabel: (e: ActivityEvent) => string): string | number | null {
  switch (key) {
    case 'action':
      return event.action;
    case 'connection': {
      const label = connectionLabel(event);
      return label === '' ? null : label;
    }
    case 'outcome':
      return OUTCOME_RANK[event.outcome];
    case 'duration':
      return event.durationMs;
  }
}

function compareValues(a: string | number, b: string | number): number {
  return typeof a === 'number' && typeof b === 'number' ? a - b : collator.compare(String(a), String(b));
}

/**
 * Orders two events on `key`. Events without a value go last in both
 * directions; equal actions are ordered by their target.
 */
function compareOn(
  a: ActivityEvent,
  b: ActivityEvent,
  key: ColumnKey,
  direction: SortDirection,
  connectionLabel: (e: ActivityEvent) => string,
): number {
  const va = columnValue(a, key, connectionLabel);
  const vb = columnValue(b, key, connectionLabel);
  if (va === null || vb === null) return va === vb ? 0 : va === null ? 1 : -1;
  let order = compareValues(va, vb);
  if (order === 0 && key === 'action') order = collator.compare(a.target ?? '', b.target ?? '');
  return direction === 'ascending' ? order : -order;
}

export interface DayGroup {
  day: string;
  events: ActivityEvent[];
}

/**
 * The events grouped by day, in the order of `sort`. `events` come newest
 * first, as the backend pages them. Days keep their time order (reversed
 * for oldest first); another column sorts the events within each day, and
 * events that compare equal keep newest first.
 */
export function groupEvents(
  events: ActivityEvent[],
  sort: ActivitySort,
  dayOf: (ms: number) => string,
  connectionLabel: (e: ActivityEvent) => string,
): DayGroup[] {
  const ordered = sort.key === 'time' && sort.direction === 'ascending' ? [...events].reverse() : events;
  const out: DayGroup[] = [];
  for (const event of ordered) {
    const day = dayOf(event.occurredAt);
    const last = out.at(-1);
    if (last && last.day === day) last.events.push(event);
    else out.push({ day, events: [event] });
  }
  const key = sort.key;
  if (key !== 'time') {
    for (const group of out) group.events.sort((a, b) => compareOn(a, b, key, sort.direction, connectionLabel));
  }
  return out;
}

// Column filters

/** "a", "a and b", "a, b and c". */
function listWords(words: string[]): string {
  if (words.length <= 1) return words.join('');
  return `${words.slice(0, -1).join(', ')} and ${words.at(-1)}`;
}

/** Tooltip of the category filter. Long selections are counted instead of listed. */
export function categoryFilterTitle(selected: readonly Category[], total: number): string {
  if (selected.length === 0) return `Category: all ${total} shown. Click to filter.`;
  if (selected.length === 1) return `Category: ${selected[0]} only. Click to change.`;
  if (selected.length <= 3) return `Category: ${listWords([...selected])}. Click to change.`;
  return `Category: ${selected.length} of ${total}. Click to change.`;
}

/** Tooltip of the action filter, from the valid entries typed. */
export function actionFilterTitle(actions: readonly string[]): string {
  if (actions.length === 0) return 'Action: any. Click to filter.';
  if (actions.length === 1) return `Action: ${actions[0]} only. Click to change.`;
  return `Action: ${listWords([...actions])}. Click to change.`;
}

/** Tooltip of the connection filter; `name` is null when every connection is shown. */
export function connectionFilterTitle(name: string | null): string {
  return name === null ? 'Connection: all. Click to filter.' : `Connection: ${name} only. Click to change.`;
}

/** Tooltip of the outcome filter; `''` is any outcome. */
export function outcomeFilterTitle(outcome: Outcome | ''): string {
  return outcome === '' ? 'Outcome: any. Click to filter.' : `Outcome: ${OUTCOME_LABELS[outcome]} only. Click to change.`;
}

// The filter popover's keyboard

/**
 * What Tab does while a filter popover is open. `stops` is how many Tab
 * stops the popover has; `at` is the focused one's place among them, `-1`
 * when focus is in the popover but on none of them (the popover itself, or
 * a button that just turned itself off), and `null` when focus is outside
 * the popover (lost to the page behind it).
 *
 * Tab past either end closes the popover, as menus do. Lost focus comes
 * back: to the first stop, or the last with Shift. In between the browser
 * moves focus itself.
 */
export function popoverTab(stops: number, at: number | null, backwards: boolean): 'close' | 'browser' | { focus: number } {
  if (at === null) return stops === 0 ? 'close' : { focus: backwards ? stops - 1 : 0 };
  if ((backwards && at <= 0) || (!backwards && at === stops - 1)) return 'close';
  return 'browser';
}
