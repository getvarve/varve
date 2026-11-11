import { flushPromises, mount } from '@vue/test-utils';
import type { ExplainCapabilities, Plan, PlanNode } from '../../api/explain';
import type { SessionInfo } from '../../api/sessions';
import { useTabs } from '../../composables/useTabs';
import type { WorkspaceTab } from '../../composables/useTabs';
import ExplainView from './ExplainView.vue';

let caps: ExplainCapabilities;
const calls: { command: string; args: Record<string, unknown> }[] = [];
let answer: (command: string, args: Record<string, unknown>) => Promise<unknown>;

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async (command: string, args: Record<string, unknown>) => {
    calls.push({ command, args });
    return answer(command, args);
  }),
}));

function node(id: number, extra: Partial<PlanNode> = {}): PlanNode {
  return {
    id,
    operation: 'Seq Scan',
    detail: {
      relation: 'orders',
      schema: null,
      alias: null,
      index: null,
      joinType: null,
      strategy: null,
      relationship: null,
      subplan: null,
      cte: null,
      function: null,
      filter: null,
      condition: null,
      sortKey: [],
      groupKey: [],
      parallelAware: false,
      workersPlanned: null,
      workersLaunched: null,
      output: [],
    },
    estimatedRows: 10,
    actualRows: null,
    loops: null,
    neverExecuted: false,
    estimatedCost: null,
    actualTime: null,
    buffers: null,
    properties: [],
    children: [],
    share: 1,
    warnings: [],
    ...extra,
  };
}

function plan(analyzed: boolean): Plan {
  return {
    format: 'postgresJson',
    analyzed,
    root: node(0, analyzed ? { actualRows: 12, loops: 1 } : {}),
    planningMs: null,
    executionMs: analyzed ? 1.5 : null,
    totalCost: 12.5,
    shareBasis: analyzed ? 'time' : 'cost',
    planningBuffers: null,
    triggers: [],
    settings: [],
    properties: [],
    warnings: [],
    notices: [],
    raw: '[]',
  };
}

const session: SessionInfo = {
  id: 's1',
  connectionId: 'c1',
  engine: 'postgres',
  server: { engine: 'postgres', version: '17.2', major: 17, minor: 2, patch: null },
  environment: 'development',
  database: 'shop',
  readOnly: false,
  confirmDestructive: false,
  purpose: 'user',
  openedAt: 0,
  encryption: { tls: false, mode: 'disable', passwordUnencrypted: false },
};

const stubs = { PlanTree: true, PlanTimeline: true, PlanNodeDetails: true, EnvBadge: true, ExplainConfirmDialog: true };

function openTab(id: string): void {
  useTabs().tabs.push({ id, kind: 'explain', statement: 'SELECT * FROM orders' } as unknown as WorkspaceTab);
}

function view(tabId: string): ReturnType<typeof mount> {
  return mount(ExplainView, {
    props: { session, connectionName: 'Shop', statement: 'SELECT * FROM orders', tabId, autoRun: 'explain' },
    global: { stubs },
  });
}

function explainCalls(): { analyze: boolean; format: unknown }[] {
  return calls
    .filter((c) => c.command === 'explain')
    .map((c) => {
      const options = c.args.options as { analyze: boolean; format: unknown };
      return { analyze: options.analyze, format: options.format };
    });
}

function button(wrapper: ReturnType<typeof mount>, text: string): HTMLButtonElement {
  const found = wrapper.findAll('button').find((b) => b.text().trim() === text);
  if (!found) throw new Error(`no button ${text}`);
  return found.element as HTMLButtonElement;
}

