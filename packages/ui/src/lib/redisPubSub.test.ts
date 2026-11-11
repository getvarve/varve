import type { PubSubMessage } from '../api/redis';
import { countReceived, EndFollower, exportMessages, gapText, isGlob, matchesFilter, messageRate, payloadDetail, pushCapped, reasonParts } from './redisPubSub';
import type { RateBucket } from './redisPubSub';

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
    const buckets: RateBucket[] = [];
    for (const at of [0, 9000, 9500, 9900]) countReceived(buckets, at, 1);
    expect(messageRate(buckets, 10_000)).toBe(1);
    expect(messageRate([], 10_000)).toBe(0);
  });

  it('keeps counting at high rates and forgets seconds outside the window', () => {
    const buckets: RateBucket[] = [];
    // 1500 messages a second for ten seconds, in batches of 150 every 100 ms.
    for (let at = 0; at < 10_000; at += 100) countReceived(buckets, at, 150);
    expect(messageRate(buckets, 10_000)).toBe(1500);
    // Half way into the next second the partly counted seconds still give the same rate.
    countReceived(buckets, 10_250, 750);
    expect(messageRate(buckets, 10_500)).toBe(1500);
    expect(buckets.length).toBeLessThanOrEqual(6);
    expect(messageRate(buckets, 20_000)).toBe(0);
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

describe('EndFollower', () => {
  /** A list of `height` px content in a 100 px box; hidden, everything reads 0 as with `display: none`. */
  function box(height: number) {
    return {
      content: height,
      hidden: false,
      top: 0,
      get scrollHeight(): number {
        return this.hidden ? 0 : this.content;
      },
      get clientHeight(): number {
        return this.hidden ? 0 : 100;
      },
      get scrollTop(): number {
        return this.hidden ? 0 : this.top;
      },
      set scrollTop(value: number) {
        if (!this.hidden) this.top = Math.max(0, Math.min(value, this.content - 100));
      },
    };
  }

  it('stays at the end as batches arrive and counts batches while scrolled up', () => {
    const follower = new EndFollower();
    // An empty list is at its end; the first batch makes it taller than its box.
    const el = box(100);
    follower.beforeBatch(el, true);
    el.content = 500;
    expect(follower.afterBatch(el, true, 3, false)).toBe(false);
    expect(el.top).toBe(400);
    expect(follower.unseen).toBe(0);

    // The user scrolls up: the next batch is counted and the rows are held in place.
    el.scrollTop = 120;
    follower.scrolled(el, true);
    follower.beforeBatch(el, true);
    el.content = 600;
    expect(follower.afterBatch(el, true, 5, false)).toBe(true);
    expect(el.top).toBe(120);
    expect(follower.unseen).toBe(5);

    follower.jumpToEnd(el);
    expect(el.top).toBe(500);
    expect(follower.unseen).toBe(0);
    expect(follower.following).toBe(true);
  });

  it('keeps following through a hidden tab and returns to the end when shown', () => {
    const follower = new EndFollower();
    const el = box(100);
    follower.beforeBatch(el, true);
    el.content = 500;
    follower.afterBatch(el, true, 1, false);
    expect(el.top).toBe(400);

    // Hidden: the offset is lost and sizes read 0; batches keep arriving.
    el.hidden = true;
    el.top = 0;
    el.content = 900;
    follower.beforeBatch(el, false);
    expect(follower.afterBatch(el, false, 20, false)).toBe(false);
    expect(follower.following).toBe(true);
    expect(follower.unseen).toBe(0);

    el.hidden = false;
    follower.shown(el);
    expect(el.top).toBe(800);
    // The next batch still follows: no "new messages" for a list the user never scrolled.
    follower.beforeBatch(el, true);
    el.content = 1000;
    follower.afterBatch(el, true, 4, false);
    expect(el.top).toBe(900);
    expect(follower.unseen).toBe(0);
  });

  it('keeps the count and the position of a scrolled up list through a hidden tab', () => {
    const follower = new EndFollower();
    const el = box(500);
    el.scrollTop = 150;
    follower.scrolled(el, true);
    follower.beforeBatch(el, true);
    follower.afterBatch(el, true, 2, false);
    expect(follower.unseen).toBe(2);

    el.hidden = true;
    el.top = 0;
    // A scroll event of the hidden list (offset 0) changes nothing.
    follower.scrolled(el, false);
    el.content = 700;
    follower.beforeBatch(el, false);
    expect(follower.afterBatch(el, false, 6, false)).toBe(false);
    expect(follower.following).toBe(false);
    expect(follower.unseen).toBe(8);

    el.hidden = false;
    follower.shown(el);
    expect(el.top).toBe(150);
    expect(follower.unseen).toBe(8);
  });

  it('counts nothing for an empty list', () => {
    const follower = new EndFollower();
    const el = box(500);
    el.scrollTop = 0;
    follower.scrolled(el, true);
    expect(follower.afterBatch(el, true, 3, true)).toBe(false);
    expect(follower.unseen).toBe(0);
    expect(follower.afterBatch(null, true, 3, false)).toBe(false);
  });
});
