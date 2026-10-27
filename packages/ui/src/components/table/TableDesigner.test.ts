/**
 * The table designer's sections against a mocked backend: partitioned
 * tables (B1), the Properties section (B2), the engine picker (B3),
 * materialized view indexes and the focus that tree nodes ask for.
 */
import { flushPromises, mount } from '@vue/test-utils';
import type { VueWrapper } from '@vue/test-utils';
import type { ObjectKind } from '../../api/catalog';
import type { EngineChange, MyTableInfo, PgMatViewInfo, PgTableInfo } from '../../api/grid';
import type { SessionInfo } from '../../api/sessions';
import type { DesignerFocus } from './designerIntent';
import TableDesigner from './TableDesigner.vue';

const planned: EngineChange[] = [];
const executed: string[] = [];
const counted: string[] = [];
/** What `catalog_row_count` answers; a promise that never settles stands for a count still running. */
let countResult: Promise<number | null> | number | null = 2;
const described: string[] = [];
let objects: Record<string, unknown> = {};
let enginesFail = false;

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async (command: string, args: Record<string, unknown>) => {
    switch (command) {
      case 'catalog_describe': {
        const object = args.object as { kind: string; name: string };
        described.push(`${object.kind}:${object.name}`);
        const found = objects[`${object.kind}:${object.name}`];
        if (!found) throw { kind: 'catalog', error: { kind: 'notFound', message: `${object.kind} ${object.name} not found` } };
        return found;
      }
      case 'catalog_engines':
        if (enginesFail) throw { kind: 'catalog', error: { kind: 'driver', error: { kind: 'query', message: 'denied', code: null, detail: null, hint: null, position: null } } };
        return [
          { name: 'Aria', default: false, transactions: false, comment: 'Crash-safe tables' },
          { name: 'InnoDB', default: true, transactions: true, comment: 'Supports transactions' },
          { name: 'MyISAM', default: false, transactions: false, comment: 'Non-transactional', warning: null },
          { name: 'BLACKHOLE', default: false, transactions: false, comment: '/dev/null storage engine', warning: 'BLACKHOLE stores nothing: every row of the table is discarded' },
        ];
      case 'catalog_row_count':
        counted.push(`${args.database as string}.${args.table as string}`);
        return countResult;
      case 'execute':
        executed.push(args.sql as string);
        return { results: [{ kind: 'rows', columns: [{ name: 'n', typeName: 'bigint' }], rows: [[{ t: 'int', v: 2 }]], truncated: false }], notices: [], elapsedMs: 1 };
      case 'edit_plan':
        planned.push(args.change as EngineChange);
        return { planId: `p${planned.length}`, plan: { statements: ['-- planned'], transactional: true, destructive: false, warnings: [] } };
      default:
        throw new Error(`unexpected command ${command}`);
    }
  }),
}));

function session(engine: 'postgres' | 'mysql' | 'mariadb', major = 18): SessionInfo {
  return {
    id: 's1',
    connectionId: 'c1',
    engine,
    server: { engine, version: `${major}.0`, major, minor: 0, patch: 0 },
    environment: 'development',
    database: engine === 'postgres' ? 'app' : 'shop',
    readOnly: false,
    confirmDestructive: false,
    purpose: 'user',
    openedAt: 0,
    encryption: { tls: false, mode: 'disable', passwordUnencrypted: false },
  };
}

function pgTable(extra: Partial<PgTableInfo> = {}): { engine: 'postgres'; object: PgTableInfo } {
  return {
    engine: 'postgres',
    object: {
      kind: 'table',
      schema: 'public',
      name: 'orders',
      owner: 'app',
      unlogged: false,
      tablespace: null,
      storageParameters: [],
      inherits: [],
      partitioning: null,
      partitionOf: null,
      rowEstimate: 0,
      totalBytes: 0,
      comment: null,
      system: false,
      columns: [
        { name: 'id', position: 1, dataType: 'bigint', notNull: true, default: null, identity: null, generated: null, collation: null, local: true, inheritedCount: 0, comment: null },
      ],
      constraints: [
        {
          name: 'orders_pkey',
          constraintType: 'primaryKey',
          columns: ['id'],
          definition: 'PRIMARY KEY (id)',
          deferrable: false,
          deferred: false,
          validated: true,
          local: true,
          inheritedCount: 0,
          hasParentConstraint: false,
          index: 'orders_pkey',
          foreignKey: null,
          comment: null,
        },
      ],
      indexes: [],
      ...extra,
    },
  };
}

