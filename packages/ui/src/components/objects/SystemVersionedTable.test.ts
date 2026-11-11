/**
 * The system-versioned table view keeps its DDL section across reloads and
 * section switches: edited DDL that has not run, its result and its error
 * are not replaced by "Loading" or an error screen.
 */
import { flushPromises, mount } from '@vue/test-utils';
import type { VueWrapper } from '@vue/test-utils';
import type * as DdlModule from '../../api/ddl';
import type { ObjectRef } from '../../api/ddl';
import type { SessionInfo } from '../../api/sessions';
import SystemVersionedTable from './SystemVersionedTable.vue';

/** What `catalog_describe` answers: the table, or the error it throws. */
let describeAnswer: () => unknown = () => table();
/** Resolves the describe that is waiting (a slow reload). */
let release: (() => void) | null = null;
let slow = false;

vi.mock('../../api/ddl', async (original) => ({
  ...(await original<typeof DdlModule>()),
  catalogDescribe: vi.fn(async () => {
    if (slow) {
      await new Promise<void>((resolve) => {
        release = resolve;
      });
    }
    return describeAnswer();
  }),
}));
vi.mock('../notices/useNotices', () => ({ useNotices: () => ({ refreshRestores: vi.fn(async () => undefined) }) }));

function table() {
  return {
    engine: 'mysql',
    object: { kind: 'table', database: 'shop', name: 'orders', columns: [], systemVersioning: null, rowEstimate: null },
  };
}

const session = {
  id: 's1',
  connectionId: 'c1',
  engine: 'mariadb',
  server: { engine: 'mariadb', version: '12.0', major: 12, minor: 0, patch: 0 },
  environment: 'development',
  database: 'shop',
  readOnly: false,
  confirmDestructive: false,
  purpose: 'user',
  openedAt: 0,
  encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
} as unknown as SessionInfo;
const object: ObjectRef = { kind: 'table', database: 'shop', schema: null, parent: null, name: 'orders', signature: null };

function button(wrapper: VueWrapper, text: string) {
  const found = wrapper.findAll('button').find((b) => b.text() === text);
  if (!found) throw new Error(`no button ${text}`);
  return found;
}

async function mountOnDdl(dirty: boolean[] = []) {
  const wrapper = mount(SystemVersionedTable, {
    props: { session, connectionName: 'Shop', object, reportDirty: (value: boolean) => dirty.push(value) },
    global: { stubs: { SqlCode: true, DdlView: true, PlanPreview: true } },
  });
  await flushPromises();
  await button(wrapper, 'DDL').trigger('click');
  const ddl = wrapper.findComponent({ name: 'DdlView' });
  expect(ddl.exists()).toBe(true);
  return { wrapper, ddl };
}

const reportDdlDirty = (ddl: VueWrapper, dirty: boolean): void => ((ddl.props() as Record<string, unknown>).reportDirty as (dirty: boolean) => void)(dirty);

beforeEach(() => {
  describeAnswer = () => table();
  release = null;
  slow = false;
});

describe('SystemVersionedTable DDL section', () => {
  it('stays mounted while the table is read again after a run, without "Loading" over it', async () => {
    const { wrapper, ddl } = await mountOnDdl();
    slow = true;
    ddl.vm.$emit('changed');
    await wrapper.vm.$nextTick();
    expect(wrapper.text()).not.toContain('Loading');
    expect(wrapper.findComponent({ name: 'DdlView' }).vm).toBe(ddl.vm);
    release?.();
    await flushPromises();
    expect(wrapper.findComponent({ name: 'DdlView' }).vm).toBe(ddl.vm);
    wrapper.unmount();
  });

  it('keeps the sections under a notice when reading again fails', async () => {
    const { wrapper, ddl } = await mountOnDdl();
    describeAnswer = () => {
      throw { kind: 'session', error: { kind: 'driver', error: { kind: 'query', message: 'server has gone away', code: null, detail: null, hint: null, position: null } } };
    };
    ddl.vm.$emit('changed');
    await flushPromises();
    expect(wrapper.text()).toContain('Could not read it again:');
    expect(wrapper.findComponent({ name: 'DdlView' }).vm).toBe(ddl.vm);
    // Retry reads again and clears the notice.
    describeAnswer = () => table();
    await button(wrapper, 'Retry').trigger('click');
    await flushPromises();
    expect(wrapper.text()).not.toContain('Could not read it again');
    wrapper.unmount();
  });

  it('keeps edited DDL when the table is gone, and shows the error screen only when no DDL would be lost', async () => {
    const { wrapper, ddl } = await mountOnDdl();
    describeAnswer = () => {
      throw { kind: 'catalog', error: { kind: 'notFound', message: 'table orders not found' } };
    };
    reportDdlDirty(ddl, true);
    ddl.vm.$emit('changed');
    await flushPromises();
    expect(wrapper.text()).toContain('shop.orders no longer exists. The DDL you edited is kept.');
    expect(wrapper.findComponent({ name: 'DdlView' }).vm).toBe(ddl.vm);
    reportDdlDirty(ddl, false);
    ddl.vm.$emit('changed');
    await flushPromises();
    expect(wrapper.text()).toContain('table orders not found');
    expect(wrapper.findComponent({ name: 'DdlView' }).exists()).toBe(false);
    wrapper.unmount();
  });

  it('counts edited DDL as unsaved and keeps it mounted while another section shows', async () => {
    const dirty: boolean[] = [];
    const { wrapper, ddl } = await mountOnDdl(dirty);
    reportDdlDirty(ddl, true);
    await wrapper.vm.$nextTick();
    expect(dirty.at(-1)).toBe(true);
    await button(wrapper, 'Versioning').trigger('click');
    const kept = wrapper.findComponent({ name: 'DdlView' });
    expect(kept.exists()).toBe(true);
    expect(kept.vm).toBe(ddl.vm);
    const hidden = (): boolean => (wrapper.findComponent({ name: 'DdlView' }).element as HTMLElement).style.display === 'none';
    expect(hidden()).toBe(true);
    await button(wrapper, 'DDL').trigger('click');
    expect(hidden()).toBe(false);
    await button(wrapper, 'Versioning').trigger('click');
    reportDdlDirty(ddl, false);
    await wrapper.vm.$nextTick();
    expect(dirty.at(-1)).toBe(false);
    expect(wrapper.findComponent({ name: 'DdlView' }).exists()).toBe(false);
    wrapper.unmount();
  });
});
