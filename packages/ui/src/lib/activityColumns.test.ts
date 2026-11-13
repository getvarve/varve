import { CATEGORIES } from '../api/activity';
import type { ActivityEvent } from '../api/activity';
import {
  actionFilterTitle,
  CATEGORY_ICONS,
  categoryFilterTitle,
  categoryTone,
  connectionFilterTitle,
  DEFAULT_SORT,
  eventTime,
  groupEvents,
  hiddenSortTitle,
  nextSort,
  outcomeFilterTitle,
  popoverTab,
  sortHiddenWhenNarrow,
  sortTitle,
} from './activityColumns';
import { formatDateTime, formatTime } from './display';

const DAY = 86_400_000;

function event(id: string, occurredAt: number, over: Partial<ActivityEvent> = {}): ActivityEvent {
  return {
    id,
    occurredAt,
    actorId: 'local',
    deviceId: 'device-1',
    workspaceId: null,
    connectionId: null,
    serverKey: null,
    category: 'query',
    action: 'query.executed',
    outcome: 'success',
    durationMs: null,
    target: null,
    details: {},
    ...over,
  };
}

/** Day buckets of whole days since the epoch, so the tests do not depend on the time zone. */
const dayOf = (ms: number): string => `day ${Math.floor(ms / DAY)}`;
const ids = (groups: { events: ActivityEvent[] }[]): string[][] => groups.map((g) => g.events.map((e) => e.id));

describe('CATEGORY_ICONS', () => {
  it('has an icon for every category', () => {
    for (const category of CATEGORIES) expect(CATEGORY_ICONS[category]).toBeTruthy();
  });
});

describe('categoryTone', () => {
  it('tints failures only', () => {
    expect(categoryTone('failure')).toBe('danger');
    expect(categoryTone('success')).toBe('neutral');
    expect(categoryTone('cancelled')).toBe('neutral');
  });
});

describe('nextSort', () => {
  it('reverses the sorted column', () => {
    expect(nextSort(DEFAULT_SORT, 'time')).toEqual({ column: 'time', direction: 'ascending' });
    expect(nextSort({ column: 'time', direction: 'ascending' }, 'time')).toEqual(DEFAULT_SORT);
    expect(nextSort({ column: 'connection', direction: 'ascending' }, 'connection')).toEqual({
      column: 'connection',
      direction: 'descending',
    });
  });

  it('starts another column in its most useful direction', () => {
    expect(nextSort(DEFAULT_SORT, 'action')).toEqual({ column: 'action', direction: 'ascending' });
    expect(nextSort(DEFAULT_SORT, 'outcome')).toEqual({ column: 'outcome', direction: 'ascending' });
    expect(nextSort(DEFAULT_SORT, 'duration')).toEqual({ column: 'duration', direction: 'descending' });
    expect(nextSort({ column: 'duration', direction: 'ascending' }, 'time')).toEqual(DEFAULT_SORT);
  });
});

describe('sortTitle', () => {
  it('describes the current order and what a click does', () => {
    expect(sortTitle('time', DEFAULT_SORT)).toBe('Sorted by time, newest first.\nClick to reverse.');
    expect(sortTitle('outcome', { column: 'outcome', direction: 'ascending' })).toBe(
      'Sorted by outcome, failed first.\nClick to reverse.',
    );
  });

  it('says where events without a value go', () => {
    expect(sortTitle('duration', { column: 'duration', direction: 'ascending' })).toBe(
      'Sorted by how long it took, fastest first. Events without a duration come last.\nClick to reverse.',
    );
    expect(sortTitle('connection', { column: 'connection', direction: 'descending' })).toBe(
      'Sorted by connection, Z to A. Events of deleted connections, then events without one, come last.\nClick to reverse.',
    );
  });

  it('offers another column, and says that leaving time order loads the list again', () => {
    expect(sortTitle('duration', DEFAULT_SORT)).toBe('Sort by how long it took, slowest first');
    expect(sortTitle('time', { column: 'action', direction: 'ascending' })).toBe(
      'Sort by time, newest first. The list loads again from the top.',
    );
  });

  it('never asks to load everything first', () => {
    for (const key of ['time', 'action', 'connection', 'outcome', 'duration'] as const) {
      expect(sortTitle(key, DEFAULT_SORT)).not.toMatch(/load more|every matching event/i);
    }
  });
});

