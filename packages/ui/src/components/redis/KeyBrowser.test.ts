import { flushPromises, mount } from '@vue/test-utils';
import type * as RecordsModule from '../../api/records';
import type * as RedisModule from '../../api/redis';
import type * as SessionsModule from '../../api/sessions';
import type { ConnectionProfile } from '../../api/records';
import type { KeyEntry, ScanPage, ValueChunk } from '../../api/redis';
import type { SessionInfo } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
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
const scans: { session: string; cursor: string; keyType: unknown; resolve: (page: ScanPage) => void; reject: (error: unknown) => void }[] = [];
const redisScan = vi.fn(
  (session: string, cursor: string, _pattern: string, keyType: unknown) =>
    new Promise<ScanPage>((resolve, reject) => {
      scans.push({ session, cursor, keyType, resolve, reject });
    }),
);
/** The id of the next session `openSession` hands out (a reconnect gets a new one). */
let nextSession = 's1';
/** The content digest the key viewer reads; another one means the key changed on the server. */
let digest = 'd1';
/** The module key types `redis_modules` reports. */
let moduleKeyTypes: string[] = [];

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisScan: (...args: [string, string, string, unknown]) => redisScan(...args),
  redisKeyspace: vi.fn(async () => []),
  redisModules: vi.fn(async () => ({ features: [], modules: null, keyTypes: moduleKeyTypes })),
  // The key viewer: every key is a small hash.
  redisKeyInfo: vi.fn(async () => ({ keyType: 'hash', ttlMs: null, memoryBytes: '120', encoding: 'listpack', length: '1' })),
  redisKeyVersion: vi.fn(async () => ({ keyType: 'hash', length: '1', digest, digestMissing: null, expireAtMs: null })),
  redisReadValue: vi.fn(
    async (): Promise<ValueChunk> => ({
      keyType: 'hash',
      total: '1',
      nextCursor: null,
      data: { kind: 'hash', entries: [{ field: { t: 'text', v: 'name' }, value: { t: 'text', v: 'Kim' } }] },
    }),
  ),
}));
vi.mock('../../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: vi.fn(
    async (): Promise<SessionInfo> => ({
      id: nextSession,
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

function stopButton(wrapper: ReturnType<typeof mount>) {
  return wrapper.findAll('button').find((b) => b.text() === 'Stop');
}

/** The tree row of a key or prefix by its label. */
function row(wrapper: ReturnType<typeof mount>, label: string) {
  return wrapper.findAll('li[role="treeitem"]').find((r) => r.find('.name').text() === label);
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

    // The new instance's own first page lands, with more to come. One key is
    // fewer than COUNT, so the batch goes on from the returned cursor.
    scans[1]?.resolve({ cursor: '42', keys: [key('new:1')] });
    await settle();
    expect(tab().index.entries().map((k) => k.display)).toEqual(['new:1']);
    expect(scans).toHaveLength(3);
    expect(scans[2]?.cursor).toBe('42');
    expect(second.text()).toContain('Scanning');
    expect(loadMore(second)).toBeUndefined();

    // Stop ends the batch; the stopped step's late answer is dropped.
    await stopButton(second)?.trigger('click');
    await settle();
    expect(second.text()).not.toContain('Scanning');
    scans[2]?.resolve({ cursor: '0', keys: [key('late')] });
    await settle();
    expect(tab().index.size).toBe(1);
    expect(tab().cursor).toBe('42');

    // Load more continues from the last cursor; while it runs, Stop replaces it.
    const button = loadMore(second);
    expect(button?.attributes('disabled')).toBeUndefined();
    await button?.trigger('click');
    await settle();
    expect(scans).toHaveLength(4);
    expect(scans[3]?.cursor).toBe('42');
    expect(loadMore(second)).toBeUndefined();
    expect(stopButton(second)).toBeDefined();
    expect(second.text()).toContain('Scanning');

    scans[3]?.resolve({ cursor: '0', keys: [key('new:2')] });
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

describe('KeyBrowser scan batches', () => {
  it('keeps scanning on one click until COUNT keys are found, then pauses', async () => {
    scans.length = 0;
    moduleKeyTypes = [];
    await useWorkspace().refresh();
    const tabs = useTabs();
    const opened = tabs.openKeysTab(profile);
    await settle();
    const tab = (): KeysTab => tabs.tabs.find((t) => t.id === opened.id) as KeysTab;
    const wrapper = mount(KeyBrowser, { props: { tab: tab() }, attachTo: document.body });
    await settle();
    // A selective MATCH: the first steps find nothing, then COUNT (1000) keys arrive at once.
    scans[0]?.resolve({ cursor: '10', keys: [] });
    await settle();
    scans[1]?.resolve({ cursor: '20', keys: [] });
    await settle();
    expect(scans.map((s) => s.cursor)).toEqual(['0', '10', '20']);
    scans[2]?.resolve({ cursor: '30', keys: Array.from({ length: 1000 }, (_, i) => key(`user:42:${i}`)) });
    await settle();
    // COUNT keys were found: the batch pauses with the cursor kept for Load more.
    expect(scans).toHaveLength(3);
    expect(tab().index.size).toBe(1000);
    expect(tab().cursor).toBe('30');
    expect(wrapper.text()).not.toContain('Scanning');
    expect(loadMore(wrapper)).toBeDefined();
    wrapper.unmount();
    await tabs.closeTab(opened.id);
  });
});

describe('KeyBrowser unsaved key edits', () => {
  it('asks before another key, a new scan or the selection change drops staged edits', async () => {
    scans.length = 0;
    moduleKeyTypes = [];
    await useWorkspace().refresh();
    const tabs = useTabs();
    const dialogs = useDialogs();
    const opened = tabs.openKeysTab(profile);
    await settle();
    const tab = (): KeysTab => tabs.tabs.find((t) => t.id === opened.id) as KeysTab;
    const wrapper = mount(KeyBrowser, { props: { tab: tab() }, attachTo: document.body });
    await settle();
    scans[0]?.resolve({ cursor: '0', keys: [key('a'), key('b')] });
    await settle();

    // Open key a and stage a field change.
    await row(wrapper, 'a')?.trigger('click');
    await settle();
    const first = tab().selected;
    expect(first).toBe(`key:${key('a').name}`);
    await wrapper.find('button[aria-label="Edit name"]').trigger('click');
    const input = wrapper.find('input.inline');
    await input.setValue('Sam');
    await input.trigger('keydown', { key: 'Enter' });
    await settle();
    expect(tab().unsaved).toBe(true);

    // Another key: asked, and "no" keeps the key and its edit.
    await row(wrapper, 'b')?.trigger('click');
    await settle();
    expect(dialogs.dialogs).toHaveLength(1);
    expect(dialogs.dialogs[0]?.spec.type).toBe('confirm');
    dialogs.close(dialogs.dialogs[0]?.id ?? 0, false);
    await settle();
    expect(tab().selected).toBe(first);
    expect(wrapper.find('.savebar').text()).toContain('1 change');

    // Scan: asked as well, and "no" starts no scan.
    await wrapper.find('form.filters').trigger('submit');
    await settle();
    expect(dialogs.dialogs).toHaveLength(1);
    dialogs.close(dialogs.dialogs[0]?.id ?? 0, false);
    await settle();
    expect(scans).toHaveLength(1);
    expect(tab().selected).toBe(first);

    // "Discard" opens the other key; the tab has no unsaved work any more.
    await row(wrapper, 'b')?.trigger('click');
    await settle();
    dialogs.close(dialogs.dialogs[0]?.id ?? 0, true);
    await settle();
    expect(tab().selected).toBe(`key:${key('b').name}`);
    expect(tab().unsaved).toBe(false);
    expect(wrapper.find('.savebar').exists()).toBe(false);

    // Without staged edits nothing is asked.
    await row(wrapper, 'a')?.trigger('click');
    await settle();
    expect(dialogs.dialogs).toHaveLength(0);
    expect(tab().selected).toBe(first);
    wrapper.unmount();
    await tabs.closeTab(opened.id);
  });

  it('clears the unsaved mark when the viewer with staged edits goes away', async () => {
    scans.length = 0;
    await useWorkspace().refresh();
    const tabs = useTabs();
    const dialogs = useDialogs();
    const opened = tabs.openKeysTab(profile);
    await settle();
    const tab = (): KeysTab => tabs.tabs.find((t) => t.id === opened.id) as KeysTab;
    const wrapper = mount(KeyBrowser, { props: { tab: tab() }, attachTo: document.body });
    await settle();
    scans[0]?.resolve({ cursor: '0', keys: [key('a')] });
    await settle();
    await row(wrapper, 'a')?.trigger('click');
    await settle();
    await wrapper.find('button[aria-label="Edit name"]').trigger('click');
    await wrapper.find('input.inline').setValue('Sam');
    await wrapper.find('input.inline').trigger('keydown', { key: 'Enter' });
    await settle();
    expect(tab().unsaved).toBe(true);

    // Scanning again after "Discard" clears the selection: the viewer unmounts and the mark goes.
    await wrapper.find('form.filters').trigger('submit');
    await settle();
    dialogs.close(dialogs.dialogs[0]?.id ?? 0, true);
    await settle();
    expect(scans).toHaveLength(2);
    expect(tab().selected).toBeNull();
    expect(tab().unsaved).toBe(false);
    scans[1]?.resolve({ cursor: '0', keys: [] });
    await settle();
    wrapper.unmount();
    await tabs.closeTab(opened.id);
  });
});

const lostConnection = {
  kind: 'session',
  error: { kind: 'driver', error: { kind: 'connection_lost', message: 'gone', code: null, detail: null, hint: null, position: null } },
};
const multiOpen = {
  kind: 'session',
  error: { kind: 'driver', error: { kind: 'query', message: 'A MULTI transaction is open in the console', code: 'MULTI_OPEN', detail: null, hint: null, position: null } },
};

/** Opens a key browser on a fresh `s1` session. */
async function browser() {
  scans.length = 0;
  moduleKeyTypes = [];
  nextSession = 's1';
  digest = 'd1';
  await useWorkspace().refresh();
  const tabs = useTabs();
  const opened = tabs.openKeysTab(profile);
  await settle();
  const tab = (): KeysTab => tabs.tabs.find((t) => t.id === opened.id) as KeysTab;
  const wrapper = mount(KeyBrowser, { props: { tab: tab() }, attachTo: document.body });
  await settle();
  return { tabs, tab, wrapper, id: opened.id };
}

/** Opens key `label` and stages a change of its `name` field. */
async function stageEdit(wrapper: ReturnType<typeof mount>, label: string): Promise<void> {
  await row(wrapper, label)?.trigger('click');
  await settle();
  await wrapper.find('button[aria-label="Edit name"]').trigger('click');
  await wrapper.find('input.inline').setValue('Sam');
  await wrapper.find('input.inline').trigger('keydown', { key: 'Enter' });
  await settle();
}

describe('KeyBrowser session loss', () => {
  it('keeps the open key and its staged edits through a lost session and a reconnect', async () => {
    const { tabs, tab, wrapper, id } = await browser();
    scans[0]?.resolve({ cursor: '0', keys: [key('a'), key('b')] });
    await settle();
    await stageEdit(wrapper, 'a');
    expect(tab().unsaved).toBe(true);

    // The connection drops: the viewer stays (not usable), with the edit and the unsaved mark.
    tabs.reportTabError(id, lostConnection);
    await settle();
    expect(tab().session).toBeNull();
    expect(wrapper.find('.savebar').text()).toContain('1 change');
    expect(wrapper.find('.detail').classes()).toContain('waiting');
    expect(tab().unsaved).toBe(true);

    // Connect again: a new session scans from the start, and the key is selected again with its edit.
    nextSession = 's2';
    await tabs.reconnect(id);
    await settle();
    expect(tab().session?.id).toBe('s2');
    expect(scans.at(-1)?.session).toBe('s2');
    expect(scans.at(-1)?.cursor).toBe('0');
    expect(tab().selected).toBe(`key:${key('a').name}`);
    expect(wrapper.find('.savebar').text()).toContain('1 change');
    expect(wrapper.find('.detail').classes()).not.toContain('waiting');
    expect(tab().unsaved).toBe(true);
    // The key is what it was: nothing to warn about.
    expect(wrapper.text()).not.toContain('changed on the server');

    // The scan's own answer holds the key too: it is listed once.
    scans.at(-1)?.resolve({ cursor: '0', keys: [key('a'), key('b')] });
    await settle();
    expect(tab().index.size).toBe(2);
    expect(tab().selected).toBe(`key:${key('a').name}`);
    expect(wrapper.find('.savebar').text()).toContain('1 change');
    wrapper.unmount();
    await tabs.closeTab(id);
  });

  it('says so when the key changed on the server while the tab was not connected', async () => {
    const { tabs, tab, wrapper, id } = await browser();
    scans[0]?.resolve({ cursor: '0', keys: [key('a')] });
    await settle();
    await stageEdit(wrapper, 'a');

    tabs.reportTabError(id, lostConnection);
    await settle();
    nextSession = 's2';
    digest = 'd2';
    await tabs.reconnect(id);
    await settle();
    expect(wrapper.find('.savebar').text()).toContain('1 change');
    expect(wrapper.text()).toContain('The key changed on the server');
    expect(tab().unsaved).toBe(true);
    scans.at(-1)?.resolve({ cursor: '0', keys: [] });
    await settle();
    wrapper.unmount();
    await tabs.closeTab(id);
  });

  it('ends "Scanning" when the running scan was reset from outside and its answer arrives', async () => {
    const { tabs, tab, wrapper, id } = await browser();
    expect(stopButton(wrapper)).toBeDefined();
    // A newer scan of this browser owns the state: the older answer does not end it.
    await wrapper.find('form.filters').trigger('submit');
    await settle();
    expect(scans).toHaveLength(2);
    scans[0]?.resolve({ cursor: '0', keys: [key('old')] });
    await settle();
    expect(stopButton(wrapper)).toBeDefined();

    // The tab's scan is reset elsewhere (a reconnect or database switch bumps the token) and nothing scans now.
    tab().scanToken += 1;
    scans[1]?.resolve({ cursor: '0', keys: [key('late')] });
    await settle();
    expect(tab().index.size).toBe(0);
    expect(stopButton(wrapper)).toBeUndefined();
    expect(wrapper.text()).not.toContain('Scanning');
    wrapper.unmount();
    await tabs.closeTab(id);
  });
});

describe('KeyBrowser scan retry', () => {
  it('continues from the cursor the batch failed at and keeps the keys and the open key', async () => {
    const { tabs, tab, wrapper, id } = await browser();
    // Step 1 finds a key (fewer than COUNT, so the batch goes on); step 2 is refused.
    scans[0]?.resolve({ cursor: '40', keys: [key('a')] });
    await settle();
    expect(scans.map((s) => s.cursor)).toEqual(['0', '40']);
    await stageEdit(wrapper, 'a');
    scans[1]?.reject(multiOpen);
    await settle();
    expect(wrapper.text()).toContain('MULTI open in the console');
    expect(tab().index.size).toBe(1);

    const retry = wrapper.findAll('button').find((b) => b.text() === 'Try again');
    await retry?.trigger('click');
    await settle();
    // No question, no restart: the same cursor again, the key and its edit untouched.
    expect(useDialogs().dialogs).toHaveLength(0);
    expect(scans.map((s) => s.cursor)).toEqual(['0', '40', '40']);
    expect(tab().index.size).toBe(1);
    expect(tab().selected).toBe(`key:${key('a').name}`);
    expect(wrapper.find('.savebar').text()).toContain('1 change');
    scans[2]?.resolve({ cursor: '0', keys: [key('b')] });
    await settle();
    expect(tab().index.size).toBe(2);
    expect(wrapper.text()).toContain('SCAN complete');
    wrapper.unmount();
    await tabs.closeTab(id);
  });

  it('starts again from 0 only when the first step failed', async () => {
    const { tabs, wrapper, id } = await browser();
    scans[0]?.reject(multiOpen);
    await settle();
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Try again')
      ?.trigger('click');
    await settle();
    expect(scans.map((s) => s.cursor)).toEqual(['0', '0']);
    scans[1]?.resolve({ cursor: '0', keys: [] });
    await settle();
    wrapper.unmount();
    await tabs.closeTab(id);
  });
});
