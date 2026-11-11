/* eslint-disable vue/one-component-per-file -- the test mounts small host components */
import { flushPromises, mount } from '@vue/test-utils';
import { defineComponent, h, nextTick, ref } from 'vue';
import type { Ref } from 'vue';
import { TAB_VISIBLE_KEY } from '../../composables/useTabs';
import { useAutoRefresh } from './useAutoRefresh';

function panel(visible: Ref<boolean>, intervalMs: number | null) {
  const session = ref('s1');
  const fetch = vi.fn(async () => undefined);
  const Panel = defineComponent({
    setup() {
      useAutoRefresh(
        fetch,
        intervalMs,
        () => undefined,
        () => session.value,
      );
      return () => h('div');
    },
  });
  const wrapper = mount(Panel, { global: { provide: { [TAB_VISIBLE_KEY as symbol]: visible } } });
  return { wrapper, fetch, session };
}

describe('useAutoRefresh', () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it('does not poll while its tab is hidden and loads once when the tab is shown again', async () => {
    const visible = ref(true);
    const { wrapper, fetch } = panel(visible, 1000);
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(1);

    visible.value = false;
    await nextTick();
    vi.advanceTimersByTime(5000);
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(1);

    visible.value = true;
    await nextTick();
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(2);
    vi.advanceTimersByTime(1000);
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(3);
    wrapper.unmount();
  });

  it('mounted in a hidden tab, the first load waits until the tab is shown', async () => {
    const visible = ref(false);
    const { wrapper, fetch } = panel(visible, null);
    await flushPromises();
    expect(fetch).not.toHaveBeenCalled();
    visible.value = true;
    await nextTick();
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(1);
    wrapper.unmount();
  });

  it('loads once when the session is replaced, also with auto refresh off', async () => {
    const visible = ref(true);
    const { wrapper, fetch, session } = panel(visible, null);
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(1);

    // A reconnect: the panel stays mounted and would keep showing the old session's rows.
    session.value = 's2';
    await nextTick();
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(2);

    // Replaced while the tab is hidden: the load waits until the tab is shown.
    visible.value = false;
    await nextTick();
    session.value = 's3';
    await nextTick();
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(2);
    visible.value = true;
    await nextTick();
    await flushPromises();
    expect(fetch).toHaveBeenCalledTimes(3);
    wrapper.unmount();
  });

  it('hands a failed fetch to onError with the session the fetch was sent on', async () => {
    const session = ref('s1');
    const sentOn: string[] = [];
    const failedOn: string[] = [];
    let fail: (error: unknown) => void = () => undefined;
    const Panel = defineComponent({
      setup() {
        useAutoRefresh(
          (id) => {
            sentOn.push(id);
            return new Promise<void>((_resolve, reject) => {
              fail = reject;
            });
          },
          null,
          (_error, id) => failedOn.push(id),
          () => session.value,
        );
        return () => h('div');
      },
    });
    const wrapper = mount(Panel, { global: { provide: { [TAB_VISIBLE_KEY as symbol]: ref(true) } } });
    await flushPromises();
    expect(sentOn).toEqual(['s1']);
    // A reconnect replaces the session while the fetch is still running.
    session.value = 's2';
    await nextTick();
    fail(new Error('gone'));
    await flushPromises();
    expect(failedOn).toEqual(['s1']);
    // The reload for the new session runs after it, on the new session.
    expect(sentOn).toEqual(['s1', 's2']);
    fail(new Error('gone'));
    await flushPromises();
    expect(failedOn).toEqual(['s1', 's2']);
    wrapper.unmount();
  });
});
