import type { ActivityEvent, ActivityOrder, ActivitySortColumn, Category, Outcome, SortDirection } from '../api/activity';
import type { IconName } from '../components/common/Icon.vue';
import { formatDateTime, formatTime } from './display';

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

// Sorting. The backend sorts over the whole log and pages in that order
// (`ActivityOrder` in crates/activity/src/query.rs); the list only shows what
// it gets, in the order it gets it.

export type ActivitySortKey = ActivitySortColumn;
export type ActivitySort = ActivityOrder;

/** The order the list starts in, and the one the old cursor forms continue. */
export const DEFAULT_SORT: ActivitySort = { column: 'time', direction: 'descending' };

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

/** Where the backend puts the events a column has no value for (the same in both directions). */
const MISSING_LAST: Partial<Record<ActivitySortKey, string>> = {
  connection: 'Events of deleted connections, then events without one, come last.',
  duration: 'Events without a duration come last.',
};

/** The sort after clicking `key`: the same column reverses, another column starts in its first direction. */
export function nextSort(current: ActivitySort, key: ActivitySortKey): ActivitySort {
  if (current.column === key) {
    return { column: key, direction: current.direction === 'ascending' ? 'descending' : 'ascending' };
  }
  return { column: key, direction: FIRST_DIRECTION[key] };
}

/** "Sorted by outcome, failed first." plus where events without a value go. */
function sortedBy(current: ActivitySort): string {
  const missing = MISSING_LAST[current.column];
  const sorted = `Sorted by ${COLUMN_NAMES[current.column]}, ${DIRECTION_WORDS[current.column][current.direction]}.`;
  return missing ? `${sorted} ${missing}` : sorted;
}

/** The tooltip of a sortable column header. A line break separates what is shown from what a click does. */
export function sortTitle(key: ActivitySortKey, current: ActivitySort): string {
  if (current.column === key) return `${sortedBy(current)}\nClick to reverse.`;
  const sort = `Sort by ${COLUMN_NAMES[key]}, ${DIRECTION_WORDS[key][FIRST_DIRECTION[key]]}`;
  return current.column === 'time' ? sort : `${sort}. The list loads again from the top.`;
}

/** The columns a narrow list leaves out (ActivityView's container query): a sort on one of them has no header to show it. */
const NARROW_HIDDEN: readonly ActivitySortKey[] = ['connection', 'duration'];

/** Whether the list is sorted on a column that a narrow list does not show. */
export function sortHiddenWhenNarrow(current: ActivitySort): boolean {
  return NARROW_HIDDEN.includes(current.column);
}

/**
 * The tooltip of the mark a narrow list shows next to the action header
 * while it is sorted on a column it has no room for. Clicking the mark goes
 * back to the default order.
 */
export function hiddenSortTitle(current: ActivitySort): string {
  return `${sortedBy(current)} That column is hidden while the list is this narrow.\nClick to sort by time, newest first.`;
}

export interface DayGroup {
  /** The day heading; null when the list is sorted on another column than time and has no day headings. */
  day: string | null;
  events: ActivityEvent[];
}

/**
 * The events as the list shows them, in the order the backend sent them.
 * Sorted by time they fall into days, each under its heading; in any other
 * order consecutive events can be days apart, so they form one group without
 * a heading (each row shows its date instead, `eventTime`).
 */
export function groupEvents(events: ActivityEvent[], sort: ActivitySort, dayOf: (ms: number) => string): DayGroup[] {
  if (sort.column !== 'time') return events.length === 0 ? [] : [{ day: null, events }];
  const out: DayGroup[] = [];
  for (const event of events) {
    const day = dayOf(event.occurredAt);
    const last = out.at(-1);
    if (last && last.day === day) last.events.push(event);
    else out.push({ day, events: [event] });
  }
  return out;
}

/**
 * An event's time in the list: the time alone under a day heading, the date
 * above it when the list has no day headings (sorted on another column).
 */
export function eventTime(ms: number, sort: ActivitySort): { date: string | null; time: string } {
  return { date: sort.column === 'time' ? null : formatDateTime(ms).slice(0, 10), time: formatTime(ms) };
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
