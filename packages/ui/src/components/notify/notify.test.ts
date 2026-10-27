import { describe, expect, it } from 'vitest';
import type { NotifyBatch, NotifyMessage, NotifyState } from '../../api/notify';
import {
  MAX_VIEW_NOTIFICATIONS,
  byteLength,
  channelProblem,
  countByChannel,
  exportEntries,
  matchesFilter,
  needsQuotesInSql,
  notifySql,
  payloadProblem,
  pushCapped,
  quoteChannel,
  stateMarkers,
  statusText,
} from './notifyLog';
import type { LogEntry } from './notifyLog';
import { applyBatch, applyState, clearPanel, newPanel, rate, setPaused } from './notifyPanel';

function state(over: Partial<NotifyState> = {}): NotifyState {
  return {
    sessionId: 's1',
    listenerId: 1,
    version: 1,
    status: 'connected',
    channels: ['orders'],
    backendPid: 42,
    attempt: 0,
    retryInMs: null,
    error: null,
    dropped: 0,
    gap: false,
    ...over,
  };
}

function message(payload: string, over: Partial<NotifyMessage> = {}): NotifyMessage {
  return { channel: 'orders', payload, pid: 7, own: false, lossy: false, receivedAt: 1_000, ...over };
}

function batch(seq: number, messages: NotifyMessage[], dropped = 0): NotifyBatch {
  return { sessionId: 's1', listenerId: 1, seq, messages, dropped };
}

describe('channel and payload checks', () => {
  it('counts bytes as the server does', () => {
    expect(byteLength('abc')).toBe(3);
    expect(byteLength('é')).toBe(2);
    expect(channelProblem('')).toMatch(/Enter a channel/);
    expect(channelProblem('x'.repeat(63))).toBeNull();
    expect(channelProblem('x'.repeat(64))).toMatch(/64 bytes/);
    expect(channelProblem('é'.repeat(32))).toMatch(/64 bytes/);
    expect(channelProblem('a\0b')).toMatch(/NUL/);
    expect(payloadProblem('')).toBeNull();
    expect(payloadProblem('p'.repeat(7999))).toBeNull();
    expect(payloadProblem('p'.repeat(8000))).toMatch(/8000 bytes/);
  });

  it('tells which names need quotes in SQL', () => {
    expect(needsQuotesInSql('order_events')).toBe(false);
    expect(needsQuotesInSql('Orders')).toBe(true);
    expect(needsQuotesInSql('a b')).toBe(true);
    expect(needsQuotesInSql('1x')).toBe(true);
    expect(quoteChannel('a"b')).toBe('"a""b"');
  });

  it('writes the SQL for a send', () => {
    expect(notifySql('Orders', '')).toBe('NOTIFY "Orders"');
    expect(notifySql("it's", "o'k")).toBe("SELECT pg_notify('it''s', 'o''k')");
  });
});

describe('the log', () => {
  it('keeps the newest entries', () => {
    const list = [1, 2, 3];
    expect(pushCapped(list, [4, 5], 4)).toBe(1);
    expect(list).toEqual([2, 3, 4, 5]);
    expect(pushCapped(list, [], 4)).toBe(0);
  });

  it('filters by channel and text, markers always shown', () => {
    const entry: LogEntry = { ...message('Hello World'), type: 'message', id: 1 };
    const gap: LogEntry = { type: 'gap', id: 2, at: 0, text: 'lost' };
    expect(matchesFilter(entry, 'world', null)).toBe(true);
    expect(matchesFilter(entry, 'ORDERS', null)).toBe(true);
    expect(matchesFilter(entry, 'nope', null)).toBe(false);
    expect(matchesFilter(entry, '', 'other')).toBe(false);
    expect(matchesFilter(entry, '', 'orders')).toBe(true);
    expect(matchesFilter(gap, 'nope', 'other')).toBe(true);
    expect(countByChannel([entry, gap, { ...entry, id: 3 }]).get('orders')).toBe(2);
  });

  it('exports messages only', () => {
    const entries: LogEntry[] = [
      { ...message('a', { receivedAt: 0, own: true }), type: 'message', id: 1 },
      { type: 'gap', id: 2, at: 0, text: 'x' },
    ];
    const parsed = JSON.parse(exportEntries(entries)) as unknown[];
    expect(parsed).toEqual([{ receivedAt: '1970-01-01T00:00:00.000Z', channel: 'orders', payload: 'a', pid: 7, own: true }]);
  });
});

