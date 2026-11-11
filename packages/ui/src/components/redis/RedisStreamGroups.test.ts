import { flushPromises, mount, type DOMWrapper } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { PendingQuery, StreamGroup } from '../../api/redis';
import RedisStreamGroups from './RedisStreamGroups.vue';

const queries: [string, PendingQuery][] = [];
const group = (name: string): StreamGroup => ({ name, consumers: '1', pending: '12', lastDeliveredId: '1-0', entriesRead: null, lag: null });

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisStreamInfo: vi.fn(async () => ({
    length: '3',
    lastGeneratedId: '3-0',
    groups: '2',
    firstEntryId: '1-0',
    lastEntryId: '3-0',
    maxDeletedEntryId: null,
    entriesAdded: '3',
    recordedFirstEntryId: null,
  })),
  redisStreamGroups: vi.fn(async () => [group('A'), group('B')]),
  redisStreamConsumers: vi.fn(async (_s: string, _k: string, g: string) => [{ name: g === 'A' ? 'c1' : 'c2', pending: '12', idleMs: '5', inactiveMs: null }]),
  redisStreamPending: vi.fn(async (_s: string, _k: string, g: string, query: PendingQuery) => {
    queries.push([g, query]);
    return { entries: [], nextStart: null };
  }),
}));

/** The pending entries' consumer filter (its first option is "all"). */
function consumerFilter(wrapper: ReturnType<typeof mount>) {
  const select = wrapper.findAll('select').find((s) => s.find('option').text() === 'all');
  if (!select) throw new Error('no consumer filter');
  return select as unknown as DOMWrapper<HTMLSelectElement>;
}

function groupButton(wrapper: ReturnType<typeof mount>, name: string) {
  return wrapper.findAll('button.link').find((b) => b.text() === name);
}

describe('RedisStreamGroups', () => {
  it('starts every group with an empty pending filter', async () => {
    const wrapper = mount(RedisStreamGroups, { props: { tabId: 't1', sessionId: 's1', keyName: 'events', editable: true, refresh: 0 } });
    await flushPromises();
    await groupButton(wrapper, 'A')?.trigger('click');
    await flushPromises();
    await consumerFilter(wrapper).setValue('c1');
    await wrapper.find('input.idle').setValue('1000');

    await groupButton(wrapper, 'B')?.trigger('click');
    await flushPromises();
    expect(queries.at(-1)).toEqual(['B', { start: null, count: 50, consumer: null, minIdleMs: null }]);
    expect(consumerFilter(wrapper).element.value).toBe('');
    expect(wrapper.find<HTMLInputElement>('input.idle').element.value).toBe('');
    wrapper.unmount();
  });
});