function myTable(): { engine: 'mysql'; object: MyTableInfo } {
  return {
    engine: 'mysql',
    object: {
      kind: 'table',
      database: 'shop',
      name: 'orders',
      tableType: 'BASE TABLE',
      engine: 'InnoDB',
      rowFormat: 'Dynamic',
      autoIncrement: 1,
      collation: 'utf8mb4_0900_ai_ci',
      charset: 'utf8mb4',
      rowEstimate: 0,
      dataLength: 0,
      indexLength: 0,
      comment: null,
      systemVersioning: null,
      columns: [
        {
          table: 'orders',
          name: 'id',
          position: 1,
          dataType: 'int',
          columnType: 'int',
          nullable: false,
          default: null,
          autoIncrement: false,
          onUpdate: null,
          generated: null,
          invisible: false,
          charset: null,
          collation: null,
          comment: null,
          srid: null,
          json: false,
          check: null,
          key: 'PRI',
        },
      ],
      indexes: [],
      constraints: [],
      partitioning: null,
      system: false,
    },
  };
}

function matView(): { engine: 'postgres'; object: PgMatViewInfo } {
  return {
    engine: 'postgres',
    object: {
      kind: 'materializedView',
      schema: 'public',
      name: 'totals',
      owner: 'app',
      populated: true,
      tablespace: null,
      comment: null,
      system: false,
      columns: [{ name: 'day', position: 1, dataType: 'date', notNull: false, default: null, identity: null, generated: null, collation: null, local: true, inheritedCount: 0, comment: null }],
      indexes: [],
    },
  };
}

interface MountOptions {
  engine?: 'postgres' | 'mysql' | 'mariadb';
  major?: number;
  name?: string | null;
  objectKind?: ObjectKind;
  startPartitioned?: boolean;
  focus?: DesignerFocus | null;
  /** Minor and patch version of the server (default 0.0). */
  version?: { minor: number; patch: number };
}

async function mountDesigner(options: MountOptions = {}): Promise<VueWrapper> {
  const engine = options.engine ?? 'postgres';
  const name = options.name === undefined ? 'orders' : options.name;
  const base = session(engine, options.major ?? 18);
  if (options.version) base.server = { ...base.server, ...options.version, version: `${base.server.major}.${options.version.minor}.${options.version.patch}` };
  const wrapper = mount(TableDesigner, {
    props: {
      session: base,
      connectionName: 'Local',
      target: name === null ? null : { database: engine === 'postgres' ? 'app' : 'shop', schema: engine === 'postgres' ? 'public' : null, table: name },
      objectKind: options.objectKind ?? 'table',
      newIn: { database: engine === 'postgres' ? 'app' : 'shop', schema: engine === 'postgres' ? 'public' : null },
      startPartitioned: options.startPartitioned ?? false,
      focus: options.focus ?? null,
    },
    attachTo: document.body,
  });
  await flushPromises();
  return wrapper;
}

function sectionLabels(wrapper: VueWrapper): string[] {
  return wrapper.findAll('[role="tab"]').map((t) => t.text().replace(/\s*\d+$/, '').trim());
}

async function openSection(wrapper: VueWrapper, label: string): Promise<void> {
  const tab = wrapper.findAll('[role="tab"]').find((t) => t.text().startsWith(label));
  if (!tab) throw new Error(`no section ${label}`);
  await tab.trigger('click');
}

async function save(wrapper: VueWrapper): Promise<void> {
  const button = wrapper.findAll('footer button').find((b) => /^(Save|Create)/.test(b.text()));
  if (!button) throw new Error('no save button');
  await button.trigger('click');
  await flushPromises();
}

