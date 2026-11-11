import { flushPromises, mount } from '@vue/test-utils';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { SIDEBAR_IDEAL, SIDEBAR_MAX, SIDEBAR_MIN, useSidebar } from '../composables/useSidebar';
import AppShell from './AppShell.vue';

vi.mock('../composables/useWorkspace', () => ({ useWorkspace: () => ({ refresh: () => Promise.resolve() }) }));
vi.mock('../composables/useWorkspaceActions', () => ({ useWorkspaceActions: () => ({ newConnection: vi.fn() }) }));
vi.mock('../composables/useUpdates', () => ({
  installUpdates: () => Promise.resolve(() => undefined),
  useUpdates: () => ({ state: { dialogOpen: false } }),
}));
vi.mock('../composables/useQuitGuard', () => ({ installQuitGuard: () => Promise.resolve(() => undefined) }));
vi.mock('../composables/useExternalChanges', () => ({ installExternalChanges: () => Promise.resolve(() => undefined) }));

// The real children pull in the whole workspace; the shell only needs something in their place.
vi.mock('./sidebar/WorkspaceTree.vue', () => ({ default: { name: 'WorkspaceTree', template: '<div class="tree-stub" />' } }));
vi.mock('./StatusBar.vue', () => ({ default: { name: 'StatusBar', template: '<footer />' } }));
vi.mock('./dialogs/DialogHost.vue', () => ({ default: { name: 'DialogHost', template: '<i />' } }));
vi.mock('./dialogs/UpdateDialog.vue', () => ({ default: { name: 'UpdateDialog', template: '<i />' } }));
vi.mock('./common/ToastHost.vue', () => ({ default: { name: 'ToastHost', template: '<i />' } }));

function shell() {
  return mount(AppShell, {
    attachTo: document.body,
    global: {
      stubs: {
        RouterLink: { template: '<a><slot /></a>' },
        RouterView: true,
        ExtensionSlot: true,
        VarveMark: true,
      },
    },
  });
}

const press = (init: KeyboardEventInit) => globalThis.dispatchEvent(new KeyboardEvent('keydown', { cancelable: true, ...init }));

describe('AppShell sidebar', () => {
  beforeEach(() => {
    localStorage.clear();
    useSidebar().reload();
    document.body.innerHTML = '';
  });

  it('hides and shows the sidebar from the button and keeps the tree mounted', async () => {
    const wrapper = shell();
    await flushPromises();
    const side = wrapper.get('.side');
    expect(side.isVisible()).toBe(true);
    expect(wrapper.find('.titlebar .icon-btn').exists()).toBe(false);

    const hide = wrapper.get('.side-head .icon-btn');
    expect(hide.attributes('aria-label')).toContain('Hide sidebar');
    (hide.element as HTMLElement).focus();
    await hide.trigger('click');
    await flushPromises();

    expect(wrapper.classes()).toContain('collapsed');
    expect(side.isVisible()).toBe(false);
    expect(wrapper.find('.tree-stub').exists()).toBe(true);
    const show = wrapper.get('.titlebar .icon-btn');
    expect(show.attributes('aria-label')).toContain('Show sidebar');
    expect(show.attributes('aria-expanded')).toBe('false');
    // Focus was inside the part that went away: it lands on the button that is left.
    expect(document.activeElement).toBe(show.element);

    await show.trigger('click');
    await flushPromises();
    expect(side.isVisible()).toBe(true);
    expect(document.activeElement).toBe(wrapper.get('.side-head .icon-btn').element);
    wrapper.unmount();
  });

  it('toggles with the keyboard shortcut and stops listening when unmounted', async () => {
    const wrapper = shell();
    await flushPromises();
    press({ key: 'b', ctrlKey: true });
    expect(useSidebar().state.collapsed).toBe(true);
    // Other combinations are left alone.
    press({ key: 'b' });
    press({ key: 'b', ctrlKey: true, shiftKey: true });
    expect(useSidebar().state.collapsed).toBe(true);
    wrapper.unmount();
    press({ key: 'b', ctrlKey: true });
    expect(useSidebar().state.collapsed).toBe(true);
  });

  it('resizes from the divider with the keyboard and resets on a double click', async () => {
    const wrapper = shell();
    await flushPromises();
    const divider = wrapper.get('.divider');
    expect(divider.attributes('aria-valuenow')).toBe(String(SIDEBAR_IDEAL));
    await divider.trigger('keydown', { key: 'ArrowRight' });
    expect(useSidebar().state.width).toBe(SIDEBAR_IDEAL + 10);
    await divider.trigger('keydown', { key: 'End' });
    expect(useSidebar().state.width).toBe(SIDEBAR_MAX);
    await divider.trigger('keydown', { key: 'Home' });
    expect(useSidebar().state.width).toBe(SIDEBAR_MIN);
    expect((wrapper.element as HTMLElement).style.getPropertyValue('--sidebar-width')).toBe(`${SIDEBAR_MIN}px`);
    await divider.trigger('dblclick');
    expect(useSidebar().state.width).toBe(SIDEBAR_IDEAL);
    wrapper.unmount();
  });
});
