/**
 * The materialized view editor: its way to its indexes (without a unique
 * index REFRESH CONCURRENTLY is refused, and "Design indexes" opens the
 * table designer on the view's indexes), its refresh history, and the
 * recreate under a temporary name (checked before it is planned, and
 * resumable when it stops part way).
 */
import { flushPromises, mount } from '@vue/test-utils';
import type { VueWrapper } from '@vue/test-utils';
import type { DropImpact, ObjectRef, PgMaterializedView } from '../../api/ddl';
import type { SessionInfo } from '../../api/sessions';
import { pendingFocus } from '../table/designerIntent';
import MatViewEditor from './MatViewEditor.vue';

const opened: unknown[][] = [];
/** The materialized views on the server, by `schema.name`. */
let views: Record<string, PgMaterializedView> = {};
let impact: DropImpact = emptyImpact();
/** Every `edit_plan`: the session it was planned on and the change. */
const planned: { sessionId: string; change: Record<string, unknown> }[] = [];
/** What ran, in order: the action of an edit step, or the text of a DDL step. */
const ran: string[] = [];
/** What a step does to `views`; throwing fails the step. */
let onRun: (step: string, change: Record<string, unknown> | null) => void = () => undefined;
const profile = { id: 'c1', name: 'Shop', engine: 'postgres' };

function emptyImpact(): DropImpact {
  return { dependents: [], dependentsTruncated: false, contents: [], sessions: [], sessionsComplete: true, blockers: [], appSessions: [], notes: [] };
}

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async (command: string, args: Record<string, unknown> = {}) => {
    if (command === 'catalog_describe') {
      const target = args.object as ObjectRef;
      const found = views[`${target.schema}.${target.name}`];
      if (!found) throw { kind: 'catalog', error: { kind: 'notFound', message: `materialized view ${target.name} not found` } };
      return { engine: 'postgres', object: { kind: 'materializedView', ...found } };
    }
    if (command === 'catalog_drop_impact') return impact;
    if (command === 'activity_query') return { events: [], total: 0, nextCursor: null };
    if (command === 'edit_plan') {
      const change = (args.change as { change: Record<string, unknown> }).change;
      planned.push({ sessionId: args.sessionId as string, change });
      return { planId: `p${planned.length}`, plan: { statements: [`-- ${String(change.action)}`], transactional: false, destructive: false, warnings: [] } };
    }
    if (command === 'edit_apply') {
      const change = planned[Number((args.planId as string).slice(1)) - 1]?.change ?? {};
      const step = change.action === 'alter' ? `alter ${String((change.operations as { op: string }[])[0]?.op)}` : String(change.action);
      onRun(step, change);
      ran.push(step);
      return { statements: 1, transactional: false, outputs: [], durationMs: 1 };
    }
    if (command === 'ddl_apply') {
      onRun(args.ddl as string, null);
      ran.push(args.ddl as string);
      return { output: { results: [], notices: [], elapsedMs: 1 }, session: null, collationDatabase: null, warnings: [], durationMs: 1 };
    }
    throw new Error(`unexpected command ${command}`);
  }),
}));
vi.mock('../notices/useNotices', () => ({ useNotices: () => ({ refreshRestores: vi.fn(async () => undefined) }) }));
let activity: unknown[] = [];
vi.mock('../../api/activity', () => ({ fetchActivity: vi.fn(async () => ({ events: activity })) }));
vi.mock('../../composables/useTabs', () => ({
  useTabs: () => ({
    openTableDesigner: (...args: unknown[]) => {
      opened.push(args);
      return { id: 'tab-mv' };
    },
  }),
}));
vi.mock('../../composables/useWorkspace', async () => {
  const { ref: vueRef } = await import('vue');
  const records = vueRef({ connections: [], folders: [], queries: [] });
  return { useWorkspace: () => ({ records, connection: () => profile, refresh: vi.fn(async () => undefined) }) };
});

function session(id = 's1'): SessionInfo {
  return {
    id,
    connectionId: 'c1',
    engine: 'postgres',
    server: { engine: 'postgres', version: '18.0', major: 18, minor: 0, patch: 0 },
    environment: 'development',
    database: 'shop',
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: true, mode: 'prefer', passwordUnencrypted: false },
  };
}

