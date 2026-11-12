/* eslint-disable vue/one-component-per-file -- the test builds a small shell around the view */
import { flushPromises, mount } from '@vue/test-utils';
import { defineComponent, h, KeepAlive, ref } from 'vue';
import type * as RecordsModule from '../api/records';
import type * as SessionsModule from '../api/sessions';
import type { ObjectRef } from '../api/catalog';
import type { ConnectionProfile } from '../api/records';
import type { SessionInfo } from '../api/sessions';
import { useTabs } from '../composables/useTabs';
import { useWorkspace } from '../composables/useWorkspace';
import WorkspaceView from './WorkspaceView.vue';

const profile = {
  id: 'p1',
  name: 'Orders',
  engine: 'postgres',
  environment: 'development',
  folderId: null,
  database: 'shop',
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { redisDb: 0 },
  savedSecrets: [],
} as unknown as ConnectionProfile;

/** Mounts and unmounts of the stubbed table data view, per tab id. */
const views = vi.hoisted(() => ({ mounted: [] as string[], unmounted: [] as string[] }));

let sessions = 0;
function session(database: string | null): SessionInfo {
  sessions += 1;
  return {
    id: `s${sessions}`,
    connectionId: 'p1',
    engine: 'postgres',
    server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 },
    environment: 'development',
    database,
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
}

vi.mock('../api/sessions', async (original) => ({
  ...(await original<typeof SessionsModule>()),
  openSession: vi.fn(async () => session(null)),
  openDatabaseSession: vi.fn(async (_id: string, database: string) => session(database)),
  closeSession: vi.fn(async () => undefined),
  cancel: vi.fn(async () => undefined),
}));
vi.mock('../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [profile], queries: [] })),
}));
vi.mock('../components/tree/objectBrowser', () => ({
  useObjectBrowser: () => ({ refresh: vi.fn(), refreshObject: vi.fn(), refreshContainer: vi.fn() }),
}));

/** A view with local state (a text field), reporting whether its tab is on screen. */
vi.mock('../components/table/TableDataView.vue', async () => {
  const vue = await import('vue');
  const { useTabVisible } = await import('../composables/useTabs');
  return {
    default: vue.defineComponent({
      props: { tab: { type: Object, required: true } },
      setup(props) {
        const id = (props.tab as { id: string }).id;
        const text = vue.ref('');
        const visible = useTabVisible();
        vue.onMounted(() => views.mounted.push(id));
        vue.onBeforeUnmount(() => views.unmounted.push(id));
        return () =>
          vue.h('div', { class: 'data-view', 'data-visible': String(visible.value) }, [
            vue.h('input', {
              value: text.value,
              onInput: (event: Event) => {
                text.value = (event.target as HTMLInputElement).value;
              },
            }),
          ]);
      },
    }),
  };
});
const stub = vi.hoisted(() => async (): Promise<{ default: unknown }> => {
  const vue = await import('vue');
  return { default: vue.defineComponent({ render: () => null }) };
});
vi.mock('../components/query/QueryTabView.vue', stub);
vi.mock('../components/redis/KeyBrowser.vue', stub);
vi.mock('../components/overview/OverviewTabView.vue', stub);
vi.mock('../components/notices/NoticeHost.vue', stub);
vi.mock('../components/notices/InitCommandNotice.vue', stub);
vi.mock('../components/ExtensionSlot.vue', stub);
vi.mock('./HomeView.vue', stub);

const orders: ObjectRef = { kind: 'table', database: 'shop', schema: 'public', parent: null, name: 'orders', signature: null };
const items: ObjectRef = { ...orders, name: 'items' };

/** WorkspaceView behind a KeepAlive like AppShell's; `page` switches to another page and back. */
function mountShell() {
  const page = ref<'workspace' | 'activity'>('workspace');
  const Activity = defineComponent({ name: 'ActivityView', render: () => h('p', 'Activity') });
  const Shell = defineComponent({
    setup: () => () => h(KeepAlive, { include: 'WorkspaceView' }, [page.value === 'workspace' ? h(WorkspaceView) : h(Activity)]),
  });
  return { wrapper: mount(Shell, { attachTo: document.body }), page };
}

