import { flushPromises, mount } from '@vue/test-utils';
import { h, nextTick, ref } from 'vue';
import { TAB_VISIBLE_KEY } from '../../composables/useTabs';
import Modal from './Modal.vue';

/** Presses a key on `target` the way the browser does: the event bubbles to the document and can be cancelled. */
function press(target: EventTarget, key: string, init: KeyboardEventInit = {}): KeyboardEvent {
  const event = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event);
  return event;
}

/** Lets the timer of the dialog's focus check run. */
async function afterFocusMoved(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

function mountModal(title: string, labels: string[]) {
  return mount(Modal, {
    attachTo: document.body,
    props: { title },
    slots: { default: () => labels.map((label) => h('button', { type: 'button' }, label)) },
  });
}

describe('Modal initial focus', () => {
  // jsdom has no layout: every element counts as hidden unless it reports an offset parent.
  const original = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetParent');
  beforeAll(() => {
    Object.defineProperty(HTMLElement.prototype, 'offsetParent', { configurable: true, get: () => document.body });
  });
  afterAll(() => {
    if (original) Object.defineProperty(HTMLElement.prototype, 'offsetParent', original);
  });
  afterEach(() => {
    document.body.innerHTML = '';
  });

  it('starts on the first control of the body, not on the header Close button', async () => {
    const wrapper = mount(Modal, {
      attachTo: document.body,
      props: { title: 'Delete folder' },
      slots: {
        default: () => h('button', { type: 'button', class: 'first' }, 'First'),
        footer: () => h('button', { type: 'button' }, 'Cancel'),
      },
    });
    await flushPromises();
    expect(document.activeElement?.textContent).toBe('First');
    wrapper.unmount();
  });

  it('prefers data-autofocus, and focusInitial moves focus there again after the content changed', async () => {
    const wrapper = mount(Modal, {
      attachTo: document.body,
      props: { title: 'Update' },
      slots: {
        default: () => h('button', { type: 'button' }, 'Details'),
        footer: () => h('button', { type: 'button', 'data-autofocus': '' }, 'Install'),
      },
    });
    await flushPromises();
    expect(document.activeElement?.textContent).toBe('Install');
    document.querySelector<HTMLButtonElement>('button[aria-label="Close"]')?.focus();
    (wrapper.vm as unknown as { focusInitial: () => void }).focusInitial();
    expect(document.activeElement?.textContent).toBe('Install');
    wrapper.unmount();
  });

  it('falls back to the Close button when there is nothing else', async () => {
    const wrapper = mount(Modal, { attachTo: document.body, props: { title: 'Notice' }, slots: { default: () => h('p', 'Text') } });
    await flushPromises();
    expect(document.activeElement?.getAttribute('aria-label')).toBe('Close');
    wrapper.unmount();
  });
});

describe('Modal keys and focus', () => {
  // jsdom has no layout: every element counts as hidden unless it reports an offset parent.
  const original = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetParent');
  beforeAll(() => {
    Object.defineProperty(HTMLElement.prototype, 'offsetParent', { configurable: true, get: () => document.body });
  });
  afterAll(() => {
    if (original) Object.defineProperty(HTMLElement.prototype, 'offsetParent', original);
  });
  afterEach(() => {
    document.body.innerHTML = '';
  });

  /** A control of the page behind the dialogs; it counts the keys that reach it. */
  function controlBehind(): { button: HTMLButtonElement; keys: string[] } {
    const button = document.createElement('button');
    const keys: string[] = [];
    button.textContent = 'Behind';
    button.addEventListener('keydown', (event) => keys.push(event.key));
    document.body.append(button);
    return { button, keys };
  }

  it('closes on Escape when the focus is on the body or on a control behind the dialog', async () => {
    const behind = controlBehind();
    const wrapper = mountModal('Edit', ['One', 'Two']);
    await flushPromises();
    (document.activeElement as HTMLElement).blur();
    expect(document.activeElement).toBe(document.body);
    expect(press(document.body, 'Escape').defaultPrevented).toBe(true);
    expect(wrapper.emitted('close')).toHaveLength(1);

    behind.button.focus();
    press(behind.button, 'Escape');
    expect(wrapper.emitted('close')).toHaveLength(2);
    expect(behind.keys).toEqual([]);
    wrapper.unmount();
  });

  it('closes once on Escape from inside the panel', async () => {
    const wrapper = mountModal('Edit', ['One', 'Two']);
    await flushPromises();
    press(document.activeElement as HTMLElement, 'Escape');
    expect(wrapper.emitted('close')).toHaveLength(1);
    wrapper.unmount();
  });

  it('moves Tab into the panel when the focus is outside it', async () => {
    const behind = controlBehind();
    const wrapper = mountModal('Edit', ['One', 'Two']);
    await flushPromises();
    (document.activeElement as HTMLElement).blur();
    expect(press(document.body, 'Tab').defaultPrevented).toBe(true);
    expect(document.activeElement?.getAttribute('aria-label')).toBe('Close');

    (document.activeElement as HTMLElement).blur();
    press(document.body, 'Tab', { shiftKey: true });
    expect(document.activeElement?.textContent).toBe('Two');

    behind.button.focus();
    press(behind.button, 'Tab');
    expect(document.activeElement?.getAttribute('aria-label')).toBe('Close');
    expect(behind.keys).toEqual([]);
    wrapper.unmount();
  });

  it('stops a key aimed at a control behind the dialog and takes the focus back', async () => {
    const behind = controlBehind();
    const wrapper = mountModal('Edit', ['One', 'Two']);
    await flushPromises();
    const two = [...document.querySelectorAll('button')].find((b) => b.textContent === 'Two') as HTMLButtonElement;
    two.focus();
    behind.button.focus();
    const enter = press(behind.button, 'Enter');
    expect(enter.defaultPrevented).toBe(true);
    expect(behind.keys).toEqual([]);
    // Back on the control of the dialog that had the focus last.
    expect(document.activeElement).toBe(two);
    expect(wrapper.emitted('close')).toBeUndefined();

    // A shortcut with the focus on nothing passes, as it does from inside the dialog.
    two.blur();
    expect(press(document.body, 's', { metaKey: true }).defaultPrevented).toBe(false);
    // A typed key does not: it would land in the control that gets the focus.
    expect(press(document.body, 'a').defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(two);
    wrapper.unmount();
  });

  it('takes the focus back when it leaves the panel, and starts over when the focused control is gone', async () => {
    const wrapper = mountModal('Edit', ['One', 'Two']);
    await flushPromises();
    const two = [...document.querySelectorAll('button')].find((b) => b.textContent === 'Two') as HTMLButtonElement;
    two.focus();
    // A click on the backdrop moves the focus to the body.
    two.blur();
    expect(document.activeElement).toBe(document.body);
    await afterFocusMoved();
    expect(document.activeElement).toBe(two);

    // The focused control is removed: the next key finds the focus outside and puts it on the first control.
    two.remove();
    expect(document.activeElement).toBe(document.body);
    press(document.body, 'ArrowDown');
    expect(document.activeElement?.textContent).toBe('One');
    wrapper.unmount();
  });

  it('with two dialogs open only the top one reacts', async () => {
    const lower = mountModal('Lower', ['Lower action']);
    await flushPromises();
    const upper = mountModal('Upper', ['Upper action']);
    await flushPromises();
    expect(document.activeElement?.textContent).toBe('Upper action');

    // Focus on the body, and focus on a control of the lower dialog: Escape goes to the upper one.
    (document.activeElement as HTMLElement).blur();
    press(document.body, 'Escape');
    const lowerAction = [...document.querySelectorAll('button')].find((b) => b.textContent === 'Lower action') as HTMLButtonElement;
    lowerAction.focus();
    press(lowerAction, 'Escape');
    expect(upper.emitted('close')).toHaveLength(2);
    expect(lower.emitted('close')).toBeUndefined();

    // Tab from the lower dialog lands in the upper one.
    lowerAction.focus();
    press(lowerAction, 'Tab');
    expect(document.activeElement?.closest('[role="dialog"]')?.textContent).toContain('Upper');

    // The upper one closes: the lower one is on top again and gets its focus and keys back.
    upper.unmount();
    expect(document.activeElement).toBe(lowerAction);
    press(document.body, 'Escape');
    expect(lower.emitted('close')).toHaveLength(1);
    lower.unmount();
    // No dialog left: keys are not touched any more.
    expect(press(document.body, 'Escape').defaultPrevented).toBe(false);
  });

  it('leaves a menu that opened above the dialog its own keys and focus', async () => {
    const wrapper = mountModal('Edit', ['One']);
    await flushPromises();
    // A context menu opened from the dialog is teleported to the body after it.
    const menu = document.createElement('div');
    const item = document.createElement('button');
    const keys: string[] = [];
    item.addEventListener('keydown', (event) => keys.push(event.key));
    menu.append(item);
    document.body.append(menu);
    item.focus();
    await afterFocusMoved();
    expect(document.activeElement).toBe(item);
    expect(press(item, 'Escape').defaultPrevented).toBe(false);
    expect(press(item, 'ArrowDown').defaultPrevented).toBe(false);
    expect(keys).toEqual(['Escape', 'ArrowDown']);
    expect(wrapper.emitted('close')).toBeUndefined();
    wrapper.unmount();
  });

  it('is hidden with the tab that owns it: no focus, no keys, and its state and focus come back when shown', async () => {
    const tabVisible = ref(true);
    const wrapper = mount(Modal, {
      attachTo: document.body,
      props: { title: 'Review changes' },
      slots: { default: () => [h('input', { class: 'note' }), h('button', { type: 'button' }, 'Apply')] },
      global: { provide: { [TAB_VISIBLE_KEY as symbol]: tabVisible } },
    });
    await flushPromises();
    const backdrop = document.querySelector('.backdrop') as HTMLElement;
    const note = document.querySelector('.note') as HTMLInputElement;
    const apply = [...document.querySelectorAll('button')].find((b) => b.textContent === 'Apply') as HTMLButtonElement;
    note.value = 'typed';
    apply.focus();
    expect(backdrop.style.display).toBe('');

    // Another tab is activated (the quit question, a closed connection).
    tabVisible.value = false;
    await nextTick();
    expect(backdrop.style.display).toBe('none');
    expect(document.activeElement).toBe(document.body);
    await afterFocusMoved();
    expect(document.activeElement).toBe(document.body);
    expect(press(document.body, 'Escape').defaultPrevented).toBe(false);
    expect(press(document.body, 'Tab').defaultPrevented).toBe(false);
    expect(wrapper.emitted('close')).toBeUndefined();
    (wrapper.vm as unknown as { focusInitial: () => void }).focusInitial();
    expect(document.activeElement).toBe(document.body);

    tabVisible.value = true;
    await flushPromises();
    expect(backdrop.style.display).toBe('');
    expect(note.value).toBe('typed');
    expect(document.activeElement).toBe(apply);
    press(document.body, 'Escape');
    expect(wrapper.emitted('close')).toHaveLength(1);
    wrapper.unmount();
  });

  it('a dialog of a hidden tab does not take the keys of a dialog that is shown', async () => {
    const shown = mountModal('Unsaved changes', ['Cancel']);
    await flushPromises();
    // Opened later (so it would be on top), but its tab is hidden.
    const hidden = mount(Modal, {
      attachTo: document.body,
      props: { title: 'Review changes' },
      slots: { default: () => h('button', { type: 'button' }, 'Apply') },
      global: { provide: { [TAB_VISIBLE_KEY as symbol]: ref(false) } },
    });
    await flushPromises();
    expect(document.activeElement?.textContent).toBe('Cancel');
    press(document.activeElement as HTMLElement, 'Escape');
    (document.activeElement as HTMLElement).blur();
    press(document.body, 'Escape');
    expect(shown.emitted('close')).toHaveLength(2);
    expect(hidden.emitted('close')).toBeUndefined();
    // Closing the hidden one does not move the focus.
    await afterFocusMoved();
    const cancel = document.activeElement;
    hidden.unmount();
    expect(document.activeElement).toBe(cancel);
    shown.unmount();
  });
});
