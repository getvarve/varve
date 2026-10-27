import type { PubSubMessage } from '../api/redis';
import { exportMessages, gapText, isGlob, matchesFilter, messageRate, payloadDetail, pushCapped, reasonParts } from './redisPubSub';

function message(payload: string, channel = 'orders.created', pattern: string | null = null): PubSubMessage {
  return { channel, pattern, payload: { t: 'text', v: payload }, size: String(payload.length), truncated: false, receivedAt: 0 };
}

describe('Pub/Sub helpers', () => {
  it('keeps the latest messages and counts what it dropped', () => {
    const list = [1, 2, 3];
    expect(pushCapped(list, [4, 5], 4)).toBe(1);
    expect(list).toEqual([2, 3, 4, 5]);
  });

  it('filters on channel, pattern and payload', () => {
    const m = message('{"id":7}', 'orders.created', 'orders.*');
    expect(matchesFilter(m, 'ORDERS')).toBe(true);
    expect(matchesFilter(m, '"id"')).toBe(true);
    expect(matchesFilter(m, 'refund')).toBe(false);
    expect(matchesFilter(m, '  ')).toBe(true);
  });

  it('pretty prints JSON payloads only when whole and asked', () => {
    expect(payloadDetail(message('{"a":1}'), true)).toBe('{\n  "a": 1\n}');
    expect(payloadDetail(message('{"a":1}'), false)).toBe('{"a":1}');
    expect(payloadDetail({ ...message('{"a":'), truncated: true }, true)).toBe('{"a":');
  });

  it('computes the rate over the last five seconds', () => {
    expect(messageRate([0, 9000, 9500, 9900], 10_000)).toBe(1);
    expect(messageRate([], 10_000)).toBe(0);
  });

  it('spots glob patterns and exports messages as JSON', () => {
    expect(isGlob('orders.*')).toBe(true);
    expect(isGlob('orders.created')).toBe(false);
    const out = JSON.parse(exportMessages([message('x')])) as { payload: string; encoding: string }[];
    expect(out[0]).toMatchObject({ payload: 'x', encoding: 'text' });
  });

  it('splits the output buffer CONFIG hint from a lost-connection reason', () => {
    const reason =
      'The server dropped the subscription because its pubsub output buffer limit was exceeded (now "pubsub 33554432 8388608 60"): messages were published faster than Varve could read them. To keep up with such bursts, raise the limit, for example CONFIG SET client-output-buffer-limit "pubsub 256mb 64mb 60"';
    const parts = reasonParts(reason);
    expect(parts.command).toBe('CONFIG SET client-output-buffer-limit "pubsub 256mb 64mb 60"');
    expect(parts.text.endsWith('raise the limit.')).toBe(true);
    expect(reasonParts('the server did not answer a PING')).toEqual({ text: 'the server did not answer a PING', command: null });
  });

  it('marks gaps, keeps them in filtered views and exports them', () => {
    const gap = { id: 1, entry: 'gap' as const, reason: 'lost', lostAt: 0, resumedAt: 4000 };
    expect(gapText(gap)).toContain('(4 s)');
    expect(matchesFilter(gap, 'nothing matches')).toBe(true);
    const out = JSON.parse(exportMessages([gap])) as { gap: boolean }[];
    expect(out[0]?.gap).toBe(true);
  });
});
