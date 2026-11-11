import { mount } from '@vue/test-utils';
import { nextTick, ref } from 'vue';
import type { Ref } from 'vue';
import { TAB_VISIBLE_KEY } from '../../composables/useTabs';
import SqlEditor from './SqlEditor.vue';

const LONG = Array.from({ length: 400 }, (_, i) => `SELECT ${i};`).join('\n');

function editor(visible: Ref<boolean>, errorRange: { key: number; from: number; to: number; doc: string } | null = null) {
  const wrapper = mount(SqlEditor, {
    attachTo: document.body,
    props: { modelValue: LONG, engine: 'postgres', revision: 0, errorRange, label: 'Query' },
    global: { provide: { [TAB_VISIBLE_KEY as symbol]: visible } },
  });
  const scroller = (wrapper.element as HTMLElement).querySelector<HTMLElement>('.cm-scroller');
  if (!scroller) throw new Error('The editor has no scrolling element.');
  return { wrapper, scroller };
}

/** The user scrolls the editor to `top`, `left`. */
function scrollTo(scroller: HTMLElement, top: number, left: number): void {
  scroller.scrollTop = top;
  scroller.scrollLeft = left;
  scroller.dispatchEvent(new Event('scroll'));
}

/** Runs the two animation frames the editor waits for. */
async function frames(): Promise<void> {
  vi.advanceTimersByTime(50);
  await nextTick();
}

describe('SqlEditor scroll position', () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['requestAnimationFrame', 'cancelAnimationFrame', 'setTimeout'] });
  });
  afterEach(() => {
    vi.useRealTimers();
    document.body.innerHTML = '';
  });

  it('puts the position back when the tab is shown again after the editor lost it', async () => {
    const visible = ref(true);
    const { wrapper, scroller } = editor(visible);
    scrollTo(scroller, 640, 35);

    // Another tab or page is shown: the scrolling element loses its offsets.
    visible.value = false;
    await nextTick();
    // What the element reports while hidden is not where the user scrolled to.
    scrollTo(scroller, 0, 0);
    visible.value = true;
    await nextTick();
    // Nor what it reports while it settles after being shown.
    scroller.dispatchEvent(new Event('scroll'));
    expect(scroller.scrollTop).toBe(0);
    await frames();
    expect(scroller.scrollTop).toBe(640);
    expect(scroller.scrollLeft).toBe(35);

    // Scrolling after that is remembered again.
    scrollTo(scroller, 200, 0);
    visible.value = false;
    await nextTick();
    scrollTo(scroller, 0, 0);
    visible.value = true;
    await nextTick();
    await frames();
    expect(scroller.scrollTop).toBe(200);
    expect(scroller.scrollLeft).toBe(0);
    wrapper.unmount();
  });

  it('keeps the position for the next time when the tab is hidden again before it was laid out', async () => {
    const visible = ref(true);
    const { wrapper, scroller } = editor(visible);
    scrollTo(scroller, 300, 0);
    visible.value = false;
    await nextTick();
    scrollTo(scroller, 0, 0);
    visible.value = true;
    await nextTick();
    visible.value = false;
    await nextTick();
    await frames();
    expect(scroller.scrollTop).toBe(0);
    visible.value = true;
    await nextTick();
    await frames();
    expect(scroller.scrollTop).toBe(300);
    wrapper.unmount();
  });

  it('leaves an editor that was never scrolled alone', async () => {
    const visible = ref(true);
    const { wrapper, scroller } = editor(visible);
    visible.value = false;
    await nextTick();
    visible.value = true;
    await nextTick();
    await frames();
    expect(scroller.scrollTop).toBe(0);
    expect(scroller.scrollLeft).toBe(0);
    wrapper.unmount();
  });
});
