/**
 * The materialized view editor's way to its indexes: without a unique index
 * REFRESH CONCURRENTLY is refused, and "Design indexes" opens the table
 * designer on the view's indexes.
 */
import { flushPromises, mount } from '@vue/test-utils';
import type { ObjectRef, PgMaterializedView } from '../../api/ddl';
import type { SessionInfo } from '../../api/sessions';
import { pendingFocus } from '../table/designerIntent';
import MatViewEditor from './MatViewEditor.vue';

const opened: unknown[][] = [];
let view: PgMaterializedView | null = null;
const profile = { id: 'c1', name: 'Shop', engine: 'postgres' };

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async (command: string) => {
    if (command === 'catalog_describe') return { engine: 'postgres', object: { kind: 'materializedView', ...view } };
    if (command === 'activity_query') return { events: [], total: 0, nextCursor: null };
    throw new Error(`unexpected command ${command}`);
  }),
}));
vi.mock('../../api/activity', () => ({ fetchActivity: vi.fn(async () => ({ events: [] })) }));
vi.mock('../../composables/useTabs', () => ({
  useTabs: () => ({
    openTableDesigner: (...args: unknown[]) => {
      opened.push(args);
      return { id: 'tab-mv' };
    },
  }),
}));
vi.mock('../../composables/useWorkspace', async () => {
  const { ref: vueRef } = await import('vue');
  const records = vueRef({ connections: [], folders: [], queries: [] });
  return { useWorkspace: () => ({ records, connection: () => profile, refresh: vi.fn(async () => undefined) }) };
});

function session(): SessionInfo {
  return {
    id: 's1',
    connectionId: 'c1',
    engine: 'postgres',
    server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 },
    environment: 'development',
    database: 'shop',
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
}

function matView(indexes: PgMaterializedView['indexes']): PgMaterializedView {
  return {
    schema: 'rep',
    name: 'totals',
    owner: 'app',
    definition: 'SELECT day FROM rep.days',
    populated: true,
    accessMethod: 'heap',
    tablespace: null,
    storageParameters: [],
    rowEstimate: null,
    totalBytes: null,
    sizeNote: null,
    lastRefreshNote: '',
    refreshConcurrently: indexes.length > 0,
    comment: null,
    system: false,
    columns: [],
    indexes,
  } as unknown as PgMaterializedView;
}

const object: ObjectRef = { kind: 'materializedView', database: 'shop', schema: 'rep', parent: null, name: 'totals', signature: null };

async function mountEditor() {
  const wrapper = mount(MatViewEditor, {
    props: { session: session(), connectionName: 'Shop', object },
    global: { stubs: { SqlCode: true, DdlView: true } },
  });
  await flushPromises();
  return wrapper;
}

beforeEach(() => {
  opened.length = 0;
});

describe('MatViewEditor indexes', () => {
  it('offers Design indexes next to the concurrent refresh hint when no unique index exists, and opens the designer on the indexes', async () => {
    view = matView([]);
    const wrapper = await mountEditor();
    expect(wrapper.text()).toContain('Concurrent refresh needs a unique index');
    const button = wrapper.findAll('button').find((b) => b.text() === 'Design indexes');
    expect(button).toBeDefined();
    await button?.trigger('click');
    expect(opened).toEqual([[profile, object, { database: 'shop', schema: 'rep', parent: null }]]);
    expect(pendingFocus('tab-mv')).toMatchObject({ section: 'indexes', name: null });
    wrapper.unmount();
  });

  it('shows no hint button once a unique index on plain columns exists', async () => {
    view = matView([
      { name: 'totals_day', unique: true, valid: true, predicate: null, keys: [{ expression: false }], sizeBytes: null, definition: 'CREATE UNIQUE INDEX totals_day ON rep.totals (day)' },
    ] as unknown as PgMaterializedView['indexes']);
    const wrapper = await mountEditor();
    expect(wrapper.findAll('button').some((b) => b.text() === 'Design indexes')).toBe(false);
    wrapper.unmount();
  });
});
