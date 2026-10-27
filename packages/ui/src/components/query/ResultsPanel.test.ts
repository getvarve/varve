import { flushPromises, mount } from '@vue/test-utils';
import type { RunResult } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import ResultsPanel from './ResultsPanel.vue';

const writes: { path: string; data: string }[] = [];
const invoked: { command: string; args: unknown }[] = [];
let recordFails = false;

vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn(async () => '/Users/kim/exports/orders.csv') }));
vi.mock('@tauri-apps/plugin-fs', () => ({
  writeTextFile: vi.fn(async (path: string, data: string) => {
    writes.push({ path, data });
  }),
}));
vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async (command: string, args: unknown) => {
    invoked.push({ command, args });
    if (recordFails) throw { kind: 'internal', message: 'disk full' };
    return null;
  }),
}));

const run: RunResult = {
  output: {
    results: [
      {
        kind: 'rows',
        columns: [{ name: 'id', typeName: 'int4' }],
        rows: [[{ t: 'int', v: 1 }], [{ t: 'int', v: 2 }]],
        truncated: false,
      },
    ],
    notices: [],
    elapsedMs: 3,
  },
  error: null,
  offset: 0,
  text: 'SELECT id FROM orders',
  doc: 'SELECT id FROM orders',
  id: 1,
  startedAt: 0,
};

async function exportCsv(): Promise<void> {
  const wrapper = mount(ResultsPanel, {
    attachTo: document.body,
    props: { run, running: false, errorLocation: null, exportName: 'Orders', engine: 'postgres', connectionId: 'c1' },
    global: { stubs: { ResultGrid: true } },
  });
  await flushPromises();
  const tab = [...document.body.querySelectorAll<HTMLButtonElement>('button[role="tab"]')].find((b) => b.textContent?.trim().startsWith('Result'));
  if (!tab) throw new Error('no result tab');
  tab.click();
  await flushPromises();
  const button = [...document.body.querySelectorAll('button')].find((b) => b.textContent?.trim() === 'CSV');
  if (!button) throw new Error('no CSV export button');
  button.click();
  for (let i = 0; i < 10; i += 1) await flushPromises();
  wrapper.unmount();
}

describe('ResultsPanel export', () => {
  beforeEach(() => {
    writes.length = 0;
    invoked.length = 0;
    recordFails = false;
  });

  it('records data.exported with the file, format, size and rows after writing the file', async () => {
    await exportCsv();
    expect(writes).toHaveLength(1);
    const record = invoked.find((c) => c.command === 'record_export');
    expect(record?.args).toEqual({
      record: {
        connectionId: 'c1',
        objects: ['Orders'],
        fileName: '/Users/kim/exports/orders.csv',
        format: 'csv',
        sizeBytes: new TextEncoder().encode(writes[0]?.data ?? '').length,
        rows: 2,
      },
    });
  });

  it('keeps a written export a success when recording it fails', async () => {
    recordFails = true;
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    await exportCsv();
    expect(writes).toHaveLength(1);
    const toasts = useToasts().toasts;
    expect(toasts.some((t) => t.title === 'Export failed')).toBe(false);
    expect(toasts.at(-1)?.tone).toBe('success');
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });
});

describe('ResultsPanel Redis console errors', () => {
  function failed(message: string, kind: 'read_only' | 'query', code: string | null): RunResult {
    return {
      ...run,
      output: null,
      error: { kind, message, code, detail: null, hint: null, position: null },
      text: 'KEYS *',
      doc: 'KEYS *',
      id: 7,
    };
  }

  it('explains a read-only refusal of KEYS like the key browser', async () => {
    const wrapper = mount(ResultsPanel, {
      props: { run: failed('Line 1: KEYS changes data and this connection is read-only', 'read_only', null), running: false, errorLocation: null, exportName: 'c', engine: 'redis', connectionId: 'c1' },
      global: { stubs: { ResultGrid: true } },
    });
    await flushPromises();
    expect(wrapper.text()).toContain('Read-only connection');
    expect(wrapper.text()).toContain('SCAN with MATCH');
    wrapper.unmount();
  });

  it('gives MULTI hints in Redis consoles only', async () => {
    const error = failed('ERR MULTI calls can not be nested', 'query', 'ERR');
    const redis = mount(ResultsPanel, {
      props: { run: error, running: false, errorLocation: null, exportName: 'c', engine: 'redis', connectionId: 'c1' },
      global: { stubs: { ResultGrid: true } },
    });
    await flushPromises();
    expect(redis.text()).toContain('MULTI already open');
    redis.unmount();
    const sql = mount(ResultsPanel, {
      props: { run: error, running: false, errorLocation: null, exportName: 'c', engine: 'postgres', connectionId: 'c1' },
      global: { stubs: { ResultGrid: true } },
    });
    await flushPromises();
    expect(sql.text()).not.toContain('MULTI already open');
    sql.unmount();
  });
});
