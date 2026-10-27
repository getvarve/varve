import { mount } from '@vue/test-utils';
import { defineComponent, h, nextTick, ref } from 'vue';
import { useCrossFade } from '../../composables/useCrossFade';
import { useToasts } from '../../composables/useToasts';
import VarveMark from '../VarveMark.vue';
import ContextMenu from './ContextMenu.vue';
import Modal from './Modal.vue';
import ToastHost from './ToastHost.vue';

/** jsdom has no Web Animations API: record calls and let the test end them. */
function stubAnimate(): { calls: Array<{ el: Element; keyframes: Keyframe[] }>; finish: () => void } {
  const calls: Array<{ el: Element; keyframes: Keyframe[] }> = [];
  let finish = (): void => {};
  const finished = new Promise<void>((resolve) => {
    finish = resolve;
  });
  Element.prototype.animate = function animate(this: Element, keyframes: Keyframe[]) {
    calls.push({ el: this, keyframes });
    return { finished } as unknown as Animation;
  } as Element['animate'];
  return { calls, finish: () => finish() };
}

async function settle(): Promise<void> {
  await nextTick();
  await nextTick();
}

afterEach(() => {
  Reflect.deleteProperty(Element.prototype, 'animate');
  document.body.innerHTML = '';
});

describe('Modal motion', () => {
  it('returns focus at once on close and leaves only an inert, fading copy', async () => {
    const { calls, finish } = stubAnimate();
    const opener = document.createElement('button');
    document.body.append(opener);
    opener.focus();
    const wrapper = mount(Modal, { props: { title: 'Rename' }, slots: { default: '<input id="name" name="name" data-autofocus>' }, attachTo: document.body });
    await settle();
    expect(document.activeElement?.id).toBe('name');

    wrapper.unmount();
    // Focus is back on the opener without waiting for the fade.
    expect(document.activeElement).toBe(opener);
    const copy = document.body.querySelector<HTMLElement>('.motion-copy');
    expect(copy?.classList.contains('backdrop')).toBe(true);
    expect(copy?.hasAttribute('inert')).toBe(true);
    expect(copy?.getAttribute('aria-hidden')).toBe('true');
    expect(document.querySelector('[role="dialog"], #name, [name="name"]')).toBeNull();
    expect(calls.map((c) => c.el)).toEqual([copy]);
    expect(calls[0]?.keyframes).toEqual([{ opacity: 1 }, { opacity: 0 }]);

    finish();
    await settle();
    expect(document.body.querySelector('.motion-copy')).toBeNull();
  });
});

describe('Modal close copy', () => {
  it('holds no typed password, also when it was revealed', async () => {
    stubAnimate();
    const wrapper = mount(Modal, {
      props: { title: 'Password' },
      slots: { default: '<input class="pw" type="password" data-autofocus><input class="shown" type="text" autocomplete="current-password">' },
      attachTo: document.body,
    });
    await settle();
    (document.querySelector('.pw') as HTMLInputElement).value = 's3cret';
    (document.querySelector('.shown') as HTMLInputElement).value = 's3cret';
    wrapper.unmount();
    const copy = document.body.querySelector<HTMLElement>('.motion-copy');
    expect(copy).not.toBeNull();
    const inputs = [...(copy?.querySelectorAll('input') ?? [])];
    expect(inputs.map((i) => [i.type, i.value])).toEqual([
      ['password', ''],
      ['password', ''],
    ]);
  });
});

describe('ContextMenu motion', () => {
  it('grows the top menu from the click point and does not animate submenus', async () => {
    const wrapper = mount(ContextMenu, {
      props: {
        items: [
          { id: 'new', label: 'New', children: [{ id: 'view', label: 'New view' }] },
          { id: 'copy', label: 'Copy name' },
        ],
        x: 40,
        y: 60,
        label: 'Actions',
      },
      attachTo: document.body,
    });
    await settle();
    const menu = document.querySelector<HTMLElement>('.menu.root');
    expect(menu?.style.transformOrigin).toBe('0px 0px');
    // Focus still lands on the first entry while the menu opens.
    expect(document.activeElement?.getAttribute('data-sub')).toBe('new');

    const parent = document.querySelector<HTMLButtonElement>('button[data-sub="new"]');
    parent?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
    await settle();
    const sub = document.querySelector('.submenu');
    expect(sub).not.toBeNull();
    expect(sub?.classList.contains('root')).toBe(false);
    wrapper.unmount();
  });
});

