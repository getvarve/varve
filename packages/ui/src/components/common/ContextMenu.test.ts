import { mount } from '@vue/test-utils';
import { nextTick } from 'vue';
import type { MenuItem } from '../../lib/menu';
import ContextMenu from './ContextMenu.vue';

const items: MenuItem[] = [
  { id: 'overview', label: 'Open overview' },
  {
    id: 'new',
    label: 'New',
    children: [
      { id: 'new-kind:view', label: 'New view…' },
      { id: 'new-kind:sequence', label: 'New sequence…', disabled: true },
      { id: 'new-kind:function', label: 'New function…' },
    ],
  },
  { id: 'copy-name', label: 'Copy name' },
];

function key(target: Element | null, name: string): void {
  target?.dispatchEvent(new KeyboardEvent('keydown', { key: name, bubbles: true }));
}

async function settle(): Promise<void> {
  await nextTick();
  await nextTick();
}

describe('ContextMenu submenus', () => {
  afterEach(() => {
    document.body.innerHTML = '';
  });

  it('opens a submenu by keyboard, moves inside it, closes it with ArrowLeft and selects a child', async () => {
    const wrapper = mount(ContextMenu, { props: { items, x: 10, y: 10, label: 'Actions' }, attachTo: document.body });
    await settle();
    const parent = document.querySelector<HTMLButtonElement>('button[data-sub="new"]');
    expect(parent?.getAttribute('aria-haspopup')).toBe('menu');
    // The wrapper between the menu and its item is presentational.
    expect(parent?.parentElement?.getAttribute('role')).toBe('none');
    expect(parent?.getAttribute('aria-expanded')).toBe('false');
    expect(document.querySelector('.submenu')).toBeNull();

    parent?.focus();
    key(parent, 'ArrowRight');
    await settle();
    expect(parent?.getAttribute('aria-expanded')).toBe('true');
    const sub = document.querySelector('.submenu');
    expect(sub?.getAttribute('role')).toBe('menu');
    expect(document.activeElement?.textContent?.trim()).toBe('New view…');

    // Arrow keys stay in the submenu and skip disabled entries.
    key(document.activeElement, 'ArrowDown');
    expect(document.activeElement?.textContent?.trim()).toBe('New function…');
    key(document.activeElement, 'ArrowDown');
    expect(document.activeElement?.textContent?.trim()).toBe('New view…');

    key(document.activeElement, 'ArrowLeft');
    await settle();
    expect(document.querySelector('.submenu')).toBeNull();
    expect(document.activeElement).toBe(parent);
    expect(wrapper.emitted('close')).toBeUndefined();

    // The top level moves over the submenu entry like any other.
    key(parent, 'ArrowDown');
    expect(document.activeElement?.textContent?.trim()).toBe('Copy name');

    // Clicking the parent opens it; clicking a child selects it and closes the menu.
    parent?.click();
    await settle();
    document.querySelector<HTMLButtonElement>('.submenu button:last-of-type')?.click();
    expect(wrapper.emitted('select')).toEqual([['new-kind:function']]);
    expect(wrapper.emitted('close')).toHaveLength(1);
    wrapper.unmount();
  });

  it('closes only the submenu on Escape, then the menu', async () => {
    const wrapper = mount(ContextMenu, { props: { items, x: 10, y: 10, label: 'Actions' }, attachTo: document.body });
    await settle();
    const parent = document.querySelector<HTMLButtonElement>('button[data-sub="new"]');
    parent?.click();
    await settle();
    key(document.activeElement, 'Escape');
    await settle();
    expect(document.querySelector('.submenu')).toBeNull();
    expect(wrapper.emitted('close')).toBeUndefined();
    key(document.activeElement, 'Escape');
    expect(wrapper.emitted('close')).toHaveLength(1);
    // Choosing the parent entry itself never selects anything.
    expect(wrapper.emitted('select')).toBeUndefined();
    wrapper.unmount();
  });
});
