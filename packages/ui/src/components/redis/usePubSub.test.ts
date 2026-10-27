import { flushPromises } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { PubSubEvent, PubSubNames } from '../../api/redis';
import { usePubSub } from './usePubSub';

let handler: ((e: { payload: PubSubEvent }) => void) | null = null;
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async (_event: string, h: (e: { payload: PubSubEvent }) => void) => {
    handler = h;
    return () => {
      handler = null;
    };
  }),
}));

const acks: [string, number][] = [];
const subscribeCalls: PubSubNames[] = [];
const changes: [PubSubNames, PubSubNames][] = [];
const unsubscribed: string[] = [];
let releaseList: () => void = () => undefined;

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisPubsubSubscribe: vi.fn(async (_session: string, names: PubSubNames) => {
    subscribeCalls.push(names);
    // The server confirms before the call returns: the store must keep this event.
    handler?.({ payload: { kind: 'subscribed', subscriptionId: 'sub1', nameKind: 'channel', name: names.channels[0] ?? '', count: 1 } });
    return { subscriptionId: 'sub1', event: 'redis-pubsub' };
  }),
  redisPubsubChange: vi.fn(async (_id: string, add: PubSubNames, remove: PubSubNames) => {
    changes.push([add, remove]);
  }),
  redisPubsubAck: vi.fn(async (id: string, seq: number) => {
    acks.push([id, seq]);
  }),
  redisPubsubUnsubscribe: vi.fn(async (id: string) => {
    unsubscribed.push(id);
  }),
  redisPubsubSubscriptions: vi.fn(async (sessionId: string) =>
    sessionId === 'racing'
      ? await new Promise<unknown[]>((resolve) => {
          releaseList = () =>
            resolve([{ subscriptionId: 'old3', channels: ['kept'], patterns: [], shardChannels: [], state: 'subscribed', delivered: 0, dropped: 0 }]);
        })
      : sessionId === 'reloaded'
      ? [
          { subscriptionId: 'old1', channels: ['orders'], patterns: ['audit.*'], shardChannels: [], state: 'subscribed', delivered: 4, dropped: 0 },
          { subscriptionId: 'old2', channels: ['orders', 'jobs'], patterns: [], shardChannels: [], state: 'reconnecting', delivered: 0, dropped: 0 },
        ]
      : [],
  ),
}));

function batch(seq: number, n: number, dropped = 0): PubSubEvent {
  return {
    kind: 'batch',
    subscriptionId: 'sub1',
    seq,
    dropped,
    messages: Array.from({ length: n }, (_, i) => ({
      channel: 'orders',
      pattern: null,
      payload: { t: 'text' as const, v: `m${seq}-${i}` },
      size: '5',
      truncated: false,
      receivedAt: 1000 + i,
    })),
  };
}