describe('workspace tabs', () => {
  beforeAll(async () => {
    await useWorkspace().refresh();
  });

  afterEach(async () => {
    const tabs = useTabs();
    for (const tab of [...tabs.tabs]) await tabs.closeTab(tab.id);
    views.mounted.length = 0;
    views.unmounted.length = 0;
  });

  it('keeps every opened tab view mounted and hides the inactive ones', async () => {
    const tabs = useTabs();
    const a = tabs.openTableDataTab(profile, orders);
    const b = tabs.openTableDataTab(profile, items);
    const { wrapper } = mountShell();
    await flushPromises();
    // Only the active tab was shown, so only its view is mounted.
    expect(views.mounted).toEqual([b.id]);

    tabs.activate(a.id);
    await flushPromises();
    const panelA = wrapper.get(`#wpanel-${a.id}`);
    const panelB = wrapper.get(`#wpanel-${b.id}`);
    await panelA.get('input').setValue('typed, not applied');
    expect(panelA.get('.data-view').attributes('data-visible')).toBe('true');

    tabs.activate(b.id);
    await flushPromises();
    expect((panelA.element as HTMLElement).style.display).toBe('none');
    expect((panelB.element as HTMLElement).style.display).toBe('');
    expect(panelA.get('.data-view').attributes('data-visible')).toBe('false');

    tabs.activate(a.id);
    await flushPromises();
    expect((panelA.get('input').element as HTMLInputElement).value).toBe('typed, not applied');
    expect(views.mounted).toEqual([b.id, a.id]);
    expect(views.unmounted).toEqual([]);

    // Each tab controls its own panel.
    for (const tab of [a, b]) {
      const panel = wrapper.get(`#wpanel-${tab.id}`);
      expect(panel.attributes('role')).toBe('tabpanel');
      expect(panel.attributes('aria-labelledby')).toBe(`wtab-${tab.id}`);
      expect(wrapper.get(`#wtab-${tab.id}`).attributes('aria-controls')).toBe(`wpanel-${tab.id}`);
    }

    // Closing a tab, also one that is not active, unmounts its view.
    await tabs.closeTab(b.id);
    await flushPromises();
    expect(views.unmounted).toEqual([b.id]);
    expect(wrapper.find(`#wpanel-${b.id}`).exists()).toBe(false);
    wrapper.unmount();
  });

  it('says in tooltips what a tab and its marks stand for', async () => {
    const tabs = useTabs();
    const a = tabs.openTableDataTab(profile, orders);
    const { wrapper } = mountShell();
    await flushPromises();
    const tab = wrapper.get(`#wtab-${a.id}`).element.parentElement as HTMLElement;
    // The full title (the strip cuts long ones short), then the connection and the environment of the coloured edge.
    expect(tab.getAttribute('title')).toBe(`${a.title}\nOrders · Development environment`);
    // Close Tab has a shortcut only in the macOS menu bar; the test document is not macOS.
    expect(tab.querySelector('.close')?.getAttribute('title')).toBe('Close tab');
    expect(tab.querySelector('.dirty')).toBeNull();

    tabs.setUnsaved(a.id, true);
    await flushPromises();
    expect(tab.querySelector('.dirty')?.getAttribute('title')).toBe('Unsaved changes');
    wrapper.unmount();
  });

  it('keeps the tabs while another page is shown and mounts tabs opened meanwhile on return', async () => {
    const tabs = useTabs();
    const a = tabs.openTableDataTab(profile, orders);
    const { wrapper, page } = mountShell();
    await flushPromises();
    await wrapper.get(`#wpanel-${a.id} input`).setValue('staged');
    expect(tabs.isTabVisible(a.id)).toBe(true);

    page.value = 'activity';
    await flushPromises();
    expect(tabs.isTabVisible(a.id)).toBe(false);
    const b = tabs.openTableDataTab(profile, items);
    await flushPromises();
    // Not mounted while the workspace is cached away (detached from the document).
    expect(views.mounted).toEqual([a.id]);

    page.value = 'workspace';
    await flushPromises();
    expect(views.mounted).toEqual([a.id, b.id]);
    expect(views.unmounted).toEqual([]);
    expect(tabs.isTabVisible(b.id)).toBe(true);
    tabs.activate(a.id);
    await flushPromises();
    expect((wrapper.get(`#wpanel-${a.id} input`).element as HTMLInputElement).value).toBe('staged');
    wrapper.unmount();
  });

  it('takes the keyboard focus out of a tab that is hidden', async () => {
    const tabs = useTabs();
    const a = tabs.openTableDataTab(profile, orders);
    const b = tabs.openTableDataTab(profile, items);
    const { wrapper } = mountShell();
    await flushPromises();
    tabs.activate(a.id);
    await flushPromises();
    const inputA = wrapper.get(`#wpanel-${a.id} input`).element as HTMLInputElement;
    inputA.focus();
    expect(document.activeElement).toBe(inputA);

    // Another tab is shown (a click on its label does not focus a button in every webview).
    tabs.activate(b.id);
    await flushPromises();
    expect(document.activeElement).toBe(wrapper.get(`#wtab-${b.id}`).element);

    // Focus inside the tab that is shown is left alone.
    const inputB = wrapper.get(`#wpanel-${b.id} input`).element as HTMLInputElement;
    inputB.focus();
    tabs.setUnsaved(b.id, true);
    await flushPromises();
    expect(document.activeElement).toBe(inputB);

    // The view is hidden behind the connection state while its tab connects again.
    const reconnecting = tabs.reconnect(b.id);
    await wrapper.vm.$nextTick();
    await wrapper.vm.$nextTick();
    expect(document.activeElement).toBe(wrapper.get(`#wtab-${b.id}`).element);
    await reconnecting;
    await flushPromises();
    wrapper.unmount();
  });

  it('keeps a view mounted, hidden behind the connection state, while its tab reconnects', async () => {
    const tabs = useTabs();
    const a = tabs.openTableDataTab(profile, orders);
    const { wrapper } = mountShell();
    await flushPromises();
    const panel = wrapper.get(`#wpanel-${a.id}`);
    await panel.get('input').setValue('staged');

    const reconnecting = tabs.reconnect(a.id);
    await wrapper.vm.$nextTick();
    expect(panel.text()).toContain('Connecting to Orders');
    expect((panel.get('.view-body').element as HTMLElement).style.display).toBe('none');
    expect(tabs.isTabVisible(a.id)).toBe(false);
    await reconnecting;
    await flushPromises();

    expect((panel.get('.view-body').element as HTMLElement).style.display).toBe('');
    expect((panel.get('input').element as HTMLInputElement).value).toBe('staged');
    expect(views.mounted).toEqual([a.id]);
    expect(views.unmounted).toEqual([]);
    wrapper.unmount();
  });

  it('offers Connect again on a tab whose connection was closed and keeps its view for after', async () => {
    const tabs = useTabs();
    const a = tabs.openTableDataTab(profile, orders);
    const { wrapper } = mountShell();
    await flushPromises();
    const panel = wrapper.get(`#wpanel-${a.id}`);
    await panel.get('input').setValue('staged');

    await tabs.disconnectConnection(profile.id);
    await flushPromises();
    expect(panel.text()).toContain('Not connected.');
    expect((panel.get('.view-body').element as HTMLElement).style.display).toBe('none');
    const connectAgain = panel.findAll('button').find((b) => b.text() === 'Connect again');
    expect(connectAgain).toBeDefined();

    await connectAgain?.trigger('click');
    await flushPromises();
    expect(tabs.tabs.find((t) => t.id === a.id)?.status).toBe('ready');
    expect(panel.text()).not.toContain('Not connected.');
    expect((panel.get('.view-body').element as HTMLElement).style.display).toBe('');
    expect((panel.get('input').element as HTMLInputElement).value).toBe('staged');
    expect(views.unmounted).toEqual([]);
    wrapper.unmount();
  });
});
