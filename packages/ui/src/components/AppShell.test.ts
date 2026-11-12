import { flushPromises, mount } from '@vue/test-utils';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { SIDEBAR_IDEAL, SIDEBAR_MAX, SIDEBAR_MIN, useSidebar } from '../composables/useSidebar';
import AppShell from './AppShell.vue';

const setGlassShapes = vi.fn((_shapes: unknown[]) => Promise.resolve(true));
vi.mock('../backend', () => ({ setGlassShapes: (shapes: unknown[]) => setGlassShapes(shapes) }));
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
    delete document.documentElement.dataset.glassShapes;
    delete document.documentElement.dataset.windowInactive;
    setGlassShapes.mockClear();
    vi.restoreAllMocks();
  });

  it('tells the backend where the sidebar card is, only with Liquid Glass', async () => {
    const box = { left: 8, top: 8, width: 202, height: 700 } as DOMRect;
    vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockReturnValue(box);

    // No Liquid Glass: nothing is reported.
    const plain = shell();
    await flushPromises();
    expect(setGlassShapes).not.toHaveBeenCalled();
    plain.unmount();

    document.documentElement.dataset.glassShapes = '';
    const wrapper = shell();
    await flushPromises();
    expect(setGlassShapes).toHaveBeenLastCalledWith([{ id: 'sidebar', x: 8, y: 8, width: 202, height: 700, radius: 18 }]);

    // Hidden: the glass goes with it.
    await wrapper.get('.side-head .icon-btn').trigger('click');
    await flushPromises();
    expect(setGlassShapes).toHaveBeenLastCalledWith([]);

    // Shown again, then the user asks for less transparency: the glass is taken away once.
    await wrapper.get('.titlebar .icon-btn').trigger('click');
    await flushPromises();
    expect(setGlassShapes).toHaveBeenLastCalledWith([expect.objectContaining({ id: 'sidebar' })]);
    delete document.documentElement.dataset.glassShapes;
    await flushPromises();
    expect(setGlassShapes).toHaveBeenLastCalledWith([]);
    const calls = setGlassShapes.mock.calls.length;
    useSidebar().setWidth(260);
    await flushPromises();
    expect(setGlassShapes.mock.calls.length).toBe(calls);
    wrapper.unmount();
  });

  it('marks the window as not in front while it has no focus', async () => {
    const wrapper = shell();
    await flushPromises();
    globalThis.dispatchEvent(new FocusEvent('blur'));
    expect(document.documentElement.dataset.windowInactive).toBe('');
    globalThis.dispatchEvent(new FocusEvent('focus'));
    expect(document.documentElement.dataset.windowInactive).toBeUndefined();
    wrapper.unmount();
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
    wrapper.unmount();
  });

  it('fits the width to the text on a double click of the divider', async () => {
    const wrapper = shell();
    await flushPromises();
    const root = wrapper.element as HTMLElement;
    // jsdom lays nothing out: stand in for a row whose name needs 300px at any sidebar width below 330.
    const name = wrapper.get('.menu-item').element.firstElementChild as HTMLElement;
    const needed = 330;
    let fits = false;
    Object.defineProperty(name, 'scrollWidth', { get: () => 300 });
    Object.defineProperty(name, 'clientWidth', {
      get: () => (fits ? 300 : Math.min(300, 300 - (needed - Number.parseInt(root.style.getPropertyValue('--sidebar-width'), 10)))),
    });
    useSidebar().setWidth(SIDEBAR_MAX);

    await wrapper.get('.divider').trigger('dblclick');
    expect(useSidebar().state.width).toBeGreaterThanOrEqual(needed);
    expect(useSidebar().state.width).toBeLessThanOrEqual(needed + 2);
    expect(root.style.getPropertyValue('--sidebar-width')).toBe(`${useSidebar().state.width}px`);

    // Nothing cut off: it shrinks to the smallest width.
    fits = true;
    await wrapper.get('.divider').trigger('keydown', { key: 'Enter' });
    expect(useSidebar().state.width).toBe(SIDEBAR_MIN);
    wrapper.unmount();
  });
});
