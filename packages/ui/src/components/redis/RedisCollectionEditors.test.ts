import { mount } from '@vue/test-utils';
import { reactive } from 'vue';
import type { FieldValue, ListItem, ScoredMember } from '../../api/redis';
import { EditDraft } from '../../lib/redisEdit';
import RedisHashEditor from './RedisHashEditor.vue';
import RedisListEditor from './RedisListEditor.vue';
import RedisSetEditor from './RedisSetEditor.vue';
import RedisZsetEditor from './RedisZsetEditor.vue';

const text = (v: string) => ({ t: 'text' as const, v });
const member = (name: string, score: number): ScoredMember => ({ member: text(name), score: { t: 'int', v: score } });

function draft(): EditDraft {
  return reactive(new EditDraft()) as EditDraft;
}

function button(wrapper: ReturnType<typeof mount>, label: string) {
  return wrapper.findAll('button').find((b) => b.text() === label);
}

describe('RedisZsetEditor', () => {
  it('focuses the score input, and drops an open edit when another page replaces the rows', async () => {
    const d = draft();
    const wrapper = mount(RedisZsetEditor, {
      props: { members: [member('a', 1), member('b', 2), member('c', 3)], draft: d, editable: true },
      attachTo: document.body,
    });
    await wrapper.find('button[aria-label="Change the score of c"]').trigger('click');
    const input = wrapper.find('input.inline');
    expect(document.activeElement).toBe(input.element);
    await input.setValue('99');

    // "Next" shows other members in the same rows: the open edit does not move to row 3 of the new page.
    await wrapper.setProps({ members: [member('x', 10), member('y', 11), member('z', 12)] });
    expect(wrapper.find('input.inline').exists()).toBe(false);
    expect(d.size).toBe(0);
    wrapper.unmount();
  });

  it('commits a score edit on blur, for the member that was edited', async () => {
    const d = draft();
    const wrapper = mount(RedisZsetEditor, { props: { members: [member('a', 1), member('b', 2)], draft: d, editable: true }, attachTo: document.body });
    await wrapper.find('button[aria-label="Change the score of b"]').trigger('click');
    await wrapper.find('input.inline').setValue('7');
    await wrapper.find('input.inline').trigger('blur');
    expect(d.edits).toEqual([{ op: 'zsetScore', member: text('b'), score: { t: 'int', v: 7 }, original: { t: 'int', v: 2 } }]);
    wrapper.unmount();
  });

  it('does not stage an empty member unless asked explicitly', async () => {
    const d = draft();
    const wrapper = mount(RedisZsetEditor, { props: { members: [], draft: d, editable: true } });
    await wrapper.find('input[placeholder="score"]').setValue('1');
    await wrapper.find('form').trigger('submit');
    expect(d.size).toBe(0);
    expect(wrapper.text()).toContain('Enter a member.');
    await button(wrapper, 'Add empty member')?.trigger('click');
    expect(d.edits).toEqual([{ op: 'zsetAdd', member: text(''), score: { t: 'int', v: 1 } }]);
    wrapper.unmount();
  });
});

describe('RedisSetEditor and RedisListEditor', () => {
  it('asks before adding an empty set member', async () => {
    const d = draft();
    const wrapper = mount(RedisSetEditor, { props: { members: [], draft: d, editable: true } });
    await wrapper.find('form').trigger('submit');
    expect(d.size).toBe(0);
    expect(wrapper.text()).toContain('Enter a member.');
    await button(wrapper, 'Add empty member')?.trigger('click');
    expect(d.edits).toEqual([{ op: 'setAdd', members: [text('')] }]);
    await wrapper.find('input').setValue('m');
    await wrapper.find('form').trigger('submit');
    expect(d.size).toBe(2);
    wrapper.unmount();
  });

  it('asks before pushing an empty list element', async () => {
    const d = draft();
    const items: ListItem[] = [{ index: '0', value: text('one') }];
    const wrapper = mount(RedisListEditor, { props: { items, draft: d, editable: true } });
    await button(wrapper, 'Push left (LPUSH)')?.trigger('click');
    await wrapper.find('form').trigger('submit');
    expect(d.size).toBe(0);
    expect(wrapper.text()).toContain('Enter an element.');
    await button(wrapper, 'Push empty right')?.trigger('click');
    expect(d.edits).toEqual([{ op: 'listPush', end: 'tail', values: [text('')] }]);
    await wrapper.find('input[placeholder="New element"]').setValue('two');
    await button(wrapper, 'Push left (LPUSH)')?.trigger('click');
    expect(d.edits.at(-1)).toEqual({ op: 'listPush', end: 'head', values: [text('two')] });
    wrapper.unmount();
  });
});

describe('RedisHashEditor', () => {
  it('focuses the input and stages the change for the field it was opened on', async () => {
    const d = draft();
    const entries: FieldValue[] = [
      { field: text('name'), value: text('Kim') },
      { field: text('city'), value: text('Utrecht') },
    ];
    const wrapper = mount(RedisHashEditor, { props: { entries, draft: d, editable: true }, attachTo: document.body });
    await wrapper.find('button[aria-label="Edit city"]').trigger('click');
    expect(document.activeElement).toBe(wrapper.find('input.inline').element);
    await wrapper.find('input.inline').setValue('Delft');
    await wrapper.find('input.inline').trigger('keydown', { key: 'Enter' });
    expect(d.edits).toEqual([{ op: 'hashSet', field: text('city'), value: text('Delft'), original: text('Utrecht') }]);
    wrapper.unmount();
  });
});