function matView(indexes: PgMaterializedView['indexes']): PgMaterializedView {
  return {
    schema: 'rep',
    name: 'totals',
    owner: 'app',
    definition: 'SELECT day FROM rep.days',
    populated: true,
    accessMethod: 'heap',
    tablespace: null,
    storageParameters: [],
    rowEstimate: null,
    totalBytes: null,
    sizeNote: null,
    lastRefreshNote: '',
    refreshConcurrently: indexes.length > 0,
    comment: null,
    system: false,
    columns: [],
    indexes,
  } as unknown as PgMaterializedView;
}

const uniqueIndex = (name: string): PgMaterializedView['indexes'][number] =>
  ({
    name,
    unique: true,
    valid: true,
    predicate: null,
    keys: [{ expression: false }],
    sizeBytes: null,
    definition: `CREATE UNIQUE INDEX ${name} ON rep.totals USING btree (day)`,
  }) as unknown as PgMaterializedView['indexes'][number];

const object: ObjectRef = { kind: 'materializedView', database: 'shop', schema: 'rep', parent: null, name: 'totals', signature: null };

async function mountEditor() {
  const wrapper = mount(MatViewEditor, {
    props: { session: session(), connectionName: 'Shop', object },
    global: { stubs: { SqlCode: true, DdlView: true } },
  });
  await flushPromises();
  return wrapper;
}

/** Waits for the debounced plan (400 ms) and the promises after it. */
async function settle(): Promise<void> {
  await flushPromises();
  await new Promise((resolve) => setTimeout(resolve, 450));
  await flushPromises();
}

function button(wrapper: VueWrapper, text: string) {
  const found = wrapper.findAll('button').find((b) => b.text() === text);
  if (!found) throw new Error(`no button ${text}`);
  return found;
}

/** The definition's SQL editor (stubbed): the first one in the editor. */
const queryEditor = (wrapper: VueWrapper) => wrapper.findAllComponents({ name: 'SqlCode' })[0] as VueWrapper;

async function typeQuery(wrapper: VueWrapper, query: string): Promise<void> {
  queryEditor(wrapper).vm.$emit('update:modelValue', query);
  await settle();
}

const driverError = (message: string) => ({ kind: 'session', error: { kind: 'driver', error: { kind: 'query', message, code: null, detail: null, hint: null, position: null } } });

beforeEach(() => {
  opened.length = 0;
  activity = [];
  views = {};
  impact = emptyImpact();
  planned.length = 0;
  ran.length = 0;
  onRun = () => undefined;
});

describe('MatViewEditor indexes', () => {
  it('offers Design indexes next to the concurrent refresh hint when no unique index exists, and opens the designer on the indexes', async () => {
    views = { 'rep.totals': matView([]) };
    const wrapper = await mountEditor();
    expect(wrapper.text()).toContain('Concurrent refresh needs a unique index');
    const button = wrapper.findAll('button').find((b) => b.text() === 'Design indexes');
    expect(button).toBeDefined();
    await button?.trigger('click');
    expect(opened).toEqual([[profile, object, { database: 'shop', schema: 'rep', parent: null }]]);
    expect(pendingFocus('tab-mv')).toMatchObject({ section: 'indexes', name: null });
    wrapper.unmount();
  });

  it('shows no hint button once a unique index on plain columns exists', async () => {
    views = { 'rep.totals': matView([uniqueIndex('totals_day')]) };
    const wrapper = await mountEditor();
    expect(wrapper.findAll('button').some((b) => b.text() === 'Design indexes')).toBe(false);
    wrapper.unmount();
  });
});

