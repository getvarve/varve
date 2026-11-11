/* eslint-disable vue/one-component-per-file -- the test mounts small host components */
import { mount } from '@vue/test-utils';
import { defineComponent, h, nextTick, ref } from 'vue';
import type { Ref } from 'vue';
import { TAB_VISIBLE_KEY } from './useTabs';
import { useKeepGridScroll, useSizeColumnsWhenShown } from './useSizeColumnsWhenShown';

function host(visible: Ref<boolean>, data: Ref<number>) {
  const grid = { isDestroyed: () => false, autoSizeAllColumns: vi.fn() };
  const Child = defineComponent({
    setup() {
      useSizeColumnsWhenShown(() => grid, [data]);
      return () => h('div');
    },
  });
  const wrapper = mount(Child, { global: { provide: { [TAB_VISIBLE_KEY as symbol]: visible } } });
  return { wrapper, grid };
}

/** Runs the two animation frames the composable waits for. */
async function frames(): Promise<void> {
  vi.advanceTimersByTime(50);
  await nextTick();
}

describe('useSizeColumnsWhenShown', () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['requestAnimationFrame', 'cancelAnimationFrame', 'setTimeout'] });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('sizes the columns once when data that arrived while hidden is shown', async () => {
    const visible = ref(true);
    const data = ref(0);
    const { wrapper, grid } = host(visible, data);
    // Data while shown: the grid's own autoSizeStrategy handles it.
    data.value = 1;
    visible.value = false;
    visible.value = true;
    await nextTick();
    await frames();
    expect(grid.autoSizeAllColumns).not.toHaveBeenCalled();

    visible.value = false;
    await nextTick();
    data.value = 2;
    await nextTick();
    visible.value = true;
    await nextTick();
    await frames();
    expect(grid.autoSizeAllColumns).toHaveBeenCalledTimes(1);
    expect(grid.autoSizeAllColumns).toHaveBeenCalledWith(false);

    // Hidden and shown again without new data: nothing to do.
    visible.value = false;
    await nextTick();
    visible.value = true;
    await nextTick();
    await frames();
    expect(grid.autoSizeAllColumns).toHaveBeenCalledTimes(1);
    wrapper.unmount();
  });

  it('does not size a grid that was hidden again before it was laid out, and sizes it the next time', async () => {
    const visible = ref(false);
    const { wrapper, grid } = host(visible, ref(0));
    visible.value = true;
    await nextTick();
    visible.value = false;
    await nextTick();
    await frames();
    expect(grid.autoSizeAllColumns).not.toHaveBeenCalled();
    visible.value = true;
    await nextTick();
    await frames();
    expect(grid.autoSizeAllColumns).toHaveBeenCalledTimes(1);
    wrapper.unmount();
  });

  it('sizes a grid created while hidden when it is first shown', async () => {
    const visible = ref(false);
    const { wrapper, grid } = host(visible, ref(0));
    visible.value = true;
    await nextTick();
    await frames();
    expect(grid.autoSizeAllColumns).toHaveBeenCalledTimes(1);
    wrapper.unmount();
  });
});

describe('useKeepGridScroll', () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['requestAnimationFrame', 'cancelAnimationFrame', 'setTimeout'] });
  });
  afterEach(() => {
    vi.useRealTimers();
    document.body.innerHTML = '';
  });

  /** A grid of `rows` rows of 24 px with its scrolling element, inside a tab whose visibility is `visible`. */
  function scrollHost(visible: Ref<boolean>, rows = 100) {
    const root = document.createElement('div');
    const viewport = document.createElement('div');
    viewport.className = 'ag-grid-viewport';
    root.append(viewport);
    document.body.append(root);
    const grid = {
      isDestroyed: () => false,
      getDisplayedRowCount: () => rows,
      getDisplayedRowAtIndex: (index: number) => ({ rowTop: index * 24 }),
      ensureIndexVisible: vi.fn(),
    };
    let keep: { noteScroll: (top: number, left: number) => void } = { noteScroll: () => undefined };
    const Child = defineComponent({
      setup() {
        keep = useKeepGridScroll(() => grid, () => root);
        return () => h('div');
      },
    });
    const wrapper = mount(Child, { global: { provide: { [TAB_VISIBLE_KEY as symbol]: visible } } });
    return { wrapper, grid, viewport, noteScroll: (top: number, left: number) => keep.noteScroll(top, left) };
  }

  it('puts the position back when the grid comes back at the top left', async () => {
    const visible = ref(true);
    const { wrapper, grid, viewport, noteScroll } = scrollHost(visible);
    viewport.scrollTop = 500;
    viewport.scrollLeft = 130;
    noteScroll(500, 130);

    // Activity is shown: the workspace leaves the document and the element loses its offsets.
    visible.value = false;
    await nextTick();
    viewport.scrollTop = 0;
    viewport.scrollLeft = 0;
    // What the grid reports while hidden, or while it settles after being shown, is not the user's position.
    noteScroll(0, 0);
    visible.value = true;
    await nextTick();
    noteScroll(0, 0);
    expect(grid.ensureIndexVisible).not.toHaveBeenCalled();
    await frames();
    // 500 px is inside row 20 (480 to 504): the grid draws from there, then the exact offsets are set.
    expect(grid.ensureIndexVisible).toHaveBeenCalledTimes(1);
    expect(grid.ensureIndexVisible).toHaveBeenCalledWith(20, 'top');
    expect(viewport.scrollTop).toBe(500);
    expect(viewport.scrollLeft).toBe(130);
    wrapper.unmount();
  });

  it('leaves a grid alone that kept its position, or was never scrolled', async () => {
    const visible = ref(true);
    const { wrapper, grid, viewport, noteScroll } = scrollHost(visible);
    visible.value = false;
    await nextTick();
    visible.value = true;
    await nextTick();
    await frames();
    expect(grid.ensureIndexVisible).not.toHaveBeenCalled();

    viewport.scrollTop = 240;
    noteScroll(240, 0);
    visible.value = false;
    await nextTick();
    visible.value = true;
    await nextTick();
    await frames();
    expect(grid.ensureIndexVisible).not.toHaveBeenCalled();
    expect(viewport.scrollTop).toBe(240);

    // Scrolling after the tab is back is remembered again.
    viewport.scrollTop = 48;
    noteScroll(48, 0);
    visible.value = false;
    await nextTick();
    viewport.scrollTop = 0;
    visible.value = true;
    await nextTick();
    await frames();
    expect(grid.ensureIndexVisible).toHaveBeenCalledWith(2, 'top');
    expect(viewport.scrollTop).toBe(48);
    wrapper.unmount();
  });

  it('waits when the tab is hidden again before the grid was laid out, and sets only the offsets of a grid without rows', async () => {
    const visible = ref(true);
    const { wrapper, grid, viewport, noteScroll } = scrollHost(visible, 0);
    viewport.scrollLeft = 90;
    noteScroll(0, 90);
    visible.value = false;
    await nextTick();
    viewport.scrollLeft = 0;
    visible.value = true;
    await nextTick();
    visible.value = false;
    await nextTick();
    await frames();
    expect(viewport.scrollLeft).toBe(0);

    visible.value = true;
    await nextTick();
    await frames();
    expect(viewport.scrollLeft).toBe(90);
    expect(grid.ensureIndexVisible).not.toHaveBeenCalled();
    wrapper.unmount();
  });
});