describe('ContextMenu submenu placement', () => {
  it('measures by layout size, not the box the opening scale shrinks', async () => {
    const items = [{ id: 'new', label: 'New', children: [{ id: 'view', label: 'New view' }] }];
    // A bounding box that would always flip, and layout sizes that never would: the layout decides.
    const box = vi.spyOn(Element.prototype, 'getBoundingClientRect').mockReturnValue(new DOMRect(5000, 5000, 200, 100));
    const width = vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockReturnValue(200);
    try {
      const wrapper = mount(ContextMenu, { props: { items, x: 10, y: 10, label: 'Actions' }, attachTo: document.body });
      await settle();
      const parent = document.querySelector<HTMLButtonElement>('button[data-sub="new"]');
      parent?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
      await settle();
      expect(document.querySelector('.submenu')?.classList.contains('flip')).toBe(false);
      wrapper.unmount();

      // A layout that reaches past the window edge flips.
      width.mockReturnValue(window.innerWidth);
      const wide = mount(ContextMenu, { props: { items, x: 10, y: 10, label: 'Actions' }, attachTo: document.body });
      await settle();
      document.querySelector('button[data-sub="new"]')?.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
      await settle();
      expect(document.querySelector('.submenu')?.classList.contains('flip')).toBe(true);
      wide.unmount();
    } finally {
      box.mockRestore();
      width.mockRestore();
    }
  });
});

describe('ToastHost motion', () => {
  it('keeps the live region on the animated list', async () => {
    const toasts = useToasts();
    const wrapper = mount(ToastHost, { attachTo: document.body });
    const id = toasts.push('info', 'Saved', '', 0);
    await settle();
    const region = document.querySelector('[role="region"][aria-label="Notifications"]');
    expect(region?.getAttribute('aria-live')).toBe('polite');
    expect(region?.querySelectorAll('.toast')).toHaveLength(1);
    toasts.dismiss(id);
    await settle();
    expect(region?.querySelectorAll('.toast')).toHaveLength(0);
    wrapper.unmount();
  });
});

describe('VarveMark draw', () => {
  it('draws only when asked', () => {
    expect(mount(VarveMark, { props: { size: 40, draw: true } }).classes()).toContain('draw');
    expect(mount(VarveMark, { props: { size: 40 } }).classes()).not.toContain('draw');
  });
});

describe('useCrossFade', () => {
  it('fades an inert copy of the old content out over the new one', async () => {
    const { calls } = stubAnimate();
    const text = ref('SELECT 1;');
    const Host = defineComponent({
      setup() {
        const el = ref<HTMLElement | null>(null);
        useCrossFade(el, () => text.value, 120);
        return () => h('div', { style: 'position: relative' }, [h('pre', { ref: el, id: 'sql' }, text.value)]);
      },
    });
    const wrapper = mount(Host, { attachTo: document.body });
    await settle();
    expect(calls).toHaveLength(0);

    text.value = 'SELECT 2;';
    await settle();
    const copy = document.querySelector<HTMLElement>('.motion-copy');
    expect(copy?.textContent).toBe('SELECT 1;');
    expect(copy?.style.position).toBe('absolute');
    expect(copy?.hasAttribute('inert')).toBe(true);
    expect(copy?.nextElementSibling?.id).toBe('sql');
    expect(document.getElementById('sql')?.textContent).toBe('SELECT 2;');
    expect(calls.map((c) => [c.el === copy ? 'copy' : (c.el as HTMLElement).id, c.keyframes])).toEqual([
      ['copy', [{ opacity: 1 }, { opacity: 0 }]],
      ['sql', [{ opacity: 0 }, { opacity: 1 }]],
    ]);
    wrapper.unmount();
  });
});
