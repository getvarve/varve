import { CATEGORIES } from '../api/activity';
import type { ActivityEvent } from '../api/activity';
import {
  actionFilterTitle,
  CATEGORY_ICONS,
  categoryFilterTitle,
  categoryTone,
  connectionFilterTitle,
  DEFAULT_SORT,
  effectiveSort,
  groupEvents,
  nextSort,
  outcomeFilterTitle,
  sortTitle,
} from './activityColumns';

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
const connectionLabel = (e: ActivityEvent): string => (e.connectionId === null ? '' : e.connectionId);
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
    expect(nextSort(DEFAULT_SORT, 'time')).toEqual({ key: 'time', direction: 'ascending' });
    expect(nextSort({ key: 'time', direction: 'ascending' }, 'time')).toEqual(DEFAULT_SORT);
  });

  it('starts another column in its most useful direction', () => {
    expect(nextSort(DEFAULT_SORT, 'action')).toEqual({ key: 'action', direction: 'ascending' });
    expect(nextSort(DEFAULT_SORT, 'outcome')).toEqual({ key: 'outcome', direction: 'ascending' });
    expect(nextSort(DEFAULT_SORT, 'duration')).toEqual({ key: 'duration', direction: 'descending' });
  });
});

describe('effectiveSort', () => {
  it('keeps newest first while more pages exist', () => {
    expect(effectiveSort({ key: 'outcome', direction: 'ascending' }, false)).toEqual(DEFAULT_SORT);
    expect(effectiveSort({ key: 'time', direction: 'ascending' }, false)).toEqual(DEFAULT_SORT);
  });

  it('applies the chosen sort once every event is loaded', () => {
    expect(effectiveSort({ key: 'outcome', direction: 'ascending' }, true)).toEqual({ key: 'outcome', direction: 'ascending' });
  });
});

describe('sortTitle', () => {
  it('describes the current order and what a click does', () => {
    expect(sortTitle('time', DEFAULT_SORT, true)).toBe('Sorted by time, newest first. Click to reverse.');
    expect(sortTitle('outcome', { key: 'outcome', direction: 'ascending' }, true)).toBe(
      'Sorted by outcome within each day, failed first. Click to reverse.',
    );
    expect(sortTitle('duration', DEFAULT_SORT, true)).toBe('Sort by how long it took within each day, slowest first');
  });

  it('says why sorting waits for every event', () => {
    expect(sortTitle('time', DEFAULT_SORT, false)).toBe(
      'Sorted by time, newest first. Reversing needs every matching event loaded. Load more, or narrow the filters.',
    );
    expect(sortTitle('action', DEFAULT_SORT, false)).toBe(
      'Sorting by action needs every matching event loaded. Load more, or narrow the filters.',
    );
  });
});

describe('groupEvents', () => {
  // Newest first, as the backend pages them: two events on day 2, two on day 1.
  const events = [
    event('d2-late', 2 * DAY + 5000, { action: 'query.failed', outcome: 'failure', durationMs: 4, connectionId: 'b' }),
    event('d2-early', 2 * DAY + 1000, { action: 'connection.opened', durationMs: 40, connectionId: 'a' }),
    event('d1-late', DAY + 5000, { action: 'app.started', durationMs: null }),
    event('d1-early', DAY + 1000, { action: 'app.started', outcome: 'cancelled', durationMs: 900, target: 'A' }),
  ];

  it('groups newest first by default', () => {
    const groups = groupEvents(events, DEFAULT_SORT, dayOf, connectionLabel);
    expect(groups.map((g) => g.day)).toEqual(['day 2', 'day 1']);
    expect(ids(groups)).toEqual([
      ['d2-late', 'd2-early'],
      ['d1-late', 'd1-early'],
    ]);
  });

  it('reverses days and events for oldest first', () => {
    const groups = groupEvents(events, { key: 'time', direction: 'ascending' }, dayOf, connectionLabel);
    expect(ids(groups)).toEqual([
      ['d1-early', 'd1-late'],
      ['d2-early', 'd2-late'],
    ]);
  });

  it('sorts within each day and keeps the days in time order', () => {
    expect(ids(groupEvents(events, { key: 'action', direction: 'ascending' }, dayOf, connectionLabel))).toEqual([
      ['d2-early', 'd2-late'],
      // Equal actions: by target, the empty one first.
      ['d1-late', 'd1-early'],
    ]);
    expect(ids(groupEvents(events, { key: 'outcome', direction: 'ascending' }, dayOf, connectionLabel))).toEqual([
      ['d2-late', 'd2-early'],
      ['d1-early', 'd1-late'],
    ]);
  });

  it('puts events without a value last in both directions', () => {
    expect(ids(groupEvents(events, { key: 'duration', direction: 'descending' }, dayOf, connectionLabel))[1]).toEqual([
      'd1-early',
      'd1-late',
    ]);
    expect(ids(groupEvents(events, { key: 'duration', direction: 'ascending' }, dayOf, connectionLabel))[1]).toEqual([
      'd1-early',
      'd1-late',
    ]);
    expect(ids(groupEvents(events, { key: 'connection', direction: 'descending' }, dayOf, connectionLabel))[0]).toEqual([
      'd2-late',
      'd2-early',
    ]);
  });

  it('does not reorder its input', () => {
    groupEvents(events, { key: 'time', direction: 'ascending' }, dayOf, connectionLabel);
    groupEvents(events, { key: 'duration', direction: 'ascending' }, dayOf, connectionLabel);
    expect(events.map((e) => e.id)).toEqual(['d2-late', 'd2-early', 'd1-late', 'd1-early']);
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