describe('MatViewEditor refresh history', () => {
  /** An event as the backend records it: the view in `target`; the statement only when the log keeps statement text. */
  const event = (id: string, target: string | null, statement: string | null) => ({
    id,
    occurredAt: 0,
    action: 'matview.refreshed',
    outcome: 'success',
    durationMs: 5,
    target,
    details: statement === null ? { concurrently: false } : { statement, concurrently: false },
  });

  it('lists the refreshes of exactly this view by the event target, not of views whose names contain its name', async () => {
    views = { 'rep.totals': matView([]) };
    activity = [
      event('a', 'rep.totals', 'REFRESH MATERIALIZED VIEW "rep"."totals"'),
      event('b', 'rep.totals_daily', 'REFRESH MATERIALIZED VIEW "rep"."totals_daily"'),
      event('c', 'eu.totals', 'REFRESH MATERIALIZED VIEW "eu"."totals"'),
      // The log keeps no statement text: the target alone names the view.
      event('d', 'rep.totals', null),
      event('e', 'eu.totals', null),
    ];
    const wrapper = await mountEditor();
    const section = wrapper.find('[aria-label="Refresh history"]');
    expect(section.findAll('tbody tr')).toHaveLength(2);
    wrapper.unmount();
  });

  it('falls back to the statement for events recorded without a target', async () => {
    views = { 'rep.totals': matView([]) };
    activity = [
      event('a', null, 'REFRESH MATERIALIZED VIEW CONCURRENTLY rep.totals WITH NO DATA'),
      event('b', null, 'REFRESH MATERIALIZED VIEW "rep"."totals_daily"'),
      event('c', null, null),
    ];
    const wrapper = await mountEditor();
    expect(wrapper.find('[aria-label="Refresh history"]').findAll('tbody tr')).toHaveLength(1);
    wrapper.unmount();
  });
});

