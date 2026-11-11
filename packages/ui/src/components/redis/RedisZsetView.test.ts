import { flushPromises, mount } from '@vue/test-utils';
import { reactive } from 'vue';
import type * as RedisModule from '../../api/redis';
import type { ZsetRange, ZsetRangePage } from '../../api/redis';
import { EditDraft } from '../../lib/redisEdit';
import RedisZsetView from './RedisZsetView.vue';

let fail = false;
const ranges: ZsetRange[] = [];

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisZsetRange: vi.fn(async (_s: string, _k: string, range: ZsetRange): Promise<ZsetRangePage | null> => {
    ranges.push(range);
    if (fail) throw { kind: 'internal', message: 'ZRANGE refused' };
    return { members: [{ member: { t: 'text', v: 'm1' }, score: { t: 'int', v: 15 } }], nextOffset: null };
  }),
}));

function view() {
  return mount(RedisZsetView, {
    props: {
      tabId: 't1',
      sessionId: 's1',
      keyName: 'scores',
      ranked: [{ member: { t: 'text', v: 'ranked-only' }, score: { t: 'int', v: 1 } }],
      draft: reactive(new EditDraft()) as EditDraft,
      editable: true,
    },
  });
}

function mode(wrapper: ReturnType<typeof mount>, label: string) {
  return wrapper.findAll('button[role="radio"]').find((b) => b.text() === label);
}

describe('RedisZsetView', () => {
  it('shows a failed first range read instead of the rank list', async () => {
    fail = true;
    const wrapper = view();
    await mode(wrapper, 'By score')?.trigger('click');
    await wrapper.find('form.range').trigger('submit');
    await flushPromises();
    expect(wrapper.text()).toContain('ZRANGE refused');
    expect(wrapper.text()).not.toContain('ranked-only');

    // Back to rank: the range error goes with the range form.
    await mode(wrapper, 'By rank')?.trigger('click');
    expect(wrapper.text()).not.toContain('ZRANGE refused');
    expect(wrapper.text()).toContain('ranked-only');
    fail = false;
    wrapper.unmount();
  });

  it('drops the range of another mode when the mode changes', async () => {
    fail = false;
    const wrapper = view();
    await mode(wrapper, 'By score')?.trigger('click');
    await wrapper.find('form.range').trigger('submit');
    await flushPromises();
    expect(wrapper.text()).toContain('BYSCORE');
    expect(wrapper.text()).toContain('m1');

    // The lex read fails: the score rows and their command must not stay under "By lex".
    fail = true;
    await mode(wrapper, 'By lex')?.trigger('click');
    expect(wrapper.text()).not.toContain('BYSCORE');
    await wrapper.find('form.range').trigger('submit');
    await flushPromises();
    expect(wrapper.text()).toContain('ZRANGE refused');
    expect(wrapper.text()).not.toContain('BYSCORE');
    expect(wrapper.text()).not.toContain('m1');
    fail = false;
    wrapper.unmount();
  });

  it('shows the command of the applied range, not of bounds typed since', async () => {
    fail = false;
    const wrapper = view();
    await mode(wrapper, 'By score')?.trigger('click');
    const [min, max] = wrapper.findAll('input.bound');
    await min?.setValue('10');
    await max?.setValue('(20');
    await wrapper.find('form.range').trigger('submit');
    await flushPromises();
    expect(wrapper.text()).toContain('ZRANGE scores (20 10 BYSCORE REV LIMIT 0 50');
    await min?.setValue('500');
    expect(wrapper.text()).toContain('ZRANGE scores (20 10 BYSCORE REV LIMIT 0 50');
    expect(wrapper.text()).not.toContain('ZRANGE scores (20 500');
    wrapper.unmount();
  });
});
