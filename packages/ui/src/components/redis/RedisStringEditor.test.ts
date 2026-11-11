import { mount } from '@vue/test-utils';
import type { Value } from '../../api/sessions';
import RedisStringEditor from './RedisStringEditor.vue';

function editor(original: Value, mode: 'string' | 'json' = 'string') {
  return mount(RedisStringEditor, {
    props: { original, mode, locked: null, editable: true, busy: false },
    attachTo: document.body,
  });
}

function button(wrapper: ReturnType<typeof mount>, text: string) {
  return wrapper.findAll('button').find((b) => b.text() === text);
}

describe('RedisStringEditor', () => {
  it('focuses the value when editing starts', async () => {
    const wrapper = editor({ t: 'text', v: 'hello' });
    await button(wrapper, 'Edit')?.trigger('click');
    expect(document.activeElement).toBe(wrapper.find('textarea').element);
    wrapper.unmount();
  });

  it('writes nothing when an unusually formatted JSON string was not changed', async () => {
    const wrapper = editor({ t: 'text', v: '{"a": 1, "b": 2}' });
    await button(wrapper, 'Edit')?.trigger('click');
    // Neither compact nor the editor's own indentation: minify is off and the user is told.
    expect(wrapper.find<HTMLInputElement>('input[type="checkbox"]').element.checked).toBe(false);
    expect(wrapper.text()).toContain('The original formatting cannot be kept.');
    await wrapper.find('form').trigger('submit');
    expect(wrapper.emitted('save')).toBeUndefined();
    expect(wrapper.find('textarea').exists()).toBe(false);
    wrapper.unmount();
  });

  it('keeps a compact document compact and reports typed changes as unsaved', async () => {
    const wrapper = editor({ t: 'text', v: '{"a":1}' });
    await button(wrapper, 'Edit')?.trigger('click');
    expect(wrapper.find<HTMLInputElement>('input[type="checkbox"]').element.checked).toBe(true);
    expect(wrapper.text()).not.toContain('cannot be kept');
    expect(wrapper.emitted('dirty')).toBeUndefined();
    await wrapper.find('textarea').setValue('{\n  "a": 2\n}');
    expect(wrapper.emitted('dirty')?.at(-1)).toEqual([true]);
    await wrapper.find('form').trigger('submit');
    expect(wrapper.emitted('save')?.[0]).toEqual([{ op: 'stringSet', value: { t: 'text', v: '{"a":2}' }, original: { t: 'text', v: '{"a":1}' } }]);

    // Cancel closes the edit: nothing unsaved any more.
    await button(wrapper, 'Cancel')?.trigger('click');
    expect(wrapper.emitted('dirty')?.at(-1)).toEqual([false]);
    wrapper.unmount();
  });

  it('saves a pretty printed document again when only its text changed', async () => {
    const original = '{\n  "a": 1\n}';
    const wrapper = editor({ t: 'json', v: original }, 'json');
    await button(wrapper, 'Edit')?.trigger('click');
    expect(wrapper.find<HTMLInputElement>('input[type="checkbox"]').element.checked).toBe(false);
    await wrapper.find('form').trigger('submit');
    expect(wrapper.emitted('save')).toBeUndefined();
    await button(wrapper, 'Edit')?.trigger('click');
    await wrapper.find('textarea').setValue('{\n  "a": 3\n}');
    await wrapper.find('form').trigger('submit');
    expect(wrapper.emitted('save')?.[0]).toEqual([{ op: 'jsonSet', document: '{\n  "a": 3\n}', original }]);
    wrapper.unmount();
  });

  it('keeps typed text when the value is read again, and closes an untouched edit', async () => {
    const wrapper = editor({ t: 'text', v: 'hello' });
    await button(wrapper, 'Edit')?.trigger('click');
    // Untouched: a new value closes the edit and shows what was read.
    await wrapper.setProps({ original: { t: 'text', v: 'hello again' } });
    expect(wrapper.find('textarea').exists()).toBe(false);

    await button(wrapper, 'Edit')?.trigger('click');
    await wrapper.find('textarea').setValue('typed');
    // Read again (a reconnect): the same value or another one, the typed text stays.
    await wrapper.setProps({ original: { t: 'text', v: 'hello again' } });
    await wrapper.setProps({ original: { t: 'text', v: 'changed elsewhere' } });
    expect(wrapper.find<HTMLTextAreaElement>('textarea').element.value).toBe('typed');
    // Saving then checks against the value that is on the server now.
    await wrapper.find('form').trigger('submit');
    expect(wrapper.emitted('save')?.[0]).toEqual([{ op: 'stringSet', value: { t: 'text', v: 'typed' }, original: { t: 'text', v: 'changed elsewhere' } }]);

    // The parent drops the edit (after an apply, or a confirmed reload).
    (wrapper.vm as unknown as { close: () => void }).close();
    await wrapper.vm.$nextTick();
    expect(wrapper.find('textarea').exists()).toBe(false);
    expect(wrapper.emitted('dirty')?.at(-1)).toEqual([false]);
    wrapper.unmount();
  });
});