beforeAll(() => {
  // jsdom has no layout: the designer scrolls the focused row into view.
  Element.prototype.scrollIntoView = vi.fn();
});

beforeEach(() => {
  planned.length = 0;
  executed.length = 0;
  counted.length = 0;
  countResult = 2;
  described.length = 0;
  objects = {};
  enginesFail = false;
});

afterEach(() => {
  document.body.innerHTML = '';
});

describe('new partitioned table (PostgreSQL)', () => {
  it('starts partitioned, chooses strategy and key in Partitioning, and sends PARTITION BY with a default partition', async () => {
    const wrapper = await mountDesigner({ name: null, startPartitioned: true });
    expect(sectionLabels(wrapper)).toEqual(['Columns', 'Indexes', 'Constraints', 'Partitioning', 'Properties']);
    expect(wrapper.find('footer').text()).toContain('Create partitioned table');
    await openSection(wrapper, 'Partitioning');
    const checkbox = wrapper.find<HTMLInputElement>('input[type="checkbox"]');
    expect(checkbox.element.checked).toBe(true);
    await wrapper.find('input[type="radio"][value="list"]').setValue(true);
    expect(wrapper.text()).toContain('List partitioning takes exactly one key.');
    const def = wrapper.findAll<HTMLInputElement>('input[type="checkbox"]').at(1);
    await def?.setValue(true);
    await save(wrapper);
    expect(planned).toHaveLength(2);
    expect(planned[0]).toMatchObject({ engine: 'postgres', change: { object: 'table', action: 'create', partitionBy: { strategy: 'list', keys: [{ key: { column: 'id' } }] } } });
    expect(planned[1]).toMatchObject({ change: { object: 'table', action: 'create', name: { name: 'new_table_default' }, partitionOf: { parent: { name: 'new_table' }, bound: { kind: 'default' } } } });
    wrapper.unmount();
  });

  it('warns when the primary key misses a partition key column, and a plain new table has no Partitioning on MySQL', async () => {
    const wrapper = await mountDesigner({ name: null, startPartitioned: true });
    await openSection(wrapper, 'Partitioning');
    const add = wrapper.findAll('button').find((b) => b.text() === 'Add expression');
    await add?.trigger('click');
    await wrapper.find('input[aria-label="Key 2 expression"]').setValue("date_trunc('day', now())");
    await save(wrapper);
    expect(planned).toEqual([]);
    expect(wrapper.find('[aria-label="Fix before saving"]').text()).toMatch(/expression key cannot have primary key/);
    wrapper.unmount();
    const my = await mountDesigner({ engine: 'mysql', name: null });
    expect(sectionLabels(my)).toEqual(['Columns', 'Indexes', 'Constraints', 'Properties']);
    my.unmount();
  });
});

describe('Properties (PostgreSQL)', () => {
  it('says SET LOGGED rewrites the table and saves the property changes', async () => {
    objects = { 'table:orders': pgTable({ unlogged: true, storageParameters: [{ name: 'fillfactor', value: '90' }] }) };
    const wrapper = await mountDesigner();
    await openSection(wrapper, 'Properties');
    const unlogged = wrapper.findAll<HTMLInputElement>('input[type="checkbox"]').at(0);
    expect(unlogged?.element.checked).toBe(true);
    await unlogged?.setValue(false);
    expect(wrapper.text()).toContain('SET LOGGED rewrites the whole table');
    const owner = wrapper.findAll('label').find((l) => l.text() === 'Owner');
    await wrapper.find(`#${owner?.attributes('for') ?? ''}`).setValue('reporting');
    expect(wrapper.find('[role="tab"][aria-selected="true"]').text()).toMatch(/Properties\s*2/);
    await save(wrapper);
    expect(planned).toEqual([
      {
        engine: 'postgres',
        change: {
          object: 'table',
          action: 'alter',
          name: { schema: 'public', name: 'orders' },
          operations: [
            { op: 'owner', owner: { kind: 'name', name: 'reporting' } },
            { op: 'setLogged', logged: true },
          ],
        },
      },
    ]);
    wrapper.unmount();
  });

  it('turns off storage parameters and UNLOGGED for a partitioned table', async () => {
    objects = { 'partitionedTable:orders': pgTable({ kind: 'partitionedTable', partitioning: { strategy: 'range', key: 'id', partitions: [], defaultPartition: null } }) };
    const wrapper = await mountDesigner({ objectKind: 'partitionedTable' });
    await openSection(wrapper, 'Properties');
    expect(wrapper.find<HTMLInputElement>('input[type="checkbox"]').element.disabled).toBe(true);
    expect(wrapper.text()).toContain('A partitioned table takes no storage parameters');
    expect(wrapper.text()).not.toContain('Inherits from');
    wrapper.unmount();
  });
});

