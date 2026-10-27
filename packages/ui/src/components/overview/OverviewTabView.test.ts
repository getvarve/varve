import { flushPromises, mount } from '@vue/test-utils';
import type * as RecordsModule from '../../api/records';
import type * as SessionsModule from '../../api/sessions';
import type { Container, KindCount, ObjectKind, ObjectSummary } from '../../api/catalog';
import type { ConnectionProfile } from '../../api/records';
import type { SessionInfo } from '../../api/sessions';
import { useTabs } from '../../composables/useTabs';
import type { OverviewTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import OverviewTabView from './OverviewTabView.vue';

function profile(id: string, engine: ConnectionProfile['engine']): ConnectionProfile {
  return {
    id,
    name: id,
    engine,
    environment: 'development',
    folderId: null,
    database: null,
    ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
    options: { redisDb: 0, showSystemObjects: false },
    savedSecrets: [],
  } as unknown as ConnectionProfile;
}

const pgProfile = profile('pg', 'postgres');
const myProfile = profile('my', 'mysql');
let sessions = 0;

function session(connectionId: string): SessionInfo {
  sessions += 1;
  const engine = connectionId === 'pg' ? 'postgres' : 'mysql';
  return {
    id: `s${sessions}`,
    connectionId,
    engine,
    server: { engine, version: '1', major: 1, minor: 0, patch: 0 },
    environment: 'development',
    database: connectionId === 'pg' ? 'shop' : 'crm',
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
}

function summary(kind: ObjectKind, name: string): ObjectSummary {
  return {
    object: { kind, database: 'shop', schema: 'public', parent: null, name, signature: null },
    comment: null,
    detail: null,
    flags: [],
    hasChildren: false,
    system: false,
    rowEstimate: null,
    sizeBytes: null,
  };
}

/** What the server has: 60 tables and, until `viewsGone`, one view. */
let viewsGone = false;
const catalogKinds = vi.fn(async (): Promise<KindCount[]> => [
  { kind: 'table', count: 60 },
  ...(viewsGone ? [] : [{ kind: 'view' as const, count: 1 }]),
]);
const catalogList = vi.fn(async (_s: string, c: Container, kind: ObjectKind): Promise<ObjectSummary[]> => {
  const prefix = c.schema ?? c.database ?? '';
  if (kind === 'view') return [summary('view', `${prefix}_v`)];
  return Array.from({ length: 60 }, (_, i) => summary('table', `${prefix}_t${String(i).padStart(2, '0')}`));
});

vi.mock('../../api/catalog', async (original) => ({
  ...(await original<object>()),
  catalogCapabilities: vi.fn(async () => ({ engine: 'postgres', kinds: [], editable: [], features: [] })),
  catalogSchemas: vi.fn(async () => [{ ...summary('schema', 'public') }, { ...summary('schema', 'sales') }]),
  catalogDatabases: vi.fn(async () => [{ ...summary('database', 'crm') }, { ...summary('database', 'shop') }]),
  catalogKinds: () => catalogKinds(),
  catalogList: (s: string, c: Container, k: ObjectKind) => catalogList(s, c, k),
}));
vi.mock('../../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: async (id: string) => session(id),
  openDatabaseSession: async (id: string) => session(id),
  closeSession: vi.fn(async () => undefined),
}));
vi.mock('../../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [pgProfile, myProfile], queries: [] })),
}));
vi.mock('../../composables/useConnect', () => ({
  withConnectPrompts: <T>(_p: unknown, run: (secrets: object) => Promise<T>) => run({}),
}));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));
async function settle(): Promise<void> {
  for (let i = 0; i < 6; i += 1) {
    await tick();
    await flushPromises();
  }
}

async function openTab(p: ConnectionProfile): Promise<OverviewTab> {
  const tabs = useTabs();
  const tab = tabs.openOverviewTab(p);
  await settle();
  const ready = tabs.tabs.find((t) => t.id === tab.id);
  if (ready?.kind !== 'overview' || !ready.session) throw new Error('the overview tab did not connect');
  return ready;
}