describe('the listener state', () => {
  it('describes each status', () => {
    expect(statusText(null)).toBe('Not listening');
    expect(statusText(state())).toBe('Listening on 1 channel (backend 42)');
    expect(statusText(state({ status: 'reconnecting', attempt: 2, retryInMs: 1000 }))).toBe('Reconnecting (2 failed attempts, next try in 1 s)');
    expect(statusText(state({ status: 'reconnecting', attempt: 0, retryInMs: 500 }))).toBe('Reconnecting (next try in 500 ms)');
    expect(
      statusText(state({ status: 'stopped', error: { kind: 'auth', message: 'bad password', code: null, detail: null, hint: null, position: null } })),
    ).toBe('Stopped: bad password');
  });

  it('marks reconnects from the gap flag and stops with a reason', () => {
    const reconnecting = state({ status: 'reconnecting', attempt: 0 });
    expect(stateMarkers(state(), reconnecting)).toEqual([]);
    expect(stateMarkers(reconnecting, state({ gap: true }))).toEqual([expect.stringMatching(/sent while it was down are lost/)]);
    // Once per listener: a later state of the same listener adds nothing.
    expect(stateMarkers(state({ gap: true }), state({ gap: true }))).toEqual([]);
    // A panel that opens after the reconnect still learns of the gap.
    expect(stateMarkers(null, state({ gap: true }))).toHaveLength(1);
    // A new listener that reconnected is a new gap.
    expect(stateMarkers(state({ gap: true }), state({ gap: true, listenerId: 2 }))).toHaveLength(1);
    const error = { kind: 'connection_lost' as const, message: 'The tab closed.', code: null, detail: null, hint: null, position: null };
    expect(stateMarkers(state(), state({ status: 'stopped', error }))).toEqual(['The listener stopped: The tab closed.']);
    expect(stateMarkers(state(), state({ status: 'stopped' }))).toEqual([]);
    expect(stateMarkers(null, state({ status: 'stopped', error }))).toEqual([]);
  });
});

describe('a panel', () => {
  it('adds batches, counts drops and holds while paused', () => {
    const panel = newPanel('s1');
    applyState(panel, state());
    applyBatch(panel, batch(1, [message('1'), message('2')]), 5_000);
    expect(panel.received).toBe(2);
    expect(panel.entries.map((e) => (e.type === 'message' ? e.payload : e.type))).toEqual(['1', '2']);

    applyBatch(panel, batch(2, [message('3')], 4), 5_000);
    expect(panel.dropped).toBe(4);
    const last = panel.entries[panel.entries.length - 1];
    expect(last?.type === 'gap' && last.text).toBe('4 notifications were dropped because the view fell behind.');

    setPaused(panel, true);
    const version = panel.version;
    applyBatch(panel, batch(3, [message('4')]), 5_000);
    expect(panel.version).toBe(version);
    expect(panel.held).toHaveLength(1);
    setPaused(panel, false);
    expect(panel.held).toHaveLength(0);
    expect(panel.entries[panel.entries.length - 1]).toMatchObject({ payload: '4' });

    clearPanel(panel);
    expect(panel.entries).toHaveLength(0);
    expect(panel.received).toBe(4);
  });

  it('adds a marker when the listener reconnected', () => {
    const panel = newPanel('s1');
    applyState(panel, state({ version: 1 }));
    applyState(panel, state({ version: 2, status: 'reconnecting', attempt: 0, backendPid: null }));
    applyState(panel, state({ version: 3, backendPid: 43, gap: true }));
    expect(panel.state?.backendPid).toBe(43);
    expect(panel.entries).toHaveLength(1);
    expect(panel.entries[0]?.type).toBe('gap');
  });

  it('never lets an older state replace a newer one', () => {
    const panel = newPanel('s1');
    applyState(panel, state({ version: 5, status: 'connected' }));
    // A command reply built before that event arrives late.
    applyState(panel, state({ version: 4, status: 'reconnecting' }));
    expect(panel.state?.status).toBe('connected');
    applyState(panel, state({ version: 6, status: 'stopped' }));
    expect(panel.state?.status).toBe('stopped');
  });

  it('caps the view and counts what scrolled out', () => {
    const panel = newPanel('s1');
    const many = Array.from({ length: MAX_VIEW_NOTIFICATIONS + 5 }, (_, i) => message(String(i)));
    applyBatch(panel, batch(1, many));
    expect(panel.entries).toHaveLength(MAX_VIEW_NOTIFICATIONS);
    expect(panel.scrolledOut).toBe(5);
    expect(panel.entries[0]).toMatchObject({ payload: '5' });
  });

  it('computes the rate over the last seconds', () => {
    expect(rate([1_000, 2_000, 9_000, 10_000], 10_000)).toBe(0.4);
    expect(rate([], 10_000)).toBe(0);
  });
});
