/**
 * The table designer's partitioning, properties and materialized view
 * index changes, built by the real designer code from drafts edited the way
 * the sections edit them, and kept in `__roundtrip__/<engine>.json`.
 * crates/catalog/tests/ui_table_designer_round_trip.rs plans and runs
 * exactly these changes on PostgreSQL 13 and 18, MySQL 8.0 and 9.7 and
 * MariaDB 10.6 and 12.3, against fixtures named like the details below,
 * and reads the catalog back.
 *
 * The details here mirror those fixtures (`m2_td` schema / database); keep
 * the two in step.
 */
import type { EngineChange, MyTableInfo, PgMatViewInfo, PgTableInfo } from '../../api/grid';
import {
  buildDraftChanges,
  cloneDraft,
  draftFromDetails,
  myPartitionChange,
  myPartitionValues,
  myReorganize,
  mySubpartitioning,
  newColumn,
  newConstraint,
  newIndex,
  newTableDraft,
  pgAttachPartition,
  pgCreatePartition,
  pgDetachPartition,
} from '../../lib/gridDesigner';

/** One step of the round trip: a label and the changes the designer sends for it, in order. */
interface Step {
  label: string;
  changes: EngineChange[];
}

function built(original: ReturnType<typeof newTableDraft>, draft: ReturnType<typeof newTableDraft>): EngineChange[] {
  const { changes, errors } = buildDraftChanges(original, draft);
  expect(errors).toEqual([]);
  return changes;
}

function pgColumn(name: string, position: number, dataType: string, notNull: boolean): PgTableInfo['columns'][number] {
  return { name, position, dataType, notNull, default: null, identity: null, generated: null, collation: null, local: true, inheritedCount: 0, comment: null };
}

// --- PostgreSQL ---

/** `CREATE UNLOGGED TABLE m2_td.orders (id bigint NOT NULL, note text, PRIMARY KEY (id)) WITH (fillfactor = 90, autovacuum_enabled = false)`. */
function pgOrders(): { engine: 'postgres'; object: PgTableInfo } {
  return {
    engine: 'postgres',
    object: {
      kind: 'table',
      schema: 'm2_td',
      name: 'orders',
      owner: 'varve',
      unlogged: true,
      tablespace: null,
      storageParameters: [
        { name: 'fillfactor', value: '90' },
        { name: 'autovacuum_enabled', value: 'false' },
      ],
      inherits: [],
      partitioning: null,
      partitionOf: null,
      rowEstimate: null,
      totalBytes: null,
      comment: null,
      system: false,
      columns: [pgColumn('id', 1, 'bigint', true), pgColumn('note', 2, 'text', false)],
      constraints: [],
      indexes: [],
    },
  };
}

/** `CREATE MATERIALIZED VIEW m2_td.mv AS SELECT DISTINCT day FROM m2_td.days`. */
function pgMatView(): { engine: 'postgres'; object: PgMatViewInfo } {
  return {
    engine: 'postgres',
    object: {
      kind: 'materializedView',
      schema: 'm2_td',
      name: 'mv',
      owner: 'varve',
      populated: true,
      tablespace: null,
      comment: null,
      system: false,
      columns: [pgColumn('day', 1, 'date', false)],
      indexes: [],
    },
  };
}

/** `m2_td.noted` as the step "create noted with constraint comments" made it. */
function pgNoted(): { engine: 'postgres'; object: PgTableInfo } {
  const details = pgOrders();
  const constraint = (name: string, constraintType: string, columns: string[], definition: string, comment: string): PgTableInfo['constraints'][number] => ({
    name,
    constraintType,
    columns,
    definition,
    deferrable: false,
    deferred: false,
    validated: true,
    local: true,
    inheritedCount: 0,
    hasParentConstraint: false,
    index: constraintType === 'primaryKey' ? name : null,
    foreignKey: null,
    comment,
  });
  Object.assign(details.object, {
    name: 'noted',
    unlogged: false,
    storageParameters: [],
    columns: [pgColumn('id', 1, 'bigint', true)],
    constraints: [constraint('noted_pkey', 'primaryKey', ['id'], 'PRIMARY KEY (id)', 'the key'), constraint('noted_positive', 'check', ['id'], 'CHECK ((id > 0))', "it's positive")],
  });
  return details;
}

