import { flushPromises, mount } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { KeyEntry, ModuleQuery, RedisEditRequest, TsRangeQuery, ValueChunk } from '../../api/redis';
import ValueViewer from './ValueViewer.vue';

const plans: RedisEditRequest[] = [];
const ranges: TsRangeQuery[] = [];
const queries: ModuleQuery[] = [];
let type = 'TSDB-TYPE';

const series: ValueChunk = {
  keyType: { other: 'TSDB-TYPE' },
  total: '3',
  nextCursor: '3001',
  data: {
    kind: 'timeSeries',
    info: {
      totalSamples: '3',
      firstTimestamp: '1000',
      lastTimestamp: '3000',
      meta: { retentionMs: '0', labels: [{ name: 'sensor', value: 'a' }], duplicatePolicy: 'block' },
      sourceKey: null,
      rules: [],
      fields: [],
    },
    samples: [{ timestamp: '1000', value: { t: 'float', v: 1.5 } }],
  },
};

const topk: ValueChunk = {
  keyType: { other: 'TopK-TYPE' },
  total: null,
  nextCursor: null,
  data: {
    kind: 'probabilistic',
    moduleKind: 'topK',
    fields: [{ name: 'k', value: { t: 'int', v: 3 } }],
    top: [{ item: { t: 'text', v: 'home' }, count: '42' }],
  },
};

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisKeyInfo: vi.fn(async () => ({ keyType: { other: type }, ttlMs: null, memoryBytes: '4096', encoding: null, length: null })),
  redisKeyVersion: vi.fn(async () => ({ keyType: { other: type }, length: '3', digest: 't-1', digestMissing: null, expireAtMs: null })),
  redisKeyDigestCancel: vi.fn(async () => undefined),
  redisReadValue: vi.fn(async (): Promise<ValueChunk> => (type === 'TSDB-TYPE' ? series : topk)),
  redisTsRange: vi.fn(async (_s: string, _k: string, query: TsRangeQuery) => {
    ranges.push(query);
    return {
      samples: [
        { timestamp: '3000', value: { t: 'float', v: 3.5 } },
        { timestamp: '2000', value: { t: 'other', v: 'nan' } },
      ],
      next: '1999',
    };
  }),
  redisModuleQuery: vi.fn(async (_s: string, _k: string, query: ModuleQuery) => {
    queries.push(query);
    return { command: 'TOPK.QUERY k home away', rows: [{ input: { t: 'text', v: 'home' }, output: { t: 'bool', v: true } }, { input: { t: 'text', v: 'away' }, output: { t: 'bool', v: false } }] };
  }),
  redisEditPlan: vi.fn(async (_s: string, request: RedisEditRequest) => {
    plans.push(request);
    return { planId: 'p1', plan: { key: request.key, db: 0, commands: ['X'], checks: [], risk: 'write', destructive: false, unchecked: false, warnings: [] } };
  }),
}));

function button(wrapper: ReturnType<typeof mount>, text: string) {
  return wrapper.findAll('button').find((b) => b.text() === text);
}

