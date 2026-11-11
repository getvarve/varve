import { flushPromises, mount } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { KeyEntry, RedisEditRequest, ValueChunk } from '../../api/redis';
import RedisHashEditor from './RedisHashEditor.vue';
import ValueViewer from './ValueViewer.vue';

const plans: RedisEditRequest[] = [];
let reads = 0;
let versionFails = false;
let digestMissing: 'tooLarge' | 'notPermitted' | 'deferred' | null = null;
let digestRead: string | null = null;
/** The content digest of the key; another one means it changed on the server. */
let digest = 'd1';
const cancelled: [string, string][] = [];
/** A key whose version read hangs until the test fails it. */
let slowVersionKey: string | null = null;
let failSlowVersion: (e: unknown) => void = () => undefined;

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisKeyInfo: vi.fn(async () => ({ keyType: 'hash', ttlMs: null, memoryBytes: '120', encoding: 'listpack', length: '2' })),
  redisKeyVersion: vi.fn(async (_s: string, key: string) => {
    if (key === slowVersionKey) {
      return new Promise((_resolve, reject) => {
        failSlowVersion = reject;
      });
    }
    if (versionFails) throw { kind: 'internal', message: 'no version' };
    const version = { keyType: 'hash', length: '2', digest: digestMissing ? null : digest, digestMissing, expireAtMs: '1792500000000' };
    return digestRead ? { ...version, digestRead } : version;
  }),
  redisKeyDigestCancel: vi.fn(async (sessionId: string, readId: string) => {
    cancelled.push([sessionId, readId]);
  }),
  redisReadValue: vi.fn(async (): Promise<ValueChunk> => {
    reads += 1;
    return {
      keyType: 'hash',
      total: '2',
      nextCursor: null,
      data: {
        kind: 'hash',
        entries: [
          { field: { t: 'text', v: 'name' }, value: { t: 'text', v: 'Kim' } },
          { field: { t: 'text', v: 'city' }, value: { t: 'text', v: 'Utrecht' } },
        ],
      },
    };
  }),
  redisEditPlan: vi.fn(async (_s: string, request: RedisEditRequest) => {
    plans.push(request);
    return {
      planId: 'p9',
      plan: { key: request.key, db: 0, commands: ['HSET user:1 name Sam'], checks: ['name is still Kim'], risk: 'write', destructive: false, warnings: [] },
    };
  }),
  redisEditApply: vi.fn(async () => ({ status: 'applied', replies: [{ t: 'int', v: 0 }] })),
}));

const entry: KeyEntry = { name: '757365723a31', display: 'user:1', keyType: 'hash', ttlMs: null };

function button(wrapper: ReturnType<typeof mount>, text: string) {
  return wrapper.findAll('button').find((b) => b.text() === text);
}

describe('ValueViewer background digest', () => {
  it('stops the background digest read when the viewer closes', async () => {
    digestMissing = 'deferred';
    digestRead = 'read-1';
    cancelled.length = 0;
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    expect(cancelled).toEqual([]);
    wrapper.unmount();
    await flushPromises();
    expect(cancelled).toEqual([['s1', 'read-1']]);
    digestMissing = null;
    digestRead = null;
  });
});

describe('ValueViewer editing', () => {
  it('stages a field change, shows the plan, applies it and reads the key again', async () => {
    plans.length = 0;
    reads = 0;
    versionFails = false;
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    expect(wrapper.text()).toContain('Utrecht');
    expect(reads).toBe(1);

    await wrapper.find('button[aria-label="Edit name"]').trigger('click');
    const input = wrapper.find('input.inline');
    await input.setValue('Sam');
    await input.trigger('keydown', { key: 'Enter' });
    expect(wrapper.find('.savebar').text()).toContain('Change field name');

    await button(wrapper, 'Review and save')?.trigger('click');
    await flushPromises();
    expect(plans[0]).toEqual({
      key: 'user:1',
      expect: { state: 'present', keyType: 'hash', version: { keyType: 'hash', length: '2', digest: 'd1', digestMissing: null, expireAtMs: '1792500000000' } },
      edits: [{ op: 'hashSet', field: { t: 'text', v: 'name' }, value: { t: 'text', v: 'Sam' }, original: { t: 'text', v: 'Kim' } }],
    });
    expect(document.body.textContent).toContain('HSET user:1 name Sam');

    const apply = [...document.body.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'Apply');
    apply?.click();
    await flushPromises();
    expect(wrapper.text()).toContain('1 change applied.');
    expect(wrapper.find('.savebar').exists()).toBe(false);
    expect(reads).toBe(2);
    wrapper.unmount();
  });

  it('sends the version with a TTL change and says when the key cannot be fully checked', async () => {
    plans.length = 0;
    versionFails = false;
    digestMissing = 'tooLarge';
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    expect(wrapper.text()).toContain('too large to read in full');

    await button(wrapper, 'TTL')?.trigger('click');
    await flushPromises();
    // The dialog is teleported to the body.
    const seconds = document.body.querySelector<HTMLInputElement>('input[inputmode="numeric"]');
    expect(seconds).not.toBeNull();
    if (seconds) {
      seconds.value = '60';
      seconds.dispatchEvent(new Event('input'));
    }
    await flushPromises();
    document.body.querySelector('form.content')?.dispatchEvent(new Event('submit', { cancelable: true }));
    // vee-validate validates asynchronously before the dialog answers.
    for (let i = 0; i < 5; i += 1) {
      await flushPromises();
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
    expect(plans[0]?.edits).toEqual([{ op: 'expire', ttlMs: '60000' }]);
    expect(plans[0]?.expect).toEqual({
      state: 'present',
      keyType: 'hash',
      version: { keyType: 'hash', length: '2', digest: null, digestMissing: 'tooLarge', expireAtMs: '1792500000000' },
    });
    digestMissing = null;
    wrapper.unmount();
    document.body.innerHTML = '';
  });

  it('turns editing off on read-only connections and without a key version', async () => {
    const ro = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry, readOnly: true } });
    await flushPromises();
    expect(ro.find('button[aria-label="Edit name"]').exists()).toBe(false);
    expect(button(ro, 'Rename')).toBeUndefined();
    ro.unmount();

    versionFails = true;
    const noVersion = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry } });
    await flushPromises();
    expect(noVersion.text()).toContain('Editing is off: no version');
    expect(noVersion.text()).toContain('Utrecht');
    noVersion.unmount();
  });
});

