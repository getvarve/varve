import { flushPromises, mount } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { KeyEntry, ValueChunk } from '../../api/redis';
import { useDialogs } from '../../composables/useDialogs';
import ValueViewer from './ValueViewer.vue';

const MiB = 1024 * 1024;
let size = 0;
const cursors: string[] = [];
const writes: { path: string; data: string; append: boolean }[] = [];
const byteWrites: { path: string; data: number[]; append: boolean }[] = [];
/** Which chunks arrive as bytes (not valid UTF-8): none, the first, or both. */
let binary: 'none' | 'first' | 'all' = 'none';

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn(async () => '/tmp/value.txt') }));
vi.mock('@tauri-apps/plugin-fs', () => ({
  writeFile: vi.fn(async (path: string, data: Uint8Array, options?: { append?: boolean }) => {
    byteWrites.push({ path, data: [...data], append: options?.append ?? false });
  }),
  writeTextFile: vi.fn(async (path: string, data: string, options?: { append?: boolean }) => {
    writes.push({ path, data, append: options?.append ?? false });
  }),
}));

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisKeyInfo: vi.fn(async () => ({ keyType: 'string', ttlMs: null, memoryBytes: String(size), encoding: 'raw', length: String(size) })),
  redisKeyVersion: vi.fn(async () => ({ keyType: 'string', length: String(size), digest: 'd', digestMissing: null, expireAtMs: null })),
  // Two chunks: "first part" (more to come), then "rest" (the end).
  redisReadValue: vi.fn(async (_s: string, _k: string, page: { cursor: string }): Promise<ValueChunk> => {
    cursors.push(page.cursor);
    const first = page.cursor === '';
    const asBytes = binary === 'all' || (binary === 'first' && first);
    const value = asBytes ? { t: 'bytes' as const, v: first ? 'ff00e2' : '82ac' } : { t: 'text' as const, v: first ? 'first part' : 'rest' };
    return {
      keyType: 'string',
      total: String(size),
      nextCursor: first ? 'c1' : null,
      data: { kind: 'string', value, offset: first ? '0' : '10', truncated: first },
    };
  }),
}));

const entry: KeyEntry = { name: '626967', display: 'big', keyType: 'string', ttlMs: null };

function buttonByText(text: string): HTMLButtonElement | undefined {
  return [...document.body.querySelectorAll('button')].find((b) => b.textContent?.trim() === text);
}

async function open() {
  cursors.length = 0;
  const wrapper = mount(ValueViewer, { props: { tabId: 't1', sessionId: 's1', entry }, attachTo: document.body });
  await flushPromises();
  return wrapper;
}

describe('ValueViewer whole string loading', () => {
  it('loads up to 16 MiB at once', async () => {
    size = 2 * MiB;
    const wrapper = await open();
    buttonByText('Load the whole value')?.click();
    await flushPromises();
    expect(cursors).toEqual(['', 'c1']);
    expect(useDialogs().dialogs).toHaveLength(0);
    wrapper.unmount();
  });

  it('asks before loading more than 16 MiB, and loads nothing when declined', async () => {
    size = 20 * MiB;
    const wrapper = await open();
    buttonByText('Load the whole value')?.click();
    await flushPromises();
    const dialogs = useDialogs();
    const dialog = dialogs.dialogs.at(-1);
    expect(dialog?.spec.type === 'confirm' ? dialog.spec.message : '').toContain('20');
    if (dialog) dialogs.close(dialog.id, false);
    await flushPromises();
    expect(cursors).toEqual(['']);

    buttonByText('Load the whole value')?.click();
    await flushPromises();
    const again = dialogs.dialogs.at(-1);
    if (again) dialogs.close(again.id, true);
    await flushPromises();
    expect(cursors).toEqual(['', 'c1']);
    wrapper.unmount();
  });

  it('refuses editing above 64 MiB and saves the value to a file chunk by chunk instead', async () => {
    size = 100 * MiB;
    writes.length = 0;
    const wrapper = await open();
    expect(wrapper.text()).toContain('too large to edit here');
    expect(buttonByText('Load the whole value')).toBeUndefined();
    const edit = wrapper.findAll('button').find((b) => b.text() === 'Edit');
    expect(edit?.attributes('disabled')).toBeDefined();

    buttonByText('Save value to file')?.click();
    await flushPromises();
    expect(writes).toEqual([
      { path: '/tmp/value.txt', data: 'first part', append: false },
      { path: '/tmp/value.txt', data: 'rest', append: true },
    ]);
    // Saving never put the rest of the value into the view.
    expect(wrapper.text()).not.toContain('rest');
    wrapper.unmount();
  });

  it('saves binary chunks as raw bytes and text chunks as text, in order', async () => {
    size = 100 * MiB;
    writes.length = 0;
    byteWrites.length = 0;
    binary = 'first';
    const wrapper = await open();
    buttonByText('Save value to file')?.click();
    await flushPromises();
    expect(byteWrites).toEqual([{ path: '/tmp/value.txt', data: [0xff, 0x00, 0xe2], append: false }]);
    expect(writes).toEqual([{ path: '/tmp/value.txt', data: 'rest', append: true }]);
    wrapper.unmount();

    // A wholly binary value: every chunk goes through writeFile, the first one creating the file.
    byteWrites.length = 0;
    writes.length = 0;
    binary = 'all';
    const again = await open();
    buttonByText('Save value to file')?.click();
    await flushPromises();
    expect(byteWrites).toEqual([
      { path: '/tmp/value.txt', data: [0xff, 0x00, 0xe2], append: false },
      { path: '/tmp/value.txt', data: [0x82, 0xac], append: true },
    ]);
    expect(writes).toEqual([]);
    binary = 'none';
    again.unmount();
  });
});
