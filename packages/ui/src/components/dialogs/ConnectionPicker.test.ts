import { flushPromises, mount } from '@vue/test-utils';
import type { ConnectionProfile } from '../../api/records';
import { useWorkspace } from '../../composables/useWorkspace';
import ConnectionPicker from './ConnectionPicker.vue';

const orders = { id: 'c1', name: 'Orders', engine: 'postgres', environment: 'production', folderId: null } as unknown as ConnectionProfile;
const ops = { id: 'c2', name: 'Ops', engine: 'mysql', environment: 'development', folderId: null } as unknown as ConnectionProfile;
const cache = { id: 'c3', name: 'Cache', engine: 'redis', environment: 'test', folderId: null } as unknown as ConnectionProfile;

function key(target: Element | null, name: string): void {
  target?.dispatchEvent(new KeyboardEvent('keydown', { key: name, bubbles: true, cancelable: true }));
}

async function open() {
  useWorkspace().records.value = { folders: [], connections: [orders, ops, cache], queries: [] };
  const wrapper = mount(ConnectionPicker, { attachTo: document.body, props: { title: 'Open on' } });
  await flushPromises();
  const input = document.body.querySelector<HTMLInputElement>('input.input');
  const names = (): string[] => [...document.body.querySelectorAll('button.item .name')].map((n) => n.textContent ?? '');
  return { wrapper, input, names };
}

describe('ConnectionPicker keyboard', () => {
  afterEach(() => {
    document.body.innerHTML = '';
  });

  it('picks the only match on Enter', async () => {
    const { wrapper, input } = await open();
    expect(document.activeElement).toBe(input);
    if (input) {
      input.value = 'ord';
      input.dispatchEvent(new Event('input'));
    }
    await flushPromises();
    key(input, 'Enter');
    expect(wrapper.emitted('done')).toEqual([[orders]]);
    wrapper.unmount();
  });

  it('moves from the filter into the list and between the connections', async () => {
    const { wrapper, input, names } = await open();
    expect(names()).toEqual(['Orders', 'Ops', 'Cache']);
    key(input, 'ArrowDown');
    expect(document.activeElement?.textContent).toContain('Orders');
    key(document.activeElement, 'ArrowDown');
    expect(document.activeElement?.textContent).toContain('Ops');
    key(document.activeElement, 'End');
    expect(document.activeElement?.textContent).toContain('Cache');
    key(document.activeElement, 'Home');
    key(document.activeElement, 'ArrowUp');
    expect(document.activeElement).toBe(input);

    // Enter with several matches moves to the first one instead of picking.
    key(input, 'Enter');
    expect(document.activeElement?.textContent).toContain('Orders');
    expect(wrapper.emitted('done')).toBeUndefined();
    wrapper.unmount();
  });
});
