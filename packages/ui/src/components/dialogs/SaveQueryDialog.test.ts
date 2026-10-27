import type * as RecordsModule from '../../api/records';
import { flushPromises, mount } from '@vue/test-utils';
import type { ConnectionProfile, SavedQuery } from '../../api/records';
import SaveQueryDialog from './SaveQueryDialog.vue';

let release: (query: SavedQuery) => void = () => undefined;
const createQuery = vi.fn(
  () =>
    new Promise<SavedQuery>((resolve) => {
      release = resolve;
    }),
);
vi.mock('../../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  createQuery: () => createQuery(),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [], queries: [] })),
}));

const connection = { id: 'c1', name: 'Orders', folderId: null } as ConnectionProfile;

describe('SaveQueryDialog', () => {
  it('saves once when Enter and Save are pressed while the first save runs', async () => {
    const wrapper = mount(SaveQueryDialog, {
      attachTo: document.body,
      props: { request: { connection, sql: 'SELECT 1', database: null, name: 'Revenue', folderId: null, tags: [] } },
    });
    const form = document.body.querySelector('form');
    const save = [...document.body.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Save' && !b.classList.contains('visually-hidden'));
    form?.dispatchEvent(new Event('submit'));
    save?.click();
    form?.dispatchEvent(new Event('submit'));
    expect(form).not.toBeNull();
    expect(save).toBeDefined();
    for (let i = 0; i < 10; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
    expect(createQuery).toHaveBeenCalledTimes(1);
    release({ id: 'q1', name: 'Revenue' } as SavedQuery);
    await flushPromises();
    expect(wrapper.emitted('done')).toHaveLength(1);
    wrapper.unmount();
  });
});