describe('Properties (MySQL / MariaDB)', () => {
  it('fills the engine picker from the server and saves a new engine with its warning', async () => {
    objects = { 'table:orders': myTable() };
    const wrapper = await mountDesigner({ engine: 'mariadb' });
    await openSection(wrapper, 'Properties');
    await flushPromises();
    const select = wrapper.find<HTMLSelectElement>('select');
    expect([...select.element.options].map((o) => o.text.trim())).toEqual(['Aria', 'InnoDB (default)', 'MyISAM', 'BLACKHOLE']);
    expect(select.element.value).toBe('InnoDB');
    await select.setValue('Aria');
    expect(wrapper.text()).toContain('Changing the engine copies the whole table');
    expect(wrapper.text()).toContain('Crash-safe tables');
    // PAGE is offered once the engine is Aria.
    expect(wrapper.findAll('option').some((o) => o.text() === 'PAGE (Aria)')).toBe(true);
    await save(wrapper);
    expect(planned).toEqual([
      { engine: 'mysql', change: { change: 'alterTable', database: 'shop', name: 'orders', operations: [{ op: 'setOptions', options: { engine: 'Aria' } }], systemVersioned: false } },
    ]);
    wrapper.unmount();
  });

  it('falls back to typing the engine when the list cannot be read', async () => {
    enginesFail = true;
    objects = { 'table:orders': myTable() };
    const wrapper = await mountDesigner({ engine: 'mysql' });
    await openSection(wrapper, 'Properties');
    await flushPromises();
    expect(wrapper.text()).toContain("The server's engines could not be read");
    expect(wrapper.findAll('input').some((i) => (i.element as HTMLInputElement).value === 'InnoDB')).toBe(true);
    wrapper.unmount();
  });
});

describe('focus from the object tree', () => {
  it('opens the section of the item and selects it', async () => {
    objects = { 'table:orders': pgTable() };
    const wrapper = await mountDesigner({ focus: { section: 'constraints', name: 'orders_pkey', seq: 1 } });
    expect(wrapper.find('[role="tab"][aria-selected="true"]').text()).toMatch(/^Constraints/);
    expect(wrapper.find('tr[aria-selected="true"]').text()).toContain('orders_pkey');
    expect(wrapper.emitted('focused')).toEqual([[{ section: 'constraints', name: 'orders_pkey', seq: 1 }]]);
    await wrapper.setProps({ focus: { section: 'partitions', name: 'p1', seq: 2 } });
    expect(wrapper.find('[role="tab"][aria-selected="true"]').text()).toMatch(/^Partitions/);
    wrapper.unmount();
  });
});

describe('materialized view indexes', () => {
  it('reads an index node\'s relation as a materialized view when it is not a table, and offers only Indexes', async () => {
    objects = { 'materializedView:totals': matView() };
    const wrapper = await mountDesigner({ name: 'totals', focus: { section: 'indexes', name: null, seq: 3 } });
    expect(described).toEqual(['table:totals', 'materializedView:totals']);
    expect(sectionLabels(wrapper)).toEqual(['Indexes']);
    const add = wrapper.findAll('button').find((b) => /Add index/.test(b.text()));
    await add?.trigger('click');
    await wrapper.findAll('button').find((b) => b.text() === '+ Column')?.trigger('click');
    await save(wrapper);
    expect(wrapper.find('[aria-label="Fix before saving"]').exists()).toBe(false);
    expect(planned[0]).toMatchObject({ engine: 'postgres', change: { object: 'index', action: 'create', table: { schema: 'public', name: 'totals' } } });
    wrapper.unmount();
  });
});