describe('usePubSub', () => {
  it('subscribes, keeps early confirmations, shows batches, acks them and caps the view', async () => {
    const pubsub = usePubSub();
    const panel = pubsub.panel('tab-1', 's1');
    expect(await pubsub.subscribe('tab-1', { channels: ['orders'], patterns: [] })).toBeNull();
    expect(subscribeCalls).toEqual([{ channels: ['orders'], patterns: [], shardChannels: [] }]);
    expect(panel.subscriptionId).toBe('sub1');
    expect(panel.names).toEqual([{ kind: 'channel', name: 'orders', confirmed: true, messages: 0 }]);

    handler?.({ payload: batch(1, 300) });
    handler?.({ payload: batch(2, 300, 7) });
    await flushPromises();
    expect(acks).toEqual([
      ['sub1', 1],
      ['sub1', 2],
    ]);
    expect(panel.received).toBe(600);
    expect(panel.messages).toHaveLength(pubsub.MAX_VIEW_MESSAGES);
    expect(panel.messages.at(-1)).toMatchObject({ entry: 'message', payload: { t: 'text', v: 'm2-299' } });
    expect(panel.droppedByBackend).toBe(7);
    expect(panel.names[0]?.messages).toBe(600);

    // Paused: messages are held, the view stays still, and resume shows them.
    pubsub.setPaused('tab-1', true);
    handler?.({ payload: batch(3, 2) });
    expect(panel.held).toHaveLength(2);
    expect(panel.messages.at(-1)).toMatchObject({ entry: 'message', payload: { t: 'text', v: 'm2-299' } });
    pubsub.setPaused('tab-1', false);
    expect(panel.messages.at(-1)).toMatchObject({ entry: 'message', payload: { t: 'text', v: 'm3-1' } });

    // A second name goes onto the same connection.
    await pubsub.subscribe('tab-1', { channels: [], patterns: ['refunds.*'] });
    expect(changes[0]).toEqual([{ channels: [], patterns: ['refunds.*'], shardChannels: [] }, { channels: [], patterns: [], shardChannels: [] }]);

    // A server refusal is shown; the subscription goes on.
    handler?.({ payload: { kind: 'error', subscriptionId: 'sub1', message: 'No permission', code: 'NOPERM' } });
    expect(panel.errors).toEqual(['No permission (NOPERM)']);
    // A refused name says which and leaves the list; the others stay.
    expect(panel.names.map((n) => n.name)).toEqual(['orders', 'refunds.*']);
    handler?.({
      payload: { kind: 'error', subscriptionId: 'sub1', message: 'No permission', code: 'NOPERM', nameKind: 'pattern', name: 'refunds.*' },
    });
    expect(panel.names.map((n) => n.name)).toEqual(['orders']);
    expect(panel.errors.at(-1)).toBe('refunds.*: No permission (NOPERM)');

    // The connection is lost and comes back: a reconnecting state, then a gap marker in the list.
    handler?.({ payload: { kind: 'reconnecting', subscriptionId: 'sub1', attempt: 2, reason: 'the server did not answer a PING' } });
    expect(panel.reconnecting).toEqual({ attempt: 2, reason: 'the server did not answer a PING' });
    expect(panel.names.every((n) => !n.confirmed)).toBe(true);
    handler?.({ payload: { kind: 'gap', subscriptionId: 'sub1', reason: 'the server did not answer a PING', lostAt: 5000, resumedAt: 9000 } });
    expect(panel.reconnecting).toBeNull();
    expect(panel.messages.at(-1)).toMatchObject({ entry: 'gap', lostAt: 5000, resumedAt: 9000 });
    handler?.({ payload: { kind: 'subscribed', subscriptionId: 'sub1', nameKind: 'shardChannel', name: 'cart.{1}', count: 3 } });
    expect(panel.names.at(-1)).toEqual({ kind: 'shardChannel', name: 'cart.{1}', confirmed: true, messages: 0 });

    // The connection drops for good: the names stay so the user can subscribe again.
    handler?.({ payload: { kind: 'closed', subscriptionId: 'sub1', reason: 'connection lost' } });
    expect(panel.subscriptionId).toBeNull();
    expect(panel.closed).toBe('connection lost');
    expect(panel.names.every((n) => !n.confirmed)).toBe(true);

    await pubsub.close('tab-1');
    expect(pubsub.panels.has('tab-1')).toBe(false);
  });

  it('closes the connection when the last name is unsubscribed', async () => {
    const pubsub = usePubSub();
    const panel = pubsub.panel('tab-2', 's2');
    await pubsub.subscribe('tab-2', { channels: ['orders'], patterns: [] });
    const name = panel.names[0];
    expect(name).toBeDefined();
    if (name) await pubsub.unsubscribe('tab-2', name);
    expect(unsubscribed).toContain('sub1');
    expect(panel.names).toEqual([]);
    expect(panel.subscriptionId).toBeNull();
    await pubsub.close('tab-2');
  });

  it('takes over the subscriptions a reloaded window left running on the backend', async () => {
    const store = usePubSub();
    store.panel('tab-reload', 'reloaded');
    unsubscribed.length = 0;
    subscribeCalls.length = 0;
    expect(await store.reattach('tab-reload')).toBeNull();
    expect(unsubscribed.sort()).toEqual(['old1', 'old2']);
    expect(subscribeCalls).toEqual([{ channels: ['orders', 'jobs'], patterns: ['audit.*'], shardChannels: [] }]);
    const p = store.panels.get('tab-reload');
    expect(p?.subscriptionId).toBe('sub1');
    expect(p?.errors.at(-1)).toMatch(/missed/);
    // Checked once per session: a second view of the tab does not repeat it.
    expect(await store.reattach('tab-reload')).toBeNull();
    expect(subscribeCalls).toHaveLength(1);
    await store.close('tab-reload');
  });

  it('does not lose the taken-over names when the user subscribes meanwhile', async () => {
    const store = usePubSub();
    store.panel('tab-race', 'racing');
    subscribeCalls.length = 0;
    const taking = store.reattach('tab-race');
    await flushPromises();
    // The panel is busy while the old subscriptions are taken over.
    expect(await store.subscribe('tab-race', { channels: ['typed'], patterns: [] })).toBeNull();
    releaseList();
    expect(await taking).toBeNull();
    const p = store.panels.get('tab-race');
    expect(p?.names.map((n) => n.name)).toContain('kept');
    expect(p?.errors.at(-1)).toMatch(/opened again/);
    await store.close('tab-race');
  });
});