describe('groupEvents', () => {
  // As the backend pages them newest first: two events on day 2, two on day 1.
  const events = [
    event('d2-late', 2 * DAY + 5000),
    event('d2-early', 2 * DAY + 1000),
    event('d1-late', DAY + 5000),
    event('d1-early', DAY + 1000),
  ];

  it('puts events in time order under their day', () => {
    const groups = groupEvents(events, DEFAULT_SORT, dayOf);
    expect(groups.map((g) => g.day)).toEqual(['day 2', 'day 1']);
    expect(ids(groups)).toEqual([
      ['d2-late', 'd2-early'],
      ['d1-late', 'd1-early'],
    ]);
  });

  it('keeps the order the backend sent, also oldest first', () => {
    const oldest = [...events].reverse();
    const groups = groupEvents(oldest, { column: 'time', direction: 'ascending' }, dayOf);
    expect(groups.map((g) => g.day)).toEqual(['day 1', 'day 2']);
    expect(ids(groups)).toEqual([
      ['d1-early', 'd1-late'],
      ['d2-early', 'd2-late'],
    ]);
  });

  it('has no day headings in another order and does not reorder', () => {
    // Sorted by duration on the backend: days alternate.
    const byDuration = [1, 3, 0, 2].flatMap((i) => events[i] ?? []);
    const groups = groupEvents(byDuration, { column: 'duration', direction: 'descending' }, dayOf);
    expect(groups).toHaveLength(1);
    expect(groups[0]?.day).toBeNull();
    expect(ids(groups)).toEqual([['d2-early', 'd1-early', 'd2-late', 'd1-late']]);
  });

  it('has no groups without events', () => {
    expect(groupEvents([], DEFAULT_SORT, dayOf)).toEqual([]);
    expect(groupEvents([], { column: 'action', direction: 'ascending' }, dayOf)).toEqual([]);
  });

  it('does not change its input', () => {
    groupEvents(events, { column: 'time', direction: 'ascending' }, dayOf);
    groupEvents(events, { column: 'duration', direction: 'ascending' }, dayOf);
    expect(events.map((e) => e.id)).toEqual(['d2-late', 'd2-early', 'd1-late', 'd1-early']);
  });
});

describe('eventTime', () => {
  const ms = 2 * DAY + 3_723_000;

  it('shows the time alone under day headings', () => {
    expect(eventTime(ms, DEFAULT_SORT)).toEqual({ date: null, time: formatTime(ms) });
    expect(eventTime(ms, { column: 'time', direction: 'ascending' }).date).toBeNull();
  });

  it('adds the date in every other order', () => {
    for (const column of ['action', 'connection', 'outcome', 'duration'] as const) {
      expect(eventTime(ms, { column, direction: 'ascending' })).toEqual({
        date: formatDateTime(ms).slice(0, 10),
        time: formatTime(ms),
      });
    }
    expect(eventTime(ms, { column: 'action', direction: 'ascending' }).date).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  });
});

describe('filter titles', () => {
  it('say what is filtered', () => {
    expect(outcomeFilterTitle('failure')).toBe('Outcome: Failed only. Click to change.');
    expect(outcomeFilterTitle('')).toBe('Outcome: any. Click to filter.');
    expect(connectionFilterTitle('Orders primary')).toBe('Connection: Orders primary only. Click to change.');
    expect(connectionFilterTitle(null)).toBe('Connection: all. Click to filter.');
    expect(actionFilterTitle([])).toBe('Action: any. Click to filter.');
    expect(actionFilterTitle(['query.failed'])).toBe('Action: query.failed only. Click to change.');
    expect(actionFilterTitle(['query.failed', 'connection.*', 'app.started'])).toBe(
      'Action: query.failed, connection.* and app.started. Click to change.',
    );
  });

  it('count long category selections', () => {
    expect(categoryFilterTitle([], 17)).toBe('Category: all 17 shown. Click to filter.');
    expect(categoryFilterTitle(['query'], 17)).toBe('Category: query only. Click to change.');
    expect(categoryFilterTitle(['query', 'tool'], 17)).toBe('Category: query and tool. Click to change.');
    expect(categoryFilterTitle(['query', 'tool', 'sync', 'app'], 17)).toBe('Category: 4 of 17. Click to change.');
  });
});

describe('a sort on a column the narrow list hides', () => {
  it('is known for connection and duration only', () => {
    expect(sortHiddenWhenNarrow({ column: 'connection', direction: 'ascending' })).toBe(true);
    expect(sortHiddenWhenNarrow({ column: 'duration', direction: 'descending' })).toBe(true);
    expect(sortHiddenWhenNarrow(DEFAULT_SORT)).toBe(false);
    expect(sortHiddenWhenNarrow({ column: 'action', direction: 'ascending' })).toBe(false);
    expect(sortHiddenWhenNarrow({ column: 'outcome', direction: 'ascending' })).toBe(false);
  });

  it('says what the list is sorted by and how to get the usual order back', () => {
    expect(hiddenSortTitle({ column: 'duration', direction: 'descending' })).toBe(
      'Sorted by how long it took, slowest first. Events without a duration come last. That column is hidden while the list is this narrow.\nClick to sort by time, newest first.',
    );
    expect(hiddenSortTitle({ column: 'connection', direction: 'ascending' })).toContain('Sorted by connection, A to Z.');
  });
});

describe('popoverTab', () => {
  it('lets the browser move focus between the stops', () => {
    expect(popoverTab(3, 0, false)).toBe('browser');
    expect(popoverTab(3, 1, true)).toBe('browser');
    // From the popover itself (after a click on its padding) Tab goes to the first stop.
    expect(popoverTab(3, -1, false)).toBe('browser');
  });

  it('closes past either end', () => {
    expect(popoverTab(3, 2, false)).toBe('close');
    expect(popoverTab(3, 0, true)).toBe('close');
    expect(popoverTab(3, -1, true)).toBe('close');
    expect(popoverTab(1, 0, false)).toBe('close');
  });

  it('brings lost focus back into the popover instead of walking the page behind it', () => {
    expect(popoverTab(3, null, false)).toEqual({ focus: 0 });
    expect(popoverTab(3, null, true)).toEqual({ focus: 2 });
    expect(popoverTab(0, null, false)).toBe('close');
  });
});