describe('audit fixes', () => {
  function partitioned(): ReturnType<typeof pgTable> {
    return pgTable({
      kind: 'partitionedTable',
      partitioning: {
        strategy: 'range',
        key: 'RANGE (id)',
        partitions: [{ schema: 'public', name: 'orders_1', bound: 'FOR VALUES FROM (1) TO (10)', isDefault: false, partitioned: false, foreign: false, detachPending: false }],
        defaultPartition: null,
      },
    });
  }

  it('TD-M2: DETACH CONCURRENTLY is off by default, and unavailable with the reason on PostgreSQL 13', async () => {
    objects = { 'partitionedTable:orders': partitioned() };
    const pg13 = await mountDesigner({ objectKind: 'partitionedTable', major: 13, focus: { section: 'partitions', name: 'orders_1', seq: 1 } });
    const box13 = pg13.findAll<HTMLInputElement>('input[type="checkbox"]').find((b) => b.element.closest('label')?.textContent?.includes('CONCURRENTLY'));
    expect(box13?.element.checked).toBe(false);
    expect(box13?.element.disabled).toBe(true);
    expect(pg13.text()).toContain('not available on PostgreSQL 13 (14 and later)');
    await pg13.findAll('button').find((b) => b.text() === 'Detach partition')?.trigger('click');
    await flushPromises();
    expect(planned.at(-1)).toMatchObject({ change: { operations: [{ op: 'detachPartition', mode: 'normal' }] } });
    pg13.unmount();
    const pg18 = await mountDesigner({ objectKind: 'partitionedTable', major: 18, focus: { section: 'partitions', name: 'orders_1', seq: 2 } });
    const box18 = pg18.findAll<HTMLInputElement>('input[type="checkbox"]').find((b) => b.element.closest('label')?.textContent?.includes('CONCURRENTLY'));
    expect(box18?.element.checked).toBe(false);
    expect(box18?.element.disabled).toBe(false);
    pg18.unmount();
  });

  it('TD-L1: a hash modulus that does not fit the existing ones is refused before planning', async () => {
    objects = {
      'partitionedTable:orders': pgTable({
        kind: 'partitionedTable',
        partitioning: {
          strategy: 'hash',
          key: 'HASH (id)',
          partitions: [{ schema: 'public', name: 'h0', bound: 'FOR VALUES WITH (modulus 4, remainder 0)', isDefault: false, partitioned: false, foreign: false, detachPending: false }],
          defaultPartition: null,
        },
      }),
    };
    const wrapper = await mountDesigner({ objectKind: 'partitionedTable', focus: { section: 'partitions', name: null, seq: 3 } });
    const inputs = wrapper.findAll<HTMLInputElement>('input[type="number"]');
    await inputs.at(-2)?.setValue(6);
    await inputs.at(-1)?.setValue(1);
    expect(wrapper.text()).toContain('4 does not divide 6');
    wrapper.unmount();
  });

  it('TD-H1 / TD2-M1: BLACKHOLE says the rows are lost; the review opens on the estimate and takes the exact count when it arrives', async () => {
    const details = myTable();
    details.object.rowEstimate = 1000;
    objects = { 'table:orders': details };
    let finish: (n: number | null) => void = () => undefined;
    countResult = new Promise<number | null>((resolve) => {
      finish = resolve;
    });
    const wrapper = await mountDesigner({ engine: 'mysql' });
    await openSection(wrapper, 'Properties');
    await flushPromises();
    await wrapper.find<HTMLSelectElement>('select').setValue('BLACKHOLE');
    expect(wrapper.find('[role="alert"]').text()).toContain('every row of this table is lost');
    await save(wrapper);
    // Nothing counted on the tab's own session; the review is open while the count still runs.
    expect(executed).toEqual([]);
    expect(counted).toEqual(['shop.orders']);
    expect(document.body.textContent).toContain("orders holds about 1000 rows (the server's estimate): BLACKHOLE keeps none of them. The rows will be lost.");
    finish(1042);
    await flushPromises();
    expect(document.body.textContent).toContain('orders holds 1042 rows: BLACKHOLE keeps none of them. The rows will be lost.');
    wrapper.unmount();
  });

  it('TD2-M1: keeps the estimate, marked, when the count runs out of time', async () => {
    const details = myTable();
    details.object.rowEstimate = 5000000;
    objects = { 'table:orders': details };
    countResult = null;
    const wrapper = await mountDesigner({ engine: 'mysql' });
    await openSection(wrapper, 'Properties');
    await flushPromises();
    await wrapper.find<HTMLSelectElement>('select').setValue('BLACKHOLE');
    await save(wrapper);
    expect(document.body.textContent).toContain("about 5000000 rows (the server's estimate; an exact count took too long)");
    wrapper.unmount();
  });

  it('TD-M1 and TD-L3: a new character set drops the old collation; a partition can change schema', async () => {
    objects = { 'table:orders': myTable() };
    const my = await mountDesigner({ engine: 'mysql' });
    await openSection(my, 'Properties');
    const label = (text: string) => my.findAll('label').find((l) => l.text() === text)?.attributes('for') ?? '';
    await my.find(`#${label('Character set')}`).setValue('latin1');
    expect((my.find(`#${label('Collation')}`).element as HTMLInputElement).value).toBe('');
    my.unmount();
    objects = { 'partition:orders_1': pgTable({ kind: 'partition', name: 'orders_1', partitionOf: { parent: { schema: 'public', name: 'orders' }, bound: 'FOR VALUES FROM (1) TO (10)', isDefault: false, detachPending: false } }) };
    const part = await mountDesigner({ name: 'orders_1', objectKind: 'partition' });
    await openSection(part, 'Properties');
    const schemaFor = part.findAll('label').find((l) => l.text() === 'Schema')?.attributes('for') ?? '';
    expect((part.find(`#${schemaFor}`).element as HTMLInputElement).disabled).toBe(false);
    part.unmount();
  });
});

