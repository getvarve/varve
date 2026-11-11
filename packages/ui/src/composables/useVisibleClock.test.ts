import { mount } from '@vue/test-utils';
import { defineComponent, h, nextTick, ref } from 'vue';
import type { Ref } from 'vue';
import { TAB_VISIBLE_KEY } from './useTabs';
import { useVisibleClock } from './useVisibleClock';

function host(visible: Ref<boolean>) {
  let clock: Readonly<Ref<number>> | null = null;
  const Child = defineComponent({
    setup() {
      clock = useVisibleClock(1000);
      return () => h('span');
    },
  });
  const wrapper = mount(Child, { global: { provide: { [TAB_VISIBLE_KEY as symbol]: visible } } });
  return { wrapper, now: () => clock?.value };
}

describe('useVisibleClock', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(1_000_000);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('ticks while the tab is shown, stops while it is hidden, and catches up when shown again', async () => {
    const visible = ref(true);
    const { wrapper, now } = host(visible);
    vi.advanceTimersByTime(1000);
    expect(now()).toBe(1_001_000);

    visible.value = false;
    await nextTick();
    expect(vi.getTimerCount()).toBe(0);
    vi.advanceTimersByTime(5000);
    expect(now()).toBe(1_001_000);

    visible.value = true;
    await nextTick();
    expect(now()).toBe(1_006_000);
    vi.advanceTimersByTime(1000);
    expect(now()).toBe(1_007_000);

    wrapper.unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('does not start while mounted in a hidden tab', () => {
    host(ref(false));
    expect(vi.getTimerCount()).toBe(0);
  });
});
