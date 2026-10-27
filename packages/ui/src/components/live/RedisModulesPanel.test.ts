import { flushPromises, mount } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { ModuleReport, SearchIndex } from '../../api/redis';
import type { SessionInfo } from '../../api/sessions';
import RedisModulesPanel from './RedisModulesPanel.vue';

const report: ModuleReport = {
  features: [
    { feature: 'search', available: true, reason: null },
    { feature: 'timeSeries', available: false, reason: 'This server has no TS.* commands' },
  ],
  modules: [{ name: 'search', version: '81001' }],
  keyTypes: [],
};

const rebuilt: SearchIndex = {
  name: 'idx',
  engine: 'rediSearch',
  keyType: 'HASH',
  prefixes: ['user:'],
  filter: null,
  language: null,
  languageField: null,
  indexesAll: false,
  defaultScore: null,
  scoreField: null,
  payloadField: null,
  indexOptions: [],
  stopwords: null,
  attributes: [{ identifier: 'name', attribute: 'name', fieldType: 'TEXT', options: ['NOSTEM', 'SORTABLE'], unknown: [] }],
  numDocs: '1',
  indexingFailures: '0',
  percentIndexed: 1,
  stats: [],
  ddl: 'FT.CREATE idx ON HASH PREFIX 1 user: SCHEMA name TEXT NOSTEM SORTABLE',
  ddlDisplay: 'FT.CREATE idx ON HASH PREFIX 1 user: SCHEMA\n  name TEXT NOSTEM SORTABLE',
  notes: ['Rebuilt from FT.INFO'],
  raw: 'index_name idx\n',
};

let shown: SearchIndex = rebuilt;
const copies: string[] = [];

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisModules: vi.fn(async () => report),
  redisSearchIndexes: vi.fn(async () => ['idx']),
  redisSearchIndex: vi.fn(async () => shown),
}));

const session = { id: 's1', environment: 'development' } as unknown as SessionInfo;

describe('RedisModulesPanel', () => {
  beforeEach(() => {
    copies.length = 0;
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText: vi.fn(async (text: string) => void copies.push(text)) },
    });
  });

  it('shows the features with reasons and copies the one-line command', async () => {
    shown = rebuilt;
    const wrapper = mount(RedisModulesPanel, { props: { session } });
    await flushPromises();
    expect(wrapper.text()).toContain('This server has no TS.* commands');
    await wrapper.findAll('button').find((b) => b.text() === 'idx')?.trigger('click');
    await flushPromises();
    expect(wrapper.find('pre[aria-label="Index definition"]').text()).toContain('\n  name TEXT');
    await wrapper.findAll('button').find((b) => b.text() === 'Copy command')?.trigger('click');
    await flushPromises();
    expect(copies).toEqual(['FT.CREATE idx ON HASH PREFIX 1 user: SCHEMA name TEXT NOSTEM SORTABLE']);
    expect(copies[0]).not.toContain('\n');
  });

  it('shows the raw reply and no copy when the definition cannot be rebuilt', async () => {
    shown = { ...rebuilt, ddl: null, ddlDisplay: null, notes: ['No FT.CREATE command is shown'], raw: 'attributes:\n  FUTUREFLAG\n' };
    const wrapper = mount(RedisModulesPanel, { props: { session } });
    await flushPromises();
    await wrapper.findAll('button').find((b) => b.text() === 'idx')?.trigger('click');
    await flushPromises();
    expect(wrapper.find('pre[aria-label="Raw FT.INFO reply"]').text()).toContain('FUTUREFLAG');
    expect(wrapper.text()).toContain('No FT.CREATE command is shown');
    expect(wrapper.findAll('button').some((b) => b.text() === 'Copy command')).toBe(false);
  });
});