describe('ValueViewer reading again', () => {
  it('keeps the type view mounted while the same key is read again', async () => {
    versionFails = false;
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    const before = reads;
    const editor = wrapper.findComponent(RedisHashEditor).vm.$.uid;
    await wrapper.find('button[aria-label="Reload key"]').trigger('click');
    // While the read runs, the value stays on screen.
    expect(wrapper.text()).toContain('Utrecht');
    await flushPromises();
    expect(reads).toBe(before + 1);
    expect(wrapper.findComponent(RedisHashEditor).vm.$.uid).toBe(editor);
    expect(wrapper.findAll('tbody tr')).toHaveLength(2);
    wrapper.unmount();
  });

  it('ignores a late version failure of the key shown before', async () => {
    versionFails = false;
    slowVersionKey = 'user:1';
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    await wrapper.setProps({ entry: { name: '757365723a32', display: 'user:2', keyType: 'hash', ttlMs: null } });
    await flushPromises();
    expect(button(wrapper, 'Rename')).toBeDefined();

    failSlowVersion({ kind: 'internal', message: 'timed out' });
    await flushPromises();
    expect(wrapper.text()).not.toContain('Editing is off');
    expect(button(wrapper, 'Rename')).toBeDefined();
    slowVersionKey = null;
    wrapper.unmount();
  });
});

describe('ValueViewer on a new session', () => {
  async function staged() {
    versionFails = false;
    digestMissing = null;
    digest = 'd1';
    const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    await wrapper.find('button[aria-label="Edit name"]').trigger('click');
    await wrapper.find('input.inline').setValue('Sam');
    await wrapper.find('input.inline').trigger('keydown', { key: 'Enter' });
    expect(wrapper.find('.savebar').text()).toContain('Change field name');
    return wrapper;
  }

  it('keeps staged edits and reads the key again when the tab connected again', async () => {
    const wrapper = await staged();
    const before = reads;
    await wrapper.setProps({ sessionId: 's2' });
    await flushPromises();
    expect(reads).toBe(before + 1);
    expect(wrapper.find('.savebar').text()).toContain('Change field name');
    // Unchanged on the server: nothing to say.
    expect(wrapper.find('.notice.outcome').exists()).toBe(false);

    // The edit is planned on the new session against the version read there.
    plans.length = 0;
    await button(wrapper, 'Review and save')?.trigger('click');
    await flushPromises();
    expect(plans[0]?.edits).toHaveLength(1);
    wrapper.unmount();
    document.body.innerHTML = '';
  });

  it('warns instead of dropping the edits when the key changed meanwhile', async () => {
    const wrapper = await staged();
    digest = 'd2';
    await wrapper.setProps({ sessionId: 's2' });
    await flushPromises();
    expect(wrapper.find('.savebar').text()).toContain('Change field name');
    expect(wrapper.text()).toContain('The key changed on the server');
    expect(wrapper.text()).toContain('Your unsaved changes are kept');
    digest = 'd1';
    wrapper.unmount();
  });

  it('says nothing about a changed key when nothing was staged, and drops edits for another key', async () => {
    digest = 'd1';
    const plain = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
    await flushPromises();
    digest = 'd2';
    await plain.setProps({ sessionId: 's2' });
    await flushPromises();
    expect(plain.text()).not.toContain('changed on the server');
    plain.unmount();
    digest = 'd1';

    const wrapper = await staged();
    await wrapper.setProps({ sessionId: 's2', entry: { name: '757365723a32', display: 'user:2', keyType: 'hash', ttlMs: null } });
    await flushPromises();
    expect(wrapper.find('.savebar').exists()).toBe(false);
    wrapper.unmount();
  });
});