describe('OverviewTabView', () => {
  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  beforeEach(async () => {
    const tabs = useTabs();
    for (const t of [...tabs.tabs]) await tabs.closeTab(t.id);
    viewsGone = false;
    document.body.innerHTML = '';
  });

  it('never scrolls on focus, and a click makes the clicked row active', async () => {
    const tab = await openTab(pgProfile);
    const wrapper = mount(OverviewTabView, { props: { tab }, attachTo: document.body });
    await settle();
    const grid = wrapper.get('[role="grid"]');
    const body = wrapper.get('.body').element as HTMLElement;
    body.scrollTop = 28 * 20;
    await wrapper.get('.body').trigger('scroll');
    await grid.trigger('focus');
    expect(body.scrollTop).toBe(28 * 20);
    expect(grid.attributes('aria-activedescendant')).toBeUndefined();
    const row = wrapper.findAll('[role="row"]').find((r) => r.text().includes('public_t25'));
    await row?.trigger('click');
    expect(body.scrollTop).toBe(28 * 20);
    expect(wrapper.get('[aria-selected="true"]').text()).toContain('public_t25');
    wrapper.unmount();
  });

  it('starts keyboard navigation at the first row in view, not at the top', async () => {
    const tab = await openTab(pgProfile);
    const wrapper = mount(OverviewTabView, { props: { tab }, attachTo: document.body });
    await settle();
    const body = wrapper.get('.body').element as HTMLElement;
    body.scrollTop = 28 * 30;
    await wrapper.get('.body').trigger('scroll');
    await wrapper.get('[role="grid"]').trigger('keydown', { key: 'ArrowDown' });
    expect(wrapper.get('[aria-selected="true"]').text()).toContain('public_t30');
    wrapper.unmount();
  });

  it('puts focus and the active row reference on the labelled grid, only for rendered rows', async () => {
    const tab = await openTab(pgProfile);
    const wrapper = mount(OverviewTabView, { props: { tab }, attachTo: document.body });
    await settle();
    const grid = wrapper.get('[role="grid"]');
    expect(grid.attributes('tabindex')).toBe('0');
    expect(grid.attributes('aria-label')).toBe('Objects in public');
    expect(wrapper.get('.body').attributes('tabindex')).toBeUndefined();
    await grid.trigger('keydown', { key: 'End' });
    const id = grid.attributes('aria-activedescendant');
    expect(id).toBeDefined();
    expect(document.getElementById(id ?? '')).not.toBeNull();
    wrapper.unmount();
  });

  it('drops a kind filter whose kind left the list and always offers to show all', async () => {
    const tab = await openTab(pgProfile);
    const wrapper = mount(OverviewTabView, { props: { tab }, attachTo: document.body });
    await settle();
    const viewChip = wrapper.findAll('.chip').find((c) => c.text().startsWith('Views'));
    await viewChip?.trigger('click');
    expect(wrapper.findAll('[role="row"]').length).toBe(2);
    expect(wrapper.find('.chip.clear').exists()).toBe(true);
    viewsGone = true;
    await wrapper.findAll('button').find((b) => b.text().includes('Refresh'))?.trigger('click');
    await settle();
    // Only tables are left: the view filter no longer applies, so the tables show.
    expect(wrapper.text()).not.toContain('Nothing matches');
    expect(wrapper.findAll('[role="row"]').length).toBeGreaterThan(2);
    wrapper.unmount();
  });

  it('keeps the state of each tab when switching away and back, without loading again', async () => {
    const pgTab = await openTab(pgProfile);
    const myTab = await openTab(myProfile);
    const first = mount(OverviewTabView, { props: { tab: pgTab }, attachTo: document.body });
    await settle();
    await first.get('input[type="search"]').setValue('t0');
    await first.findAll('button.sort')[0]?.trigger('click');
    const row = first.findAll('[role="row"]').find((r) => r.text().includes('public_t05'));
    await row?.trigger('click');
    const body = first.get('.body').element as HTMLElement;
    body.scrollTop = 56;
    await first.get('.body').trigger('scroll');
    // The context menu opens on (and selects) the same row.
    await row?.trigger('contextmenu');
    expect(document.querySelector('[role="menu"]')).not.toBeNull();
    first.unmount();
    // Leaving the tab closes its menu.
    expect(document.querySelector('[role="menu"]')).toBeNull();
    const kindsCalls = catalogKinds.mock.calls.length;

    // Another tab starts clean and shows its own connection.
    const other = mount(OverviewTabView, { props: { tab: myTab }, attachTo: document.body });
    await settle();
    expect((other.get('input[type="search"]').element as HTMLInputElement).value).toBe('');
    expect(other.findAll('option').map((o) => o.text())).toEqual(['crm', 'shop']);
    expect(other.text()).toContain('crm_t00');
    expect(other.text()).not.toContain('public_t00');
    other.unmount();
    const afterOther = catalogKinds.mock.calls.length;
    expect(afterOther).toBe(kindsCalls + 1);

    // Back on the first tab: everything as it was, nothing loaded again.
    const again = mount(OverviewTabView, { props: { tab: pgTab }, attachTo: document.body });
    await settle();
    expect((again.get('input[type="search"]').element as HTMLInputElement).value).toBe('t0');
    expect(again.get('[aria-sort="ascending"]').text()).toContain('Name');
    expect(again.get('[aria-selected="true"]').text()).toContain('public_t05');
    expect((again.get('.body').element as HTMLElement).scrollTop).toBe(56);
    expect(catalogKinds.mock.calls.length).toBe(afterOther);
    again.unmount();
  });

  it('leaves keys on the sort buttons and Show all to those buttons', async () => {
    const tab = await openTab(pgProfile);
    const wrapper = mount(OverviewTabView, { props: { tab }, attachTo: document.body });
    await settle();
    const grid = wrapper.get('[role="grid"]');
    await grid.trigger('keydown', { key: 'Home' });
    expect(wrapper.find('[aria-selected="true"]').exists()).toBe(true);
    const enter = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
    wrapper.get('button.sort').element.dispatchEvent(enter);
    expect(enter.defaultPrevented).toBe(false);
    const end = new KeyboardEvent('keydown', { key: 'End', bubbles: true, cancelable: true });
    wrapper.get('button.sort').element.dispatchEvent(end);
    expect(end.defaultPrevented).toBe(false);
    expect(wrapper.get('[aria-selected="true"]').text()).toContain('public_t00');
    await wrapper.get('input[type="search"]').setValue('nothing like this');
    const showAll = wrapper.get('.empty button');
    // The empty state is outside the grid, so its button is never a grid key.
    expect(showAll.element.closest('[role="grid"]')).toBeNull();
    const again = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
    showAll.element.dispatchEvent(again);
    expect(again.defaultPrevented).toBe(false);
    wrapper.unmount();
  });

  it('shows the container chosen while the tab was in the background, never the previous one', async () => {
    const tabs = useTabs();
    const tab = await openTab(pgProfile);
    const first = mount(OverviewTabView, { props: { tab }, attachTo: document.body });
    await settle();
    expect(first.text()).toContain('public_t00');
    first.unmount();
    // The tree picks schema sales: the tab comes to the front with the new scope.
    tabs.openOverviewTab(pgProfile, { database: 'shop', schema: 'sales' });
    expect(tab.scope.schema).toBe('sales');
    const again = mount(OverviewTabView, { props: { tab }, attachTo: document.body });
    // Before the list answers, nothing of public shows under the sales label.
    expect(again.text()).not.toContain('public_t00');
    await settle();
    expect(again.text()).toContain('sales_t00');
    expect(again.text()).not.toContain('public_t00');
    const names = (): string[] => again.findAll('.rows [role="row"]').map((r) => r.get('.cell.name').text());
    expect(names().every((n) => n.startsWith('sales_'))).toBe(true);
    // After a reconnect the rows still belong to the container in the picker.
    await tabs.reconnect(tab.id);
    await settle();
    expect(names().length).toBeGreaterThan(0);
    expect(names().every((n) => n.startsWith('sales_'))).toBe(true);
    again.unmount();
  });

  it('explains missing edit actions when the capabilities cannot be read', async () => {
    const catalog = await import('../../api/catalog');
    vi.mocked(catalog.catalogCapabilities).mockRejectedValueOnce({ kind: 'catalog', error: { kind: 'unexpected', message: 'no capabilities' } });
    const tab = await openTab(pgProfile);
    const wrapper = mount(OverviewTabView, { props: { tab }, attachTo: document.body });
    await settle();
    expect(wrapper.text()).toContain('Edit actions are not available: no capabilities');
    expect(wrapper.text()).toContain('public_t00');
    wrapper.unmount();
  });
});