describe('MatViewEditor recreate', () => {
  it('does not plan a recreate while objects depend on the view, and says which', async () => {
    views = { 'rep.totals': matView([]) };
    impact = { ...emptyImpact(), dependents: [{ description: 'view rep.report', effect: 'blocks' }] };
    const wrapper = await mountEditor();
    await typeQuery(wrapper, 'SELECT 2 AS day');
    expect(wrapper.text()).toContain('1 object depends on this view (view rep.report), so it cannot be dropped and recreated. Drop it first.');
    expect(planned).toEqual([]);
    wrapper.unmount();
  });

  it('refuses a recreate while the temporary name is taken, naming it', async () => {
    views = { 'rep.totals': matView([]), 'rep.totals_varve_new': matView([]) };
    const wrapper = await mountEditor();
    await typeQuery(wrapper, 'SELECT 2 AS day');
    expect(wrapper.text()).toContain('rep.totals_varve_new already exists');
    expect(wrapper.text()).toContain('drop rep.totals_varve_new, then save again.');
    expect(planned).toEqual([]);
    wrapper.unmount();
  });

  it('plans the recreate once the check found nothing in its way, with everything that can fail before the drop', async () => {
    views = { 'rep.totals': matView([uniqueIndex('totals_day')]) };
    const wrapper = await mountEditor();
    await typeQuery(wrapper, 'SELECT 2 AS day');
    expect(planned.map((p) => p.change.action)).toEqual(['create', 'alter', 'drop', 'alter']);
    await button(wrapper, 'Save').trigger('click');
    await settle();
    expect(ran).toEqual([
      'create',
      'alter owner',
      'CREATE UNIQUE INDEX "totals_day_varve_new" ON "rep"."totals_varve_new" USING btree (day);',
      'drop',
      'alter rename',
      'ALTER INDEX "rep"."totals_day_varve_new" RENAME TO "totals_day";',
    ]);
    wrapper.unmount();
  });

  /** The steps as a server would run them, on `views`. */
  function server(step: string): void {
    const temporary = views['rep.totals_varve_new'];
    if (step === 'create') views['rep.totals_varve_new'] = { ...matView([]), name: 'totals_varve_new', definition: 'SELECT 2 AS day' };
    else if (step.startsWith('CREATE UNIQUE INDEX') && temporary) views['rep.totals_varve_new'] = { ...temporary, indexes: [uniqueIndex('totals_day_varve_new')] };
    else if (step === 'drop') Reflect.deleteProperty(views, 'rep.totals');
    else if (step === 'alter rename' && temporary) {
      Reflect.deleteProperty(views, 'rep.totals_varve_new');
      views['rep.totals'] = { ...temporary, name: 'totals' };
    } else if (step.startsWith('ALTER INDEX') && views['rep.totals']) views['rep.totals'] = { ...views['rep.totals'], indexes: [uniqueIndex('totals_day')] };
  }

  it('keeps the remaining steps when the drop fails, says what exists, and finishes on a new session', async () => {
    views = { 'rep.totals': matView([uniqueIndex('totals_day')]) };
    let dropFails = true;
    onRun = (step) => {
      if (step === 'drop' && dropFails) throw driverError('cannot drop materialized view rep.totals because other objects depend on it');
      server(step);
    };
    const wrapper = await mountEditor();
    await typeQuery(wrapper, 'SELECT 2 AS day');
    await button(wrapper, 'Save').trigger('click');
    await settle();
    expect(ran).toHaveLength(3);
    // Not a create form, and not a reload over the form: the editor says what exists and keeps the steps that did not run.
    expect(wrapper.text()).toContain('Nothing was dropped: rep.totals still exists, and the new definition exists as rep.totals_varve_new.');
    expect(wrapper.text()).not.toContain('New materialized view');
    expect(wrapper.text()).not.toContain('Saving creates it from the form');
    expect((queryEditor(wrapper).props() as Record<string, unknown>).modelValue).toBe('SELECT 2 AS day');
    expect((queryEditor(wrapper).props() as Record<string, unknown>).readonly).toBe(true);

    // A reconnect: the remaining steps, index rename included, are planned on the new session; nothing is planned from the form.
    planned.length = 0;
    await wrapper.setProps({ session: session('s2') });
    await settle();
    expect(planned.map((p) => [p.sessionId, p.change.action])).toEqual([
      ['s2', 'drop'],
      ['s2', 'alter'],
    ]);
    expect(wrapper.text()).toContain('Nothing was dropped: rep.totals still exists');

    dropFails = false;
    await button(wrapper, 'Save').trigger('click');
    await settle();
    expect(ran.slice(3)).toEqual(['drop', 'alter rename', 'ALTER INDEX "rep"."totals_day_varve_new" RENAME TO "totals_day";']);
    expect(wrapper.text()).not.toContain('stopped part way');
    expect(wrapper.text()).not.toContain('Unsaved changes');
    expect((queryEditor(wrapper).props() as Record<string, unknown>).readonly).toBe(false);
    wrapper.unmount();
  });

  it('says that the old view is gone and where the new one is when a step after the drop fails, and offers the remaining steps', async () => {
    views = { 'rep.totals': matView([]) };
    onRun = (step) => {
      if (step === 'alter rename') throw driverError('relation "totals" already exists');
      server(step);
    };
    const wrapper = await mountEditor();
    await typeQuery(wrapper, 'SELECT 2 AS day');
    await button(wrapper, 'Save').trigger('click');
    await settle();
    expect(ran).toEqual(['create', 'alter owner', 'drop']);
    expect(wrapper.text()).toContain('rep.totals was dropped, and the new view exists as rep.totals_varve_new until it is renamed.');
    expect(wrapper.text()).not.toContain('New materialized view');
    // The old view cannot be gone back to: Revert waits, the remaining step is offered.
    expect(button(wrapper, 'Revert').attributes('disabled')).toBeDefined();
    planned.length = 0;
    await button(wrapper, 'Plan the remaining steps').trigger('click');
    await settle();
    expect(planned.map((p) => p.change)).toEqual([
      { object: 'materializedView', action: 'alter', name: { schema: 'rep', name: 'totals_varve_new' }, operations: [{ op: 'rename', newName: 'totals' }] },
    ]);
    wrapper.unmount();
  });

  it('gives up the remaining steps on Revert while the old view still exists, and then names the leftover', async () => {
    views = { 'rep.totals': matView([]) };
    onRun = (step) => {
      if (step === 'alter owner') throw driverError('must be able to SET ROLE "app"');
      server(step);
    };
    const wrapper = await mountEditor();
    await typeQuery(wrapper, 'SELECT 2 AS day');
    await button(wrapper, 'Save').trigger('click');
    await settle();
    expect(wrapper.text()).toContain('Nothing was dropped');
    await button(wrapper, 'Revert').trigger('click');
    await settle();
    expect(wrapper.text()).not.toContain('stopped part way');
    expect((queryEditor(wrapper).props() as Record<string, unknown>).modelValue).toBe('SELECT day FROM rep.days');
    // A new attempt is refused until the leftover is gone.
    await typeQuery(wrapper, 'SELECT 3 AS day');
    expect(wrapper.text()).toContain('rep.totals_varve_new already exists');
    wrapper.unmount();
  });
});