describe('ExplainView per tab state', () => {
  beforeEach(() => {
    calls.length = 0;
    useTabs().tabs.splice(0);
    caps = { formats: ['json'], analyze: true, analyzeFormats: ['json'], buffers: true, verbose: true, settings: true, wal: true };
    answer = async (command, args) => {
      if (command === 'explain_capabilities') return caps;
      return plan((args.options as { analyze: boolean }).analyze);
    };
  });

  it('keeps an EXPLAIN ANALYZE result and the plan view when the view is mounted again for its tab', async () => {
    openTab('t1');
    const first = view('t1');
    await flushPromises();
    expect(explainCalls()).toEqual([{ analyze: false, format: null }]);
    button(first, 'Explain analyze').click();
    await flushPromises();
    button(first, 'Text').click();
    await flushPromises();
    first.unmount();

    const again = view('t1');
    await flushPromises();
    // No second automatic EXPLAIN replaced the analyzed plan.
    expect(explainCalls()).toEqual([
      { analyze: false, format: null },
      { analyze: true, format: null },
    ]);
    expect(again.text()).toContain('EXPLAIN (ANALYZE');
    expect(again.text()).toContain('Execution 1.50 ms');
    expect(again.find('button[aria-pressed="true"]').text()).toBe('Text');
    again.unmount();
  });

  it('shows a run still in progress, with Cancel, in a view mounted again', async () => {
    openTab('t2');
    let finish: (p: Plan) => void = () => undefined;
    answer = async (command) => {
      if (command === 'explain_capabilities') return caps;
      return new Promise<Plan>((resolve) => {
        finish = resolve;
      });
    };
    const first = view('t2');
    await flushPromises();
    first.unmount();
    const again = view('t2');
    await flushPromises();
    expect(explainCalls()).toHaveLength(1);
    expect(again.text()).toContain('Cancel');
    finish(plan(false));
    await flushPromises();
    expect(again.text()).not.toContain('Cancel');
    expect(again.text()).toContain('Copy plan');
    again.unmount();
  });

  it('drops the state of a closed tab', async () => {
    openTab('t3');
    const first = view('t3');
    await flushPromises();
    first.unmount();
    // t3 closes; the next view (of another tab) drops its state.
    useTabs().tabs.splice(0);
    openTab('t4');
    view('t4').unmount();
    await flushPromises();
    expect(explainCalls()).toHaveLength(2);
    openTab('t3');
    const again = view('t3');
    await flushPromises();
    expect(explainCalls()).toHaveLength(3);
    again.unmount();
  });
});

describe('ExplainView options', () => {
  beforeEach(() => {
    calls.length = 0;
    useTabs().tabs.splice(0);
    answer = async (command, args) => {
      if (command === 'explain_capabilities') return caps;
      return plan((args.options as { analyze: boolean }).analyze);
    };
  });

  it('offers only the formats the server has and marks the ones one mode lacks', async () => {
    caps = { formats: ['tree', 'json'], analyze: true, analyzeFormats: ['tree'], buffers: false, verbose: false, settings: false, wal: false };
    openTab('f1');
    const wrapper = view('f1');
    await flushPromises();
    await wrapper.find('button[aria-label^="Explain options"]').trigger('click');
    const labels = wrapper.findAll('.options option').map((o) => o.text());
    expect(labels).toEqual(['Server default', 'Tree', 'JSON (plain EXPLAIN only)']);
    wrapper.unmount();
  });

  it('falls back to the share highlight when the new plan lacks what the chosen one needs', async () => {
    caps = { formats: ['json'], analyze: true, analyzeFormats: ['json'], buffers: false, verbose: false, settings: false, wal: false };
    openTab('h1');
    const wrapper = view('h1');
    await flushPromises();
    button(wrapper, 'Explain analyze').click();
    await flushPromises();
    // The select's id is generated (two EXPLAIN tabs can be mounted at once): found through its label.
    const highlight = () => wrapper.find(`#${wrapper.findAll('label').find((l) => l.text() === 'Highlight')?.attributes('for') ?? 'missing'}`);
    await highlight().setValue('misestimate');
    button(wrapper, 'Explain').click();
    await flushPromises();
    expect((highlight().element as HTMLSelectElement).value).toBe('share');
    wrapper.unmount();
  });
});