describe('ValueViewer with a time series', () => {
  const entry: KeyEntry = { name: '7473', display: 'ts', keyType: { other: 'TSDB-TYPE' }, ttlMs: null };

  beforeEach(() => {
    type = 'TSDB-TYPE';
    plans.length = 0;
    ranges.length = 0;
  });

  it('shows the series, pages newest first and leaves paging to the series view', async () => {
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    expect(wrapper.text()).toContain('TIME SERIES');
    expect(wrapper.text()).toContain('sensor=a');
    expect(ranges[0]).toEqual({ from: null, to: null, reverse: true, count: 200, cursor: null });
    expect(wrapper.text()).toContain('3.5');
    expect(wrapper.text()).toContain('nan');
    await button(wrapper, 'Load more')?.trigger('click');
    await flushPromises();
    expect(ranges[1]?.cursor).toBe('1999');
    wrapper.unmount();
  });

  it('plans a sample add, a range delete with the version, and a settings change against what was read', async () => {
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    const add = wrapper.find('form[aria-label="Add a sample"]');
    await add.find('input[inputmode="decimal"]').setValue('7.25');
    await add.trigger('submit');
    await flushPromises();
    expect(plans[0]?.edits).toEqual([{ op: 'tsAdd', timestamp: null, value: { t: 'text', v: '7.25' } }]);

    const remove = wrapper.find('form[aria-label="Delete samples"]');
    const [from, to] = remove.findAll('input');
    await from?.setValue('1000');
    await to?.setValue('2000');
    await remove.trigger('submit');
    await flushPromises();
    const request = plans.at(-1);
    expect(request?.edits).toEqual([{ op: 'tsDeleteRange', from: '1000', to: '2000' }]);
    expect(request?.expect).toMatchObject({ state: 'present', version: { digest: 't-1' } });
    wrapper.unmount();
  });

  it('plans a settings change and warns when the retention drops samples', async () => {
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    const settings = wrapper.find('form[aria-label="Series settings"]');
    await settings.find('input').setValue('3600000');
    expect(wrapper.text()).toContain('removes the samples older than that');
    await settings.trigger('submit');
    await flushPromises();
    expect(plans.at(-1)?.edits).toEqual([
      {
        op: 'tsAlter',
        retentionMs: '3600000',
        labels: null,
        duplicatePolicy: null,
        original: { retentionMs: '0', labels: [{ name: 'sensor', value: 'a' }], duplicatePolicy: 'block' },
      },
    ]);
    wrapper.unmount();
  });

  it('edits labels as rows and keeps their values exactly', async () => {
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    const settings = wrapper.find('form[aria-label="Series settings"]');
    await settings.find('input[aria-label="Label 1 value"]').setValue(' a b ');
    await settings.findAll('button').find((b) => b.text() === 'Add label')?.trigger('click');
    await settings.find('input[aria-label="Label 2 name"]').setValue('x=y');
    await settings.find('input[aria-label="Label 2 value"]').setValue('1');
    await settings.trigger('submit');
    await flushPromises();
    const edit = plans.at(-1)?.edits[0];
    expect(edit?.op === 'tsAlter' && edit.labels).toEqual([
      { name: 'sensor', value: ' a b ' },
      { name: 'x=y', value: '1' },
    ]);
    expect(edit?.op === 'tsAlter' && edit.retentionMs).toBeNull();
    wrapper.unmount();
  });

  it('hides the changes on a read-only connection', async () => {
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry, readOnly: true }, attachTo: document.body });
    await flushPromises();
    expect(wrapper.find('form[aria-label="Add a sample"]').exists()).toBe(false);
    expect(wrapper.find('form[aria-label="Sample range"]').exists()).toBe(true);
    wrapper.unmount();
  });
});

describe('ValueViewer with a Top-K', () => {
  const entry: KeyEntry = { name: '6b', display: 'k', keyType: { other: 'TopK-TYPE' }, ttlMs: null };

  beforeEach(() => {
    type = 'TopK-TYPE';
    plans.length = 0;
    queries.length = 0;
  });

  it('lists the top items, answers queries and plans adds', async () => {
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    expect(wrapper.text()).toContain('TOP-K');
    expect(wrapper.text()).toContain('home');
    expect(wrapper.text()).toContain('42');

    const query = wrapper.find('form[aria-label="Query"]');
    await query.find('textarea').setValue('home\naway');
    await query.trigger('submit');
    await flushPromises();
    expect(queries).toEqual([{ op: 'topkQuery', items: [{ t: 'text', v: 'home' }, { t: 'text', v: 'away' }] }]);
    expect(wrapper.text()).toContain('in the top list');
    expect(wrapper.text()).toContain('not in the top list');

    const add = wrapper.find('form[aria-label="Add"]');
    await add.find('textarea').setValue('a\nb');
    await add.trigger('submit');
    await flushPromises();
    expect(plans[0]?.edits).toEqual([{ op: 'topkAdd', items: [{ t: 'text', v: 'a' }, { t: 'text', v: 'b' }] }]);
    expect(plans[0]?.expect).toMatchObject({ state: 'present', keyType: { other: 'TopK-TYPE' } });
    wrapper.unmount();
  });
});