describe('MySQL / MariaDB INVISIBLE columns and ON UPDATE', () => {
  const headers = (wrapper: VueWrapper): string[] => wrapper.findAll('table.cols th').map((th) => th.text().trim());

  it('shows the Invisible control from MySQL 8.0.23 and MariaDB 10.3.3, never on PostgreSQL', async () => {
    objects = { 'table:orders': myTable() };
    for (const [engine, major, minor, patch, shown] of [
      ['mysql', 8, 0, 22, false],
      ['mysql', 8, 0, 23, true],
      ['mysql', 9, 7, 0, true],
      ['mariadb', 10, 3, 2, false],
      ['mariadb', 10, 3, 3, true],
      ['mariadb', 12, 3, 0, true],
    ] as const) {
      const wrapper = await mountDesigner({ engine, major, version: { minor, patch } });
      expect(headers(wrapper).includes('Invisible'), `${engine} ${major}.${minor}.${patch}`).toBe(shown);
      expect(headers(wrapper)).toContain('On update');
      wrapper.unmount();
    }
    objects = { 'table:orders': pgTable() };
    const pg = await mountDesigner();
    expect(headers(pg)).not.toContain('Invisible');
    expect(headers(pg)).not.toContain('On update');
    pg.unmount();
  });

  it('sets INVISIBLE and ON UPDATE CURRENT_TIMESTAMP on a column and clears them again', async () => {
    const details = myTable();
    const [id] = details.object.columns;
    if (!id) throw new Error('fixture');
    details.object.columns.push({
      ...id,
      name: 'changed',
      position: 2,
      dataType: 'datetime',
      columnType: 'datetime(3)',
      nullable: true,
      key: null,
      default: { type: 'expression', value: 'CURRENT_TIMESTAMP(3)' },
      onUpdate: 'CURRENT_TIMESTAMP(3)',
    });
    objects = { 'table:orders': details };
    const wrapper = await mountDesigner({ engine: 'mysql', major: 8, version: { minor: 4, patch: 0 } });
    // ON UPDATE is offered on DATETIME and TIMESTAMP columns only.
    expect(wrapper.find('[aria-label="On update of id"]').exists()).toBe(false);
    const onUpdate = wrapper.find<HTMLSelectElement>('[aria-label="On update of changed"]');
    expect(onUpdate.element.value).toBe('3');
    await onUpdate.setValue('');
    await wrapper.find('[aria-label="changed invisible"]').setValue(true);
    await save(wrapper);
    const alter = planned[0]?.change as unknown as { operations: { op: string; column: Record<string, unknown> }[] };
    expect(alter.operations).toHaveLength(1);
    expect(alter.operations[0]).toMatchObject({ op: 'modifyColumn', column: { name: 'changed', invisible: true } });
    expect(alter.operations[0]?.column).not.toHaveProperty('onUpdateCurrentTimestamp');
    wrapper.unmount();
  });

  it('refuses a table whose columns would all be invisible', async () => {
    objects = { 'table:orders': myTable() };
    const wrapper = await mountDesigner({ engine: 'mariadb', major: 11, version: { minor: 4, patch: 0 } });
    await wrapper.find('[aria-label="id invisible"]').setValue(true);
    await save(wrapper);
    expect(planned).toEqual([]);
    expect(wrapper.text()).toContain('A table needs at least one visible column.');
    wrapper.unmount();
  });
});