function pgSteps(): Step[] {
  const steps: Step[] = [];

  // New partitioned table by RANGE (id, at) with its default partition, owner and comment.
  const events = newTableDraft('postgres', 'm2_td', '', true);
  events.table = 'events';
  events.columns.push({ ...newColumn('postgres', 'at'), dataType: 'timestamptz', nullable: false });
  events.partitionBy.keys = [
    { text: 'id', expression: false },
    { text: 'at', expression: false },
  ];
  for (const k of events.constraints) if (k.type === 'primaryKey') k.columns = ['id', 'at'];
  events.partitionBy.defaultPartition = true;
  events.props.owner = 'm2_td_owner';
  events.props.comment = 'events by time';
  steps.push({ label: 'create range partitioned events', changes: built(events, events) });

  // Its partitions, from the Partitions section: one created, one attached, then detached again.
  steps.push({
    label: 'partitions of events',
    changes: [
      pgCreatePartition('m2_td', 'events', 'events_low', { kind: 'range', from: 'MINVALUE, MINVALUE', to: "1000, '2026-01-01'" }, null),
      pgAttachPartition('m2_td', 'events', { schema: 'm2_td', name: 'events_old' }, { kind: 'range', from: "1000, '2026-01-01'", to: "2000, '2026-01-01'" }),
    ],
  });
  steps.push({ label: 'detach events_old', changes: pgDetachPartition('m2_td', 'events', { schema: 'm2_td', name: 'events_old' }, false, false) });

  // LIST by an expression, no unique keys (an expression key cannot have them).
  const regions = newTableDraft('postgres', 'm2_td', '', true);
  regions.table = 'regions';
  regions.constraints = [];
  regions.columns.push({ ...newColumn('postgres', 'code'), dataType: 'text', nullable: false });
  regions.partitionBy.strategy = 'list';
  regions.partitionBy.keys = [{ text: 'lower(code)', expression: true }];
  regions.partitionBy.defaultPartition = true;
  steps.push({ label: 'create list partitioned regions', changes: built(regions, regions) });
  steps.push({ label: 'partition of regions', changes: [pgCreatePartition('m2_td', 'regions', 'regions_eu', { kind: 'list', values: "'nl', 'de'" }, null)] });

  // HASH with two partitions.
  const spread = newTableDraft('postgres', 'm2_td', '', true);
  spread.table = 'spread';
  spread.partitionBy.strategy = 'hash';
  steps.push({ label: 'create hash partitioned spread', changes: built(spread, spread) });
  steps.push({
    label: 'partitions of spread',
    changes: [0, 1].map((r) => pgCreatePartition('m2_td', 'spread', `spread_${r}`, { kind: 'hash', modulus: 2, remainder: r }, null)),
  });

  // A plain new table with every property set at creation.
  const plain = newTableDraft('postgres', 'm2_td', '');
  plain.table = 'plain';
  Object.assign(plain.props, {
    unlogged: true,
    inherits: ['m2_td.base'],
    storageParams: [{ name: 'fillfactor', value: '80' }],
    tablespace: 'pg_default',
    comment: 'plain table',
    owner: 'm2_td_owner',
  });
  steps.push({ label: 'create plain with properties', changes: built(plain, plain) });

  // Properties of an existing table: everything at once, rename and SET SCHEMA last.
  const original = draftFromDetails(pgOrders());
  const draft = cloneDraft(original);
  Object.assign(draft.props, {
    owner: 'm2_td_owner',
    unlogged: false,
    storageParams: [{ name: 'fillfactor', value: '70' }],
    inherits: ['m2_td.base'],
    comment: 'orders, renamed',
    name: 'orders_moved',
    schema: 'm2_td_b',
  });
  steps.push({ label: 'orders properties', changes: built(original, draft) });

  // A unique index on a materialized view (enables REFRESH CONCURRENTLY).
  const mv = draftFromDetails(pgMatView());
  const mvDraft = cloneDraft(mv);
  const index = newIndex('postgres', 'mv_day');
  index.unique = true;
  index.parts = [{ text: 'day', expression: false, descending: false, nulls: '', extra: '' }];
  mvDraft.indexes.push(index);
  steps.push({ label: 'unique index on mv', changes: built(mv, mvDraft) });

  // TD-L3: a partition moves to another schema (PostgreSQL allows SET SCHEMA on a partition).
  const low = pgOrders();
  Object.assign(low.object, {
    kind: 'partition',
    name: 'events_low',
    unlogged: false,
    storageParameters: [],
    columns: [pgColumn('id', 1, 'bigint', true), pgColumn('at', 2, 'timestamp with time zone', true)],
    partitionOf: { parent: { schema: 'm2_td', name: 'events' }, bound: '', isDefault: false, detachPending: false },
  });
  const lowDraft = draftFromDetails(low);
  const moved = cloneDraft(lowDraft);
  moved.props.schema = 'm2_td_b';
  steps.push({ label: 'events_low to m2_td_b', changes: built(lowDraft, moved) });

  // M2: constraint comments from the Constraints section, on a new table...
  const noted = newTableDraft('postgres', 'm2_td', '');
  noted.table = 'noted';
  for (const c of noted.columns) c.identity = 'none';
  for (const k of noted.constraints) if (k.type === 'primaryKey') Object.assign(k, { name: 'noted_pkey', comment: 'the key' });
  const positive = newConstraint('check', 'noted_positive');
  Object.assign(positive, { expression: 'id > 0', comment: "it's positive" });
  noted.constraints.push(positive);
  steps.push({ label: 'create noted with constraint comments', changes: built(noted, noted) });
  // ...and on an existing one: removed, changed together with a rename, and set on a new constraint.
  const notedRead = draftFromDetails(pgNoted());
  const notedEdited = cloneDraft(notedRead);
  for (const k of notedEdited.constraints) {
    if (k.name === 'noted_pkey') k.comment = '';
    if (k.name === 'noted_positive') Object.assign(k, { name: 'noted_pos', comment: 'renamed, still positive' });
  }
  const small = newConstraint('check', 'noted_small');
  Object.assign(small, { expression: 'id < 1000000', comment: 'below a million' });
  notedEdited.constraints.push(small);
  steps.push({ label: 'noted constraint comments changed', changes: built(notedRead, notedEdited) });
  return steps;
}

