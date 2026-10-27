import { flushPromises, mount } from '@vue/test-utils';
import type * as RecordsModule from '../../api/records';
import type * as RedisModule from '../../api/redis';
import type * as SessionsModule from '../../api/sessions';
import type { ConnectionProfile } from '../../api/records';
import type { KeyEntry, ScanPage } from '../../api/redis';
import type { SessionInfo } from '../../api/sessions';
import { useTabs } from '../../composables/useTabs';
import type { KeysTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import KeyBrowser from './KeyBrowser.vue';

const profile = {
  id: 'r1',
  name: 'Cache',
  engine: 'redis',
  environment: 'development',
  folderId: null,
  database: null,
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { redisDb: 0 },
  savedSecrets: [],
} as unknown as ConnectionProfile;

/** Every SCAN call, answered by the test when it chooses. */
const scans: { cursor: string; keyType: unknown; resolve: (page: ScanPage) => void }[] = [];
const redisScan = vi.fn(
  (_session: string, cursor: string, _pattern: string, keyType: unknown) =>
    new Promise<ScanPage>((resolve) => {
      scans.push({ cursor, keyType, resolve });
    }),
);
/** The module key types `redis_modules` reports. */
let moduleKeyTypes: string[] = [];

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisScan: (...args: [string, string, string, unknown]) => redisScan(...args),
  redisKeyspace: vi.fn(async () => []),
  redisModules: vi.fn(async () => ({ features: [], modules: null, keyTypes: moduleKeyTypes })),
}));
vi.mock('../../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: vi.fn(
    async (): Promise<SessionInfo> => ({
      id: 's1',
      connectionId: 'r1',
      engine: 'redis',
      server: { engine: 'redis', version: '7.4.1', major: 7, minor: 4, patch: 1 },
      environment: 'development',
      database: null,
      readOnly: false,
      confirmDestructive: true,
      purpose: 'user',
      openedAt: 0,
      encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
    }),
  ),
  closeSession: vi.fn(async () => undefined),
}));
vi.mock('../../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [profile], queries: [] })),
}));

function key(display: string): KeyEntry {
  const name = [...new TextEncoder().encode(display)].map((b) => b.toString(16).padStart(2, '0')).join('');
  return { name, display, keyType: 'string', ttlMs: null };
}

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
async function settle(): Promise<void> {
  for (let i = 0; i < 5; i += 1) {
    await flushPromises();
    await tick();
  }
}

/** The scan form's type filter (the first select is the database picker). */
function typeSelect(wrapper: ReturnType<typeof mount>) {
  const id = wrapper.findAll('label').find((l) => l.text() === 'Type filter')?.attributes('for') ?? '';
  return wrapper.find<HTMLSelectElement>(`#${CSS.escape(id)}`);
}

function loadMore(wrapper: ReturnType<typeof mount>) {
  return wrapper.findAll('button').find((b) => b.text().startsWith('Load more'));
}

describe('KeyBrowser remount', () => {
  it('drops the old instance scan, rescans the first page and keeps Load more off while scanning', async () => {
    await useWorkspace().refresh();
    const tabs = useTabs();
    const opened = tabs.openKeysTab(profile);
    await settle();
    const tab = (): KeysTab => tabs.tabs.find((t) => t.id === opened.id) as KeysTab;
    expect(tab().session?.id).toBe('s1');

    // The first instance starts a scan that answers late.
    const first = mount(KeyBrowser, { props: { tab: tab() }, attachTo: document.body });
    await settle();
    expect(scans).toHaveLength(1);
    expect(first.text()).toContain('Scanning');
    first.unmount();

    // A new instance mounts while that scan is still pending: it asks for the first page again.
    const second = mount(KeyBrowser, { props: { tab: tab() }, attachTo: document.body });
    await settle();
    expect(scans).toHaveLength(2);
    expect(scans[1]?.cursor).toBe('0');
    expect(second.text()).toContain('Scanning');

    // The old answer arrives now and must not write anything.
    scans[0]?.resolve({ cursor: '0', keys: [key('old:1')] });
    await settle();
    expect(tab().index.size).toBe(0);
    expect(second.text()).toContain('Scanning');

    // The new instance's own first page lands, with more to come.
    scans[1]?.resolve({ cursor: '42', keys: [key('new:1')] });
    await settle();
    expect(tab().index.entries().map((k) => k.display)).toEqual(['new:1']);
    expect(second.text()).not.toContain('Scanning');
    const button = loadMore(second);
    expect(button?.attributes('disabled')).toBeUndefined();

    // While the next page is read, Load more is off and the scan state shows.
    await button?.trigger('click');
    await settle();
    expect(scans).toHaveLength(3);
    expect(scans[2]?.cursor).toBe('42');
    expect(loadMore(second)?.attributes('disabled')).toBeDefined();
    expect(second.text()).toContain('Scanning');

    scans[2]?.resolve({ cursor: '0', keys: [key('new:2')] });
    await settle();
    expect(tab().index.size).toBe(2);
    expect(second.text()).toContain('SCAN complete');
    second.unmount();
    await tabs.closeTab(opened.id);
  });
});

describe('KeyBrowser type filter', () => {
  it('offers the module key types the server stores, and scans for them by their TYPE name', async () => {
    scans.length = 0;
    moduleKeyTypes = ['ReJSON-RL', 'TSDB-TYPE', 'bloomfltr'];
    await useWorkspace().refresh();
    const tabs = useTabs();
    const opened = tabs.openKeysTab(profile);
    await settle();
    const tab = (): KeysTab => tabs.tabs.find((t) => t.id === opened.id) as KeysTab;
    const wrapper = mount(KeyBrowser, { props: { tab: tab() }, attachTo: document.body });
    await settle();
    scans[0]?.resolve({ cursor: '0', keys: [] });
    await settle();
    const select = typeSelect(wrapper);
    expect([...select.element.options].map((o) => o.text.trim())).toEqual([
      'All types',
      'string',
      'hash',
      'list',
      'set',
      'zset',
      'stream',
      'ReJSON-RL (JSON)',
      'TSDB-TYPE (Time series)',
      'bloomfltr (Bloom filter)',
    ]);
    for (const [value, sent] of [
      ['TSDB-TYPE', { other: 'TSDB-TYPE' }],
      ['json', 'json'],
      ['hash', 'hash'],
    ] as const) {
      await select.setValue(value);
      await wrapper.find('form').trigger('submit');
      await settle();
      expect(scans.at(-1)?.keyType).toEqual(sent);
      scans.at(-1)?.resolve({ cursor: '0', keys: [] });
      await settle();
    }
    wrapper.unmount();
    await tabs.closeTab(opened.id);
  });

  it('offers only the core types on a server without modules', async () => {
    scans.length = 0;
    moduleKeyTypes = [];
    await useWorkspace().refresh();
    const tabs = useTabs();
    const opened = tabs.openKeysTab(profile);
    await settle();
    const tab = (): KeysTab => tabs.tabs.find((t) => t.id === opened.id) as KeysTab;
    const wrapper = mount(KeyBrowser, { props: { tab: tab() }, attachTo: document.body });
    await settle();
    const options = [...typeSelect(wrapper).element.options].map((o) => o.value);
    expect(options).toEqual(['', 'string', 'hash', 'list', 'set', 'zset', 'stream']);
    scans[0]?.resolve({ cursor: '0', keys: [] });
    await settle();
    wrapper.unmount();
    await tabs.closeTab(opened.id);
  });
});