describe('PostgreSQL constraint comments', () => {
  it('edits a constraint comment in the Constraints section and saves COMMENT ON CONSTRAINT', async () => {
    const details = pgTable();
    const [pkey] = details.object.constraints;
    if (!pkey) throw new Error('fixture');
    pkey.comment = 'the key';
    objects = { 'table:orders': details };
    const wrapper = await mountDesigner();
    await openSection(wrapper, 'Constraints');
    await wrapper.findAll('tbody tr').find((r) => r.text().includes('orders_pkey'))?.trigger('click');
    const field = wrapper.find<HTMLInputElement>('section.editor input:not([type="checkbox"])[placeholder="none"]');
    expect(field.element.value).toBe('the key');
    await field.setValue('primary key of orders');
    await save(wrapper);
    expect(planned).toEqual([
      {
        engine: 'postgres',
        change: { object: 'comment', target: { kind: 'constraint', database: null, schema: 'public', parent: 'orders', name: 'orders_pkey', signature: null }, comment: 'primary key of orders' },
      },
    ]);
    wrapper.unmount();
  });

  it('has no comment field on MySQL constraints', async () => {
    const details = myTable();
    details.object.constraints.push({ table: 'orders', name: 'PRIMARY', constraintType: 'primaryKey', columns: ['id'], references: null, checkClause: null, enforced: null, level: null, json: false, period: false });
    objects = { 'table:orders': details };
    const wrapper = await mountDesigner({ engine: 'mysql', major: 8 });
    await openSection(wrapper, 'Constraints');
    await wrapper.findAll('tbody tr').find((r) => r.text().includes('PRIMARY'))?.trigger('click');
    expect(wrapper.find('section.editor').exists()).toBe(true);
    expect(wrapper.find('section.editor').text()).not.toContain('Comment');
    wrapper.unmount();
  });
});