// --- MySQL / MariaDB ---

/** `CREATE TABLE m2_td.orders (id int NOT NULL AUTO_INCREMENT PRIMARY KEY, note varchar(20)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin`. */
function myOrders(name = 'orders', charset = 'utf8mb4', collation = 'utf8mb4_bin'): { engine: 'mysql'; object: MyTableInfo } {
  const column = (columnName: string, position: number, columnType: string, extra: Partial<MyTableInfo['columns'][number]>): MyTableInfo['columns'][number] => ({
    table: name,
    name: columnName,
    position,
    dataType: columnType.replace(/\(.*$/, ''),
    columnType,
    nullable: true,
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
    key: null,
    ...extra,
  });
  return {
    engine: 'mysql',
    object: {
      kind: 'table',
      database: 'm2_td',
      name,
      tableType: 'BASE TABLE',
      engine: 'InnoDB',
      rowFormat: 'Dynamic',
      autoIncrement: 1,
      collation,
      charset,
      rowEstimate: 0,
      dataLength: 0,
      indexLength: 0,
      comment: null,
      systemVersioning: null,
      columns: [
        column('id', 1, 'int', { nullable: false, autoIncrement: true, key: 'PRI' }),
        column('note', 2, 'varchar(20)', { charset, collation, default: { type: 'null' } }),
      ],
      indexes: [{ table: name, name: 'PRIMARY', indexKind: 'primary', method: 'BTREE', parts: [{ column: 'id', expression: null, prefixLength: null, descending: false }], invisible: false, ignored: false, comment: null, parser: null, methodExplicit: false }],
      constraints: [{ table: name, name: 'PRIMARY', constraintType: 'primaryKey', columns: ['id'], references: null, checkClause: null, enforced: null, level: null, json: false, period: false }],
      partitioning: null,
      system: false,
    },
  };
}

/**
 * `m2_td.vis` as the step "create vis with invisible and on update" made it:
 * `id bigint unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY, secret varchar(20) INVISIBLE,
 * changed datetime(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE CURRENT_TIMESTAMP(3)`.
 */
function myVis(): { engine: 'mysql'; object: MyTableInfo } {
  const details = myOrders('vis');
  const [id, note] = details.object.columns;
  if (!id || !note) throw new Error('myOrders has id and note');
  details.object.columns = [
    { ...id, dataType: 'bigint', columnType: 'bigint unsigned' },
    { ...note, name: 'secret', position: 2, invisible: true, default: { type: 'null' } },
    {
      ...note,
      name: 'changed',
      position: 3,
      dataType: 'datetime',
      columnType: 'datetime(3)',
      nullable: false,
      charset: null,
      collation: null,
      default: { type: 'expression', value: 'CURRENT_TIMESTAMP(3)' },
      onUpdate: 'CURRENT_TIMESTAMP(3)',
    },
  ];
  return details;
}

function mySteps(mariadb: boolean): Step[] {
  const steps: Step[] = [];
  // A new table with its options; MariaDB on Aria with PAGE rows, MySQL on InnoDB with COMPACT rows (MySQL partitions only InnoDB tables).
  const made = newTableDraft('mysql', 'm2_td', 'm2_td');
  made.table = 'made';
  made.columns.push({ ...newColumn('mysql', 'label'), dataType: 'varchar(30)' });
  Object.assign(made.props, {
    engine: mariadb ? 'Aria' : 'InnoDB',
    rowFormat: mariadb ? 'page' : 'compact',
    autoIncrement: '100',
    charset: 'utf8mb4',
    collation: 'utf8mb4_bin',
    comment: 'made by the designer',
  });
  steps.push({ label: 'create made with options', changes: built(made, made) });

  // An existing table: engine, row format, AUTO_INCREMENT, comment, then a character set conversion and a rename.
  const original = draftFromDetails(myOrders());
  const draft = cloneDraft(original);
  Object.assign(draft.props, {
    engine: mariadb ? 'Aria' : 'MyISAM',
    // MyISAM keeps DYNAMIC rows for a varchar column; Aria's own format is PAGE.
    rowFormat: mariadb ? 'page' : 'dynamic',
    autoIncrement: '500',
    comment: 'orders, converted',
  });
  steps.push({ label: 'orders options', changes: built(original, draft) });
  const converted = cloneDraft(draft);
  const after = cloneDraft(draft);
  Object.assign(after.props, { charset: 'latin1', collation: 'latin1_swedish_ci', convertCharset: true, name: 'orders_renamed' });
  steps.push({ label: 'orders charset and rename', changes: built(converted, after) });

  // Partition the new table afterwards (MySQL and MariaDB partition existing tables), from the Partitions section.
  steps.push({
    label: 'partition made by hash',
    changes: [myPartitionChange('m2_td', 'made', { op: 'partitionBy', partitioning: { method: { kind: 'hash', expression: 'id' }, count: 2, partitions: [] } })],
  });

  // TD-M3: back to InnoDB; MariaDB's Aria table has ROW_FORMAT=PAGE, which InnoDB refuses, so DEFAULT goes with it.
  const renamed = cloneDraft(after);
  renamed.table = 'orders_renamed';
  const toInnodb = cloneDraft(renamed);
  toInnodb.props.engine = 'InnoDB';
  steps.push({ label: 'orders_renamed to InnoDB', changes: built(renamed, toInnodb) });

  // TD-M1: only the character set changes (latin1 to utf8mb4, converted); the collation read with the table is latin1's.
  const latin = draftFromDetails(myOrders('latin', 'latin1', 'latin1_swedish_ci'));
  const utf8 = cloneDraft(latin);
  Object.assign(utf8.props, { charset: 'utf8mb4', convertCharset: true });
  steps.push({ label: 'latin to utf8mb4', changes: built(latin, utf8) });

  // TD-L6: a comment with backslashes (a Windows path), stored as typed whatever the sql_mode.
  const commented = cloneDraft(utf8);
  commented.props.comment = "C:\\data\\new\\x */ it's; \u00fc";
  steps.push({ label: 'latin comment with backslashes', changes: built(utf8, commented) });

  // TD-H1: an engine that loses the rows: BLACKHOLE (MySQL) discards them, MEMORY (MariaDB) keeps them until a restart.
  const scratch = draftFromDetails(myOrders('scratch'));
  const lossy = cloneDraft(scratch);
  lossy.props.engine = mariadb ? 'MEMORY' : 'BLACKHOLE';
  steps.push({ label: 'scratch to a row-losing engine', changes: built(scratch, lossy) });

  // M1: INVISIBLE columns and ON UPDATE CURRENT_TIMESTAMP, set at creation from the Columns section...
  const vis = newTableDraft('mysql', 'm2_td', 'm2_td');
  vis.table = 'vis';
  // The collation myVis() reads back for secret.
  Object.assign(vis.props, { charset: 'utf8mb4', collation: 'utf8mb4_bin' });
  vis.columns.push({ ...newColumn('mysql', 'secret'), dataType: 'varchar(20)', invisible: true });
  vis.columns.push({ ...newColumn('mysql', 'changed'), dataType: 'datetime(3)', nullable: false, defaultKind: 'currentTimestamp', defaultText: '3', onUpdate: '3' });
  steps.push({ label: 'create vis with invisible and on update', changes: built(vis, vis) });
  // ...then cleared and set on an existing table: secret visible again, changed invisible without ON UPDATE.
  const visRead = draftFromDetails(myVis());
  const visEdited = cloneDraft(visRead);
  for (const c of visEdited.columns) {
    if (c.name === 'secret') c.invisible = false;
    if (c.name === 'changed') Object.assign(c, { invisible: true, onUpdate: '' });
  }
  steps.push({ label: 'vis visibility swapped and on update cleared', changes: built(visRead, visEdited) });

  // L2: subpartitions and REORGANIZE PARTITION, from the Partitions section.
  const subp = newTableDraft('mysql', 'm2_td', 'm2_td');
  subp.table = 'subp';
  steps.push({ label: 'create subp', changes: built(subp, subp) });
  steps.push({
    label: 'partition subp by range with hash subpartitions',
    changes: [
      myPartitionChange('m2_td', 'subp', {
        op: 'partitionBy',
        partitioning: {
          method: { kind: 'range', expression: 'id' },
          count: null,
          subpartition: mySubpartitioning('hash', 'id', 2),
          partitions: [
            { name: 'p0', values: myPartitionValues('range', '100') },
            { name: 'pmax', values: myPartitionValues('range', 'MAXVALUE') },
          ],
        },
      }),
    ],
  });
  const split = myReorganize(
    'range',
    ['pmax'],
    [
      { name: 'p1', values: '1000' },
      { name: 'pmax', values: 'MAXVALUE' },
    ],
    [{ name: 'pmax', description: 'MAXVALUE' }],
  );
  expect(split.errors).toEqual([]);
  if (split.op) steps.push({ label: 'split subp pmax', changes: [myPartitionChange('m2_td', 'subp', split.op)] });
  const merge = myReorganize(
    'range',
    ['p0', 'p1'],
    [{ name: 'plow', values: '1000' }],
    [
      { name: 'p0', description: '100' },
      { name: 'p1', description: '1000' },
    ],
  );
  expect(merge.errors).toEqual([]);
  if (merge.op) steps.push({ label: 'merge subp p0 and p1', changes: [myPartitionChange('m2_td', 'subp', merge.op)] });

  // LIST: the server deletes rows whose value no new partition lists, so the designer refuses dropping a value...
  const lst = newTableDraft('mysql', 'm2_td', 'm2_td');
  lst.table = 'lst';
  steps.push({ label: 'create lst', changes: built(lst, lst) });
  steps.push({
    label: 'partition lst by list',
    changes: [
      myPartitionChange('m2_td', 'lst', {
        op: 'partitionBy',
        partitioning: {
          method: { kind: 'list', expression: 'id' },
          count: null,
          partitions: [
            { name: 'a', values: myPartitionValues('list', '1, 2') },
            { name: 'b', values: myPartitionValues('list', '3') },
          ],
        },
      }),
    ],
  });
  const lstOld = [
    { name: 'a', description: '1,2' },
    { name: 'b', description: '3' },
  ];
  const lstLossy = myReorganize('list', ['a'], [{ name: 'a1', values: '1' }], lstOld);
  expect(lstLossy.op).toBeNull();
  expect(lstLossy.errors).toEqual(['Rows with the values 2 would be deleted: no new partition lists them. Add them to a new partition.']);
  // ...and splits a list partition that keeps every value.
  const lstSplit = myReorganize(
    'list',
    ['a'],
    [
      { name: 'a1', values: '1' },
      { name: 'a2', values: '+2' },
    ],
    lstOld,
  );
  expect(lstSplit.errors).toEqual([]);
  if (lstSplit.op) steps.push({ label: 'split lst a', changes: [myPartitionChange('m2_td', 'lst', lstSplit.op)] });
  return steps;
}

describe('table designer round trip samples', () => {
  it('PostgreSQL', async () => {
    await expect(`${JSON.stringify(pgSteps(), null, 2)}\n`).toMatchFileSnapshot('./__roundtrip__/postgres.json');
  });

  it('MySQL', async () => {
    await expect(`${JSON.stringify(mySteps(false), null, 2)}\n`).toMatchFileSnapshot('./__roundtrip__/mysql.json');
  });

  it('MariaDB', async () => {
    await expect(`${JSON.stringify(mySteps(true), null, 2)}\n`).toMatchFileSnapshot('./__roundtrip__/mariadb.json');
  });
});