describe('MySQL / MariaDB subpartitions and REORGANIZE PARTITION', () => {
  const part = (name: string, description: string, subs: string[]): NonNullable<MyTableInfo['partitioning']>['partitions'][number] => ({
    table: 'orders',
    name,
    partition: null,
    position: 1,
    method: 'RANGE',
    expression: 'id',
    description,
    values: `VALUES LESS THAN (${description})`,
    rows: 0,
    dataLength: 0,
    indexLength: 0,
    comment: null,
    tablespace: null,
    subpartitions: subs.map((sub, i) => ({ ...part(sub, '', []), partition: name, position: i + 1, method: 'HASH', description: null, values: null })),
  });

  it('partitions a table by RANGE with HASH subpartitions', async () => {
    objects = { 'table:orders': myTable() };
    const wrapper = await mountDesigner({ engine: 'mysql', major: 8 });
    await openSection(wrapper, 'Partitions');
    const field = (label: string) => {
      const l = wrapper.findAll('label').find((x) => x.text() === label);
      const id = l?.attributes('for');
      if (!id) throw new Error(`no field ${label}`);
      return wrapper.find(`#${CSS.escape(id)}`);
    };
    await field('Expression').setValue('id');
    await field('LESS THAN').setValue('MAXVALUE');
    await field('Subpartition by').setValue('hash');
    await field('Subpartition expression').setValue('id');
    await field('Subpartitions per partition').setValue(3);
    await wrapper.findAll('button').find((b) => b.text() === 'Partition table')?.trigger('click');
    await flushPromises();
    expect(planned[0]).toMatchObject({
      engine: 'mysql',
      change: {
        change: 'alterTable',
        operations: [
          {
            op: 'partitionBy',
            partitioning: {
              method: { kind: 'range', expression: 'id' },
              subpartition: { method: { kind: 'hash', expression: 'id' }, count: 3 },
              partitions: [{ name: 'p0', values: { kind: 'lessThan', bounds: [{ kind: 'maxValue' }] } }],
            },
          },
        ],
      },
    });
    wrapper.unmount();
  });

  it('shows subpartitions and splits the selected partition with REORGANIZE PARTITION', async () => {
    const details = myTable();
    details.object.partitioning = {
      method: 'RANGE',
      linear: false,
      expression: 'id',
      subpartitionMethod: 'HASH',
      subpartitionExpression: 'id',
      partitions: [part('p0', '100', ['p0sp0', 'p0sp1']), part('pmax', 'MAXVALUE', ['pmaxsp0', 'pmaxsp1'])],
    };
    objects = { 'table:orders': details };
    const wrapper = await mountDesigner({ engine: 'mariadb', major: 11, focus: { section: 'partitions', name: 'pmax', seq: 1 } });
    expect(wrapper.text()).toContain('subpartitioned by HASH (id)');
    expect(wrapper.text()).toContain('pmaxsp0, pmaxsp1');
    // The form starts on the selected partition, unchanged: split it into p1 and pmax.
    expect(wrapper.find<HTMLInputElement>('[aria-label="Name of new partition 1"]').element.value).toBe('pmax');
    expect(wrapper.find<HTMLInputElement>('[aria-label="LESS THAN of new partition 1"]').element.value).toBe('MAXVALUE');
    await wrapper.find('[aria-label="Name of new partition 1"]').setValue('p1');
    await wrapper.find('[aria-label="LESS THAN of new partition 1"]').setValue('1000');
    await wrapper.findAll('button').find((b) => b.text() === 'Add new partition')?.trigger('click');
    const reorganize = wrapper.findAll('button').find((b) => b.text() === 'Reorganize partitions');
    expect(reorganize?.attributes('disabled')).toBeDefined();
    expect(wrapper.text()).toContain('New partition 2: give it a name.');
    await wrapper.find('[aria-label="Name of new partition 2"]').setValue('pmax');
    await wrapper.find('[aria-label="LESS THAN of new partition 2"]').setValue('MAXVALUE');
    await reorganize?.trigger('click');
    await flushPromises();
    expect(planned[0]).toEqual({
      engine: 'mysql',
      change: {
        change: 'alterTable',
        database: 'shop',
        name: 'orders',
        operations: [
          {
            op: 'reorganizePartitions',
            names: ['pmax'],
            into: [
              { name: 'p1', values: { kind: 'lessThan', bounds: [{ kind: 'value', value: { t: 'int', v: 1000 } }] } },
              { name: 'pmax', values: { kind: 'lessThan', bounds: [{ kind: 'maxValue' }] } },
            ],
          },
        ],
        systemVersioned: false,
      },
    });
    wrapper.unmount();
  });
});
