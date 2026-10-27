import type { DesignerDetails, MyColumnInfo, MyTableInfo, PgColumnInfo, PgIndexInfo, PgIndexKeyInfo, PgTableInfo, TableDetails } from '../api/grid';
import type { IndexDraft } from './gridDesigner';
import {
  buildDraftChanges,
  changedProps,
  collationOf,
  convertIndexWarnings,
  defaultPartitionName,
  engineRowLoss,
  hashModuli,
  pgBoundProblem,
  rowFormatFor,
  cloneDraft,
  draftFromDetails,
  moveColumn,
  myColumnDef,
  myBoundValue,
  myListValueKey,
  myPartitionValues,
  myReorganize,
  mySubpartitioning,
  newColumn,
  newConstraint,
  newIndex,
  newTableDraft,
  nextRangePartitions,
  parseDateRange,
  parseMyColumnType,
  parsePgName,
  pgIndexMethod,
  pgNameText,
  swapName,
  pgBoundValue,
  pgCheckExpression,
  pgCreatePartition,
  pgDetachPartition,
  splitList,
  summarize,
} from './gridDesigner';

afterEach(() => {
  vi.restoreAllMocks();
});

function pgColumn(name: string, position: number, extra: Partial<PgColumnInfo> = {}): PgColumnInfo {
  return {
    name,
    position,
    dataType: 'text',
    notNull: false,
    default: null,
    identity: null,
    generated: null,
    collation: null,
    local: true,
    inheritedCount: 0,
    comment: null,
    ...extra,
  };
}

function pgTable(): TableDetails {
  const object: PgTableInfo = {
    kind: 'table',
    schema: 'public',
    name: 'orders',
    owner: 'migrator',
    unlogged: false,
    tablespace: null,
    storageParameters: [],
    inherits: [],
    partitioning: null,
    partitionOf: null,
    rowEstimate: 10,
    totalBytes: 8192,
    comment: null,
    system: false,
    columns: [
      pgColumn('id', 1, {
        dataType: 'bigint',
        notNull: true,
        identity: { generation: 'always', sequence: null, start: '1', increment: '1', min: null, max: null, cache: '1', cycle: false },
      }),
      pgColumn('total', 2, { dataType: 'numeric(12,2)', notNull: true, default: '0' }),
      pgColumn('note', 3),
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
      {
        name: 'orders_total_check',
        constraintType: 'check',
        columns: ['total'],
        definition: 'CHECK ((total >= (0)::numeric)) NOT VALID',
        deferrable: false,
        deferred: false,
        validated: false,
        local: true,
        inheritedCount: 0,
        hasParentConstraint: false,
        index: null,
        foreignKey: null,
        comment: null,
      },
    ],
    indexes: [
      {
        schema: 'public',
        name: 'orders_note_idx',
        method: 'btree',
        unique: false,
        primary: false,
        exclusion: false,
        valid: true,
        partitioned: false,
        parentIndex: null,
        nullsNotDistinct: false,
        clustered: false,
        replicaIdentity: false,
        foreignKeys: [],
        keys: [{ definition: 'note', column: 'note', expression: false, descending: false, nullsFirst: false, collation: null, opclass: null, opclassOptions: [], statisticsTarget: null }],
        includeColumns: [],
        predicate: null,
        storageParameters: [],
        tablespace: null,
        sizeBytes: 16384,
        constraint: null,
        definition: 'CREATE INDEX orders_note_idx ON public.orders USING btree (note)',
        comment: null,
      },
    ],
  };
  return { engine: 'postgres', object };
}

function myColumn(name: string, position: number, columnType: string, extra: Partial<MyColumnInfo> = {}): MyColumnInfo {
  return {
    table: 'orders',
    name,
    position,
    dataType: columnType.replace(/\(.*$/, '').split(' ')[0] ?? columnType,
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
  };
}

function myTable(): TableDetails {
  const object: MyTableInfo = {
    kind: 'table',
    database: 'shop',
    name: 'orders',
    tableType: 'BASE TABLE',
    engine: 'InnoDB',
    rowFormat: 'Dynamic',
    autoIncrement: 4,
    collation: 'utf8mb4_0900_ai_ci',
    charset: 'utf8mb4',
    rowEstimate: 3,
    dataLength: 16384,
    indexLength: 0,
    comment: null,
    systemVersioning: null,
    columns: [
      myColumn('id', 1, 'bigint unsigned', { nullable: false, autoIncrement: true }),
      myColumn('status', 2, "enum('new','it''s')", { nullable: false, default: { type: 'literal', value: 'new' }, charset: 'utf8mb4', collation: 'utf8mb4_0900_ai_ci' }),
      myColumn('changed', 3, 'datetime(3)', { default: { type: 'expression', value: 'CURRENT_TIMESTAMP(3)' }, onUpdate: 'CURRENT_TIMESTAMP(3)' }),
    ],
    indexes: [
      { table: 'orders', name: 'PRIMARY', indexKind: 'primary', method: 'BTREE', parts: [{ column: 'id', expression: null, prefixLength: null, descending: false }], invisible: false, ignored: false, comment: null, parser: null, methodExplicit: false },
      { table: 'orders', name: 'status_idx', indexKind: 'index', method: 'BTREE', parts: [{ column: 'status', expression: null, prefixLength: null, descending: false }], invisible: false, ignored: false, comment: null, parser: null, methodExplicit: false },
    ],
    constraints: [
      { table: 'orders', name: 'PRIMARY', constraintType: 'primaryKey', columns: ['id'], references: null, checkClause: null, enforced: null, level: null, json: false, period: false },
    ],
    partitioning: null,
    system: false,
  };
  return { engine: 'mysql', object };
}

describe('PostgreSQL designer', () => {
  it('reads the table into a draft', () => {
    const d = draftFromDetails(pgTable());
    expect(d.columns.map((c) => [c.name, c.identity, c.defaultKind])).toEqual([
      ['id', 'always', 'none'],
      ['total', 'none', 'expression'],
      ['note', 'none', 'none'],
    ]);
    expect(d.constraints[1]?.expression).toBe('total >= (0)::numeric');
    expect(pgCheckExpression('CHECK ((a > 1))')).toBe('a > 1');
  });

  it('turns edits into index drops, one ALTER TABLE, index creates and comments, in that order', () => {
    const original = draftFromDetails(pgTable());
    const draft = cloneDraft(original);
    const total = draft.columns[1];
    const note = draft.columns[2];
    const idx = draft.indexes[0];
    if (!total || !note || !idx) throw new Error('fixture');
    total.dataType = 'numeric(14,2)';
    total.name = 'amount';
    note.nullable = false;
    note.comment = 'Free text';
    idx.dropped = true;
    const added = newColumn('postgres', 'currency');
    added.dataType = 'char(3)';
    added.defaultKind = 'expression';
    added.defaultText = "'EUR'";
    draft.columns.push(added);
    const fk = newConstraint('foreignKey', 'orders_currency_fkey');
    fk.columns = ['currency'];
    fk.refTable = 'currencies';
    fk.refColumns = ['code'];
    fk.onDelete = 'restrict';
    fk.notValid = true;
    draft.constraints.push(fk);
    const index = newIndex('postgres', 'orders_amount_idx');
    index.parts.push({ text: 'amount', expression: false, descending: true, nulls: '', extra: '' });
    index.predicate = "currency <> 'XXX'";
    draft.indexes.push(index);

    const { changes, errors } = buildDraftChanges(original, draft);
    expect(errors).toEqual([]);
    expect(changes).toEqual([
      { engine: 'postgres', change: { object: 'index', action: 'drop', name: { schema: 'public', name: 'orders_note_idx' }, concurrently: true } },
      {
        engine: 'postgres',
        change: {
          object: 'table',
          action: 'alter',
          name: { schema: 'public', name: 'orders' },
          operations: [
            { op: 'renameColumn', name: 'total', newName: 'amount' },
            { op: 'alterColumnType', name: 'amount', dataType: 'numeric(14,2)', collation: null, using: null },
            { op: 'setNotNull', column: 'note' },
            { op: 'addColumn', column: { name: 'currency', dataType: 'char(3)', default: "'EUR'" } },
            {
              op: 'addConstraint',
              notValid: true,
              constraint: {
                name: 'orders_currency_fkey',
                deferrable: false,
                initiallyDeferred: false,
                kind: {
                  type: 'foreignKey',
                  columns: ['currency'],
                  references: { schema: 'public', name: 'currencies' },
                  referencedColumns: ['code'],
                  matchType: null,
                  onDelete: { kind: 'restrict' },
                  onUpdate: { kind: 'noAction' },
                },
              },
            },
          ],
        },
      },
      {
        engine: 'postgres',
        change: {
          object: 'index',
          action: 'create',
          name: 'orders_amount_idx',
          table: { schema: 'public', name: 'orders' },
          unique: false,
          nullsNotDistinct: false,
          concurrently: true,
          method: { kind: 'btree' },
          elements: [{ key: { column: 'amount' }, order: 'desc' }],
          include: [],
          storageParams: [],
          tablespace: null,
          predicate: "currency <> 'XXX'",
        },
      },
      {
        engine: 'postgres',
        change: {
          object: 'comment',
          target: { kind: 'column', database: null, schema: 'public', parent: 'orders', name: 'note', signature: null },
          comment: 'Free text',
        },
      },
    ]);
    expect(summarize(original, draft)).toMatchObject({ total: 6, columns: { added: 1, modified: 2 } });
  });

  it('redefines an index exactly as read: extension method, unquoted names, collation, operator class, options', () => {
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('fixture');
    const key = (k: Partial<PgIndexKeyInfo> & { definition: string }): PgIndexKeyInfo => ({
      column: null,
      expression: false,
      descending: false,
      nullsFirst: false,
      collation: null,
      opclass: null,
      opclassOptions: [],
      statisticsTarget: null,
      ...k,
    });
    details.object.indexes = [
      {
        ...(details.object.indexes[0] as PgIndexInfo),
        name: 'Orders_Mixed',
        method: 'hnsw',
        unique: true,
        nullsNotDistinct: true,
        keys: [
          key({ definition: '"OrderId"', column: 'OrderId', descending: true, nullsFirst: true }),
          key({
            definition: '"we""ird"',
            column: 'we"ird',
            collation: { schema: 'pg_catalog', name: 'POSIX' },
            opclass: { schema: 'My Ops', name: 'vector_l2_ops' },
            opclassOptions: [{ name: 'siglen', value: '32' }],
          }),
          key({ definition: 'lower(note)', expression: true, opclass: { schema: 'pg_catalog', name: 'text_pattern_ops' } }),
        ],
        includeColumns: ['Amount', 'x"y'],
        storageParameters: [
          { name: 'deduplicate_items', value: 'off' },
          { name: 'fillfactor', value: '70' },
        ],
        tablespace: 'fast space',
        predicate: 'total > 0',
        comment: 'mixed keys',
      },
    ];
    const original = draftFromDetails(details);
    const draft = cloneDraft(original);
    const idx = draft.indexes[0];
    if (!idx) throw new Error('fixture');
    idx.changed = true;
    // The opclass field shows SQL text; the schema-qualified name goes back exactly.
    expect(idx.parts.map((p) => [p.text, p.extra])).toEqual([
      ['OrderId', ''],
      ['we"ird', '"My Ops".vector_l2_ops'],
      ['lower(note)', 'text_pattern_ops'],
    ]);
    const { changes, errors } = buildDraftChanges(original, draft);
    expect(errors).toEqual([]);
    expect(changes).toEqual([
      { engine: 'postgres', change: { object: 'index', action: 'drop', name: { schema: 'public', name: 'Orders_Mixed' }, concurrently: true } },
      {
        engine: 'postgres',
        change: {
          object: 'index',
          action: 'create',
          name: 'Orders_Mixed',
          table: { schema: 'public', name: 'orders' },
          unique: true,
          nullsNotDistinct: true,
          concurrently: true,
          method: { kind: 'custom', name: 'hnsw' },
          elements: [
            { key: { column: 'OrderId' }, order: 'desc' },
            {
              key: { column: 'we"ird' },
              collation: { schema: 'pg_catalog', name: 'POSIX' },
              opclass: { schema: 'My Ops', name: 'vector_l2_ops' },
              opclassOptions: [{ name: 'siglen', value: '32' }],
            },
            { key: { expression: 'lower(note)' }, opclass: { schema: 'pg_catalog', name: 'text_pattern_ops' } },
          ],
          include: ['Amount', 'x"y'],
          // In the order the server stores them.
          storageParams: [
            { name: 'deduplicate_items', value: 'off' },
            { name: 'fillfactor', value: '70' },
          ],
          tablespace: 'fast space',
          predicate: 'total > 0',
        },
      },
      {
        engine: 'postgres',
        change: {
          object: 'comment',
          target: { kind: 'index', database: null, schema: 'public', parent: null, name: 'Orders_Mixed', signature: null },
          comment: 'mixed keys',
        },
      },
    ]);
  });

  it('sends core methods by kind, others as custom, and parses typed operator classes', () => {
    expect(pgIndexMethod('gin')).toEqual({ kind: 'gin' });
    expect(pgIndexMethod('ivfflat')).toEqual({ kind: 'custom', name: 'ivfflat' });
    expect(parsePgName('Text_Pattern_Ops')).toEqual({ schema: null, name: 'text_pattern_ops' });
    expect(parsePgName('"My Ops"."A""b"')).toEqual({ schema: 'My Ops', name: 'A"b' });
    expect(parsePgName('a.b.c')).toBeNull();
    expect(parsePgName('"open')).toBeNull();
    expect(pgNameText({ schema: 'My Ops', name: 'x' })).toBe('"My Ops".x');
    // A changed key drops the collation that belonged to the old one; a typed class replaces the read one.
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('fixture');
    const first = details.object.indexes[0] as PgIndexInfo;
    details.object.indexes = [
      { ...first, keys: [{ ...(first.keys[0] as PgIndexKeyInfo), collation: { schema: 'pg_catalog', name: 'C' }, opclass: { schema: 'pg_catalog', name: 'text_pattern_ops' }, opclassOptions: [] }] },
    ];
    const original = draftFromDetails(details);
    const draft = cloneDraft(original);
    const idx = draft.indexes[0];
    const part = idx?.parts[0];
    if (!idx || !part) throw new Error('fixture');
    idx.changed = true;
    part.text = 'total';
    part.extra = '"Ops".numeric_ops';
    const create = buildDraftChanges(original, draft).changes[1]?.change;
    expect(create).toMatchObject({ elements: [{ key: { column: 'total' }, opclass: { schema: 'Ops', name: 'numeric_ops' } }] });
    expect(create && 'elements' in create ? create.elements[0] : null).not.toHaveProperty('collation');
    part.extra = '"broken';
    expect(buildDraftChanges(original, draft).errors.some((e) => e.includes('operator class'))).toBe(true);
  });

  it('keeps the table usable while redefining: replica identity swap, locks, no CONCURRENTLY on partitioned tables', () => {
    vi.spyOn(Math, 'random').mockReturnValue(0.5);
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('fixture');
    const base = details.object.indexes[0] as PgIndexInfo;
    details.object.indexes = [
      {
        ...base,
        name: 'rid',
        unique: true,
        replicaIdentity: true,
        clustered: true,
        keys: [{ ...(base.keys[0] as PgIndexKeyInfo), definition: 'id', column: 'id' }],
        storageParameters: [
          { name: 'deduplicate_items', value: 'off' },
          { name: 'fillfactor', value: '80' },
        ],
      },
      { ...base, name: 'child', parentIndex: { schema: 'public', name: 'parent_idx' } },
      { ...base, name: 'ref_u', unique: true, foreignKeys: ['public.lines.lines_order_fkey'] },
    ];
    const original = draftFromDetails(details);
    const draft = cloneDraft(original);
    for (const i of draft.indexes) i.changed = true;
    const { changes, errors } = buildDraftChanges(original, draft);
    expect(errors).toEqual([
      'Index child: Attached to the partitioned index public.parent_idx; change it on the partitioned table.',
      'Index ref_u: Foreign keys reference this index (public.lines.lines_order_fkey); drop or change them first.',
    ]);
    const kinds = changes.map((c) => (c.engine === 'postgres' ? `${c.change.object}:${'action' in c.change ? c.change.action : ''}` : ''));
    // Built under another name, made the replica identity and clustered, then swapped in.
    expect(kinds).toEqual(['index:create', 'table:alter', 'index:drop', 'index:alter']);
    expect(changes[0]?.change).toMatchObject({
      name: 'rid_varve_swap_i00000',
      storageParams: [
        { name: 'deduplicate_items', value: 'off' },
        { name: 'fillfactor', value: '80' },
      ],
    });
    expect(changes[1]?.change).toMatchObject({ operations: [{ op: 'replicaIdentityUsingIndex', index: 'rid_varve_swap_i00000' }, { op: 'clusterOn', index: 'rid_varve_swap_i00000' }] });
    expect(changes[3]?.change).toMatchObject({ name: { name: 'rid_varve_swap_i00000' }, operations: [{ op: 'rename', newName: 'rid' }] });
    expect(swapName('x'.repeat(63), 'abc123')).toBe(`${'x'.repeat(45)}_varve_swap_abc123`);
    // A partitioned table builds and drops its indexes without CONCURRENTLY.
    details.object.kind = 'partitionedTable';
    details.object.indexes = [{ ...base, partitioned: true }];
    const parted = draftFromDetails(details);
    expect(parted.partitioned).toBe(true);
    expect(parted.indexes[0]?.concurrently).toBe(false);
  });

  it('refuses a replica identity index that could no longer be one, and clears leftovers of failed swaps', () => {
    vi.spyOn(Math, 'random').mockReturnValue(0.25);
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('fixture');
    const base = details.object.indexes[0] as PgIndexInfo;
    const key = base.keys[0] as PgIndexKeyInfo;
    const idKey = { ...key, definition: 'id', column: 'id' };
    details.object.indexes = [
      { ...base, name: 'rid', unique: true, replicaIdentity: true, keys: [idKey] },
      { ...base, name: 'rid_varve_swap', unique: true, valid: false, keys: [idKey] },
      { ...base, name: 'rid_varve_swap_x1y2z3', unique: true, keys: [idKey] },
      { ...base, name: 'other_varve_swap_abc', unique: true, keys: [idKey] },
    ];
    const original = draftFromDetails(details);

    // Redefined onto a nullable column with a WHERE: refused, nothing planned.
    const bad = cloneDraft(original);
    const rid = bad.indexes[0];
    if (!rid?.parts[0]) throw new Error('fixture');
    rid.changed = true;
    rid.parts[0].text = 'note';
    rid.predicate = 'id > 0';
    const refused = buildDraftChanges(original, bad);
    expect(refused.changes).toEqual([]);
    expect(refused.errors).toEqual([
      "Index rid: it is the table's replica identity, which needs a UNIQUE index without WHERE or expressions on NOT NULL columns (it has a WHERE clause; note allows NULL). Keep that, or change the table's replica identity first.",
    ]);

    // Unchanged definition: both leftovers of rid are dropped first (shown in the preview), a fresh name is used.
    const good = cloneDraft(original);
    if (good.indexes[0]) good.indexes[0].changed = true;
    const planned = buildDraftChanges(original, good);
    expect(planned.errors).toEqual([]);
    expect(planned.changes.slice(0, 3).map((c) => c.change)).toEqual([
      { object: 'index', action: 'drop', name: { schema: 'public', name: 'rid_varve_swap' }, concurrently: true },
      { object: 'index', action: 'drop', name: { schema: 'public', name: 'rid_varve_swap_x1y2z3' }, concurrently: true },
      expect.objectContaining({ object: 'index', action: 'create', name: 'rid_varve_swap_900000' }),
    ]);

    // A swap index that is the replica identity: an earlier attempt stopped halfway.
    details.object.indexes = [
      { ...base, name: 'rid', unique: true, replicaIdentity: false, keys: [idKey] },
      { ...base, name: 'rid_varve_swap_x1y2z3', unique: true, replicaIdentity: true, keys: [idKey] },
    ];
    const half = draftFromDetails(details);
    const ridAgain = cloneDraft(original);
    ridAgain.indexes = [{ ...(half.indexes[0] as IndexDraft), replicaIdentity: true, changed: true }, half.indexes[1] as IndexDraft];
    expect(buildDraftChanges(original, ridAgain).errors[0]).toMatch(/stopped halfway and rid_varve_swap_x1y2z3 is now the replica identity/);
  });

  it('keeps the MySQL index method and full-text parser when redefining', () => {
    const details = myTable();
    if (details.engine !== 'mysql') throw new Error('fixture');
    const pk = details.object.indexes[0];
    if (!pk) throw new Error('fixture');
    details.object.indexes = [
      { ...pk, name: 'kb', indexKind: 'index', method: 'BTREE', methodExplicit: true },
      { ...pk, name: 'ki', indexKind: 'index', method: 'BTREE', methodExplicit: false },
      { ...pk, name: 'ft', indexKind: 'fulltext', method: null, parser: 'ngram' },
    ];
    const original = draftFromDetails(details);
    const draft = cloneDraft(original);
    for (const i of draft.indexes) i.changed = true;
    const ops = buildDraftChanges(original, draft).changes.flatMap((c) => (c.engine === 'mysql' && c.change.change === 'alterTable' ? c.change.operations : []));
    expect(ops.filter((op) => op.op === 'addIndex').map((op) => (op.op === 'addIndex' ? op.index : null))).toEqual([
      { name: 'kb', kind: 'index', method: 'btree', parts: [{ kind: 'column', name: 'id', length: null, descending: false }] },
      // The engine's default is left to the engine.
      { name: 'ki', kind: 'index', parts: [{ kind: 'column', name: 'id', length: null, descending: false }] },
      { name: 'ft', kind: 'fulltext', parser: 'ngram', parts: [{ kind: 'column', name: 'id', length: null, descending: false }] },
    ]);
  });

  it('locks constraints it cannot write back: unknown types and MATCH PARTIAL', () => {
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('fixture');
    const pk = details.object.constraints[0];
    if (!pk) throw new Error('fixture');
    details.object.constraints = [
      { ...pk, name: 'odd', constraintType: 't', definition: 'TRIGGER ...' },
      {
        ...pk,
        name: 'fk_partial',
        constraintType: 'foreignKey',
        definition: 'FOREIGN KEY (a, b) REFERENCES t(a, b) MATCH PARTIAL',
        foreignKey: { referenced: { schema: 'public', name: 't' }, referencedColumns: ['a', 'b'], onUpdate: 'a', onDelete: 'a', matchType: 'partial', onDeleteSetColumns: [] },
      },
    ];
    const original = draftFromDetails(details);
    expect(original.constraints.map((k) => k.locked)).toEqual([expect.stringMatching(/does not know \(t\)/), expect.stringMatching(/MATCH PARTIAL/)]);
    const draft = cloneDraft(original);
    for (const k of draft.constraints) k.changed = true;
    expect(buildDraftChanges(original, draft).errors).toHaveLength(2);
  });

  it('refuses drafts PostgreSQL cannot do or that are not safe', () => {
    const original = draftFromDetails(pgTable());
    const draft = cloneDraft(original);
    const note = draft.columns[2];
    const total = draft.columns[1];
    if (!note || !total) throw new Error('fixture');
    note.generated = 'upper(name)';
    total.defaultText = '0); DROP TABLE x; --';
    const check = newConstraint('check', '');
    draft.constraints.push(check);
    const { errors } = buildDraftChanges(original, draft);
    expect(errors.some((e) => e.includes('cannot turn an existing column into a generated column'))).toBe(true);
    expect(errors.some((e) => e.startsWith('Default of total'))).toBe(true);
    expect(errors.some((e) => e.includes('is empty'))).toBe(true);
  });

  it('drops and adds a constraint whose definition changed, renames otherwise', () => {
    const original = draftFromDetails(pgTable());
    const draft = cloneDraft(original);
    const check = draft.constraints[1];
    const pk = draft.constraints[0];
    if (!check || !pk) throw new Error('fixture');
    check.expression = 'total > 0';
    check.changed = true;
    pk.name = 'orders_pk';
    const alter = buildDraftChanges(original, draft).changes[0]?.change as { operations: { op: string }[] };
    expect(alter.operations.map((o) => o.op)).toEqual(['renameConstraint', 'dropConstraint', 'addConstraint']);
  });

  it('sets, keeps and clears constraint comments under their final names, after the ALTER TABLE', () => {
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('fixture');
    const [pkInfo, checkInfo] = details.object.constraints;
    if (!pkInfo || !checkInfo) throw new Error('fixture');
    pkInfo.comment = 'the key';
    checkInfo.comment = 'never negative';
    const original = draftFromDetails(details);
    expect(original.constraints.map((k) => k.comment)).toEqual(['the key', 'never negative']);
    const draft = cloneDraft(original);
    const [pk, check] = draft.constraints;
    if (!pk || !check) throw new Error('fixture');
    expect(buildDraftChanges(original, draft).changes).toEqual([]);

    // A comment change alone: one COMMENT ON CONSTRAINT, and the constraint counts as modified.
    pk.comment = '';
    expect(summarize(original, draft).constraints.modified).toBe(1);
    const target = (name: string) => ({ kind: 'constraint', database: null, schema: 'public', parent: 'orders', name, signature: null });
    expect(buildDraftChanges(original, draft).changes).toEqual([{ engine: 'postgres', change: { object: 'comment', target: target('orders_pkey'), comment: null } }]);

    // Renamed and redefined: the redefined check lost its comment with the drop, so it is set again under its new name.
    pk.name = 'orders_pk';
    pk.comment = 'renamed key';
    check.name = 'orders_total_positive';
    check.expression = 'total > 0';
    check.changed = true;
    const fresh = newConstraint('check', 'orders_note_check');
    Object.assign(fresh, { expression: "note <> ''", comment: 'no empty notes' });
    draft.constraints.push(fresh);
    const { changes, errors } = buildDraftChanges(original, draft);
    expect(errors).toEqual([]);
    expect(changes[0]?.change).toMatchObject({ object: 'table', action: 'alter' });
    expect(changes.slice(1)).toEqual([
      { engine: 'postgres', change: { object: 'comment', target: target('orders_pk'), comment: 'renamed key' } },
      { engine: 'postgres', change: { object: 'comment', target: target('orders_total_positive'), comment: 'never negative' } },
      { engine: 'postgres', change: { object: 'comment', target: target('orders_note_check'), comment: 'no empty notes' } },
    ]);

    // PostgreSQL names an unnamed constraint itself: it needs a name to take a comment.
    fresh.name = '';
    expect(buildDraftChanges(original, draft).errors).toContain('Constraint new check: give it a name to set its comment.');
  });

  it('comments constraints of a new table after the CREATE TABLE', () => {
    const draft = newTableDraft('postgres', 'public', '');
    draft.table = 'items';
    for (const k of draft.constraints) Object.assign(k, { name: 'items_pkey', comment: 'the key' });
    const { changes, errors } = buildDraftChanges(draft, draft);
    expect(errors).toEqual([]);
    expect(changes.map((c) => (c.change as { object: string }).object)).toEqual(['table', 'comment']);
    expect(changes[1]?.change).toEqual({ object: 'comment', target: { kind: 'constraint', database: null, schema: 'public', parent: 'items', name: 'items_pkey', signature: null }, comment: 'the key' });
  });

  it('builds partition actions', () => {
    expect(pgBoundValue('MAXVALUE')).toEqual({ kind: 'maxValue' });
    expect(pgBoundValue("'it''s'")).toEqual({ kind: 'literal', value: "it's" });
    expect(splitList("1, 'a, b', (2)")).toEqual(['1', "'a, b'", '(2)']);
    expect(pgCreatePartition('public', 'events', 'events_2027_01', { kind: 'range', from: "'2027-01-01'", to: "'2027-02-01'" }, null)).toEqual({
      engine: 'postgres',
      change: {
        object: 'table',
        action: 'create',
        name: { schema: 'public', name: 'events_2027_01' },
        partitionOf: {
          parent: { schema: 'public', name: 'events' },
          bound: { kind: 'range', from: [{ kind: 'literal', value: '2027-01-01' }], to: [{ kind: 'literal', value: '2027-02-01' }] },
        },
        tablespace: null,
      },
    });
    expect(pgDetachPartition('public', 'events', { schema: 'public', name: 'events_2025_10' }, true, true).map((c) => (c.change as { action: string }).action)).toEqual([
      'alter',
      'drop',
    ]);
  });

  it('proposes the next date range partitions', () => {
    expect(parseDateRange("FOR VALUES FROM ('2026-11-01 00:00:00+00') TO ('2026-12-01 00:00:00+00')")).toEqual({
      from: '2026-11-01',
      to: '2026-12-01',
      suffix: ' 00:00:00+00',
    });
    const next = nextRangePartitions(
      ["FOR VALUES FROM ('2026-11-01') TO ('2026-12-01')", "FOR VALUES FROM ('2026-12-01') TO ('2027-01-01')", 'DEFAULT'],
      3,
      'month',
      'events_{YYYY}_{MM}',
    );
    expect(next).toEqual([
      { name: 'events_2027_01', from: "'2027-01-01'", to: "'2027-02-01'" },
      { name: 'events_2027_02', from: "'2027-02-01'", to: "'2027-03-01'" },
      { name: 'events_2027_03', from: "'2027-03-01'", to: "'2027-04-01'" },
    ]);
    expect(nextRangePartitions(['FOR VALUES IN (1)'], 2, 'day', 'p')).toEqual([]);
  });
});

describe('MySQL designer', () => {
  it('parses column types', () => {
    expect(parseMyColumnType('int(11) unsigned zerofill')).toEqual({ name: 'int', length: 11, unsigned: true, zerofill: true });
    expect(parseMyColumnType('decimal(14,2)')).toEqual({ name: 'decimal', length: 14, scale: 2 });
    expect(parseMyColumnType("enum('a','it''s')")).toEqual({ name: 'enum', values: ['a', "it's"] });
    expect(parseMyColumnType('double precision')).toEqual({ name: 'double' });
    expect(parseMyColumnType('longtext', true)).toEqual({ name: 'json' });
    expect(() => parseMyColumnType('mystery(1)')).toThrow(/not a type/);
  });

  it('modifies with the previous definition, renames with CHANGE, and moves columns', () => {
    const details = myTable();
    const original = draftFromDetails(details);
    expect(original.columns[2]).toMatchObject({ defaultKind: 'currentTimestamp', defaultText: '3', onUpdate: '3' });
    const draft = cloneDraft(original);
    const status = draft.columns[1];
    if (!status) throw new Error('fixture');
    status.name = 'state';
    status.comment = 'Order state';
    const changed = draft.columns[2];
    if (!changed) throw new Error('fixture');
    moveColumn(draft, changed.id, -1);
    const infos = details.engine === 'mysql' ? details.object.columns : [];
    const { changes, errors } = buildDraftChanges(original, draft, infos);
    expect(errors).toEqual([]);
    const ops = (changes[0]?.change as { operations: Record<string, unknown>[] }).operations;
    expect(ops.map((o) => o.op)).toEqual(['modifyColumn', 'changeColumn']);
    expect(ops[0]).toMatchObject({ op: 'modifyColumn', position: { kind: 'after', column: 'id' } });
    expect(ops[1]).toMatchObject({
      op: 'changeColumn',
      indexed: true,
      position: null,
      previous: {
        name: 'status',
        nullable: false,
        default: { kind: 'literal', value: { t: 'text', v: 'new' } },
        dataType: { name: 'enum', values: ['new', "it's"], charset: 'utf8mb4', collation: 'utf8mb4_0900_ai_ci' },
      },
      column: { name: 'state', comment: 'Order state' },
    });
  });

  it('drops indexes before adding, and handles the primary key through constraints', () => {
    const details = myTable();
    const original = draftFromDetails(details);
    const draft = cloneDraft(original);
    const idx = draft.indexes[1];
    const pk = draft.constraints[0];
    if (!idx || !pk) throw new Error('fixture');
    idx.changed = true;
    idx.method = 'unique';
    pk.dropped = true;
    const { changes } = buildDraftChanges(original, draft, details.engine === 'mysql' ? details.object.columns : []);
    const ops = (changes[0]?.change as { operations: Record<string, unknown>[] }).operations;
    expect(ops).toEqual([
      { op: 'dropIndex', name: 'status_idx' },
      { op: 'dropPrimaryKey' },
      { op: 'addIndex', index: { name: 'status_idx', kind: 'unique', parts: [{ kind: 'column', name: 'status', length: null, descending: false }] } },
    ]);
  });

  it('sends BIT defaults beyond 2^53 as exact text', () => {
    const c = newColumn('mysql', 'flags');
    c.dataType = 'bit(64)';
    c.defaultKind = 'literal';
    c.defaultText = `b'${'1'.repeat(64)}'`;
    expect(myColumnDef(c).default).toEqual({ kind: 'literal', value: { t: 'uInt', v: '18446744073709551615' } });
    c.defaultText = "b'101'";
    expect(myColumnDef(c).default).toEqual({ kind: 'literal', value: { t: 'uInt', v: 5 } });
  });

  it('refuses a LIST reorganize that would delete rows, naming the values', () => {
    const old = [
      { name: 'a', description: "'nl','de'" },
      { name: 'b', description: "'fr'" },
      { name: 'c', description: '(1,\'x\'),(2,\'y\')' },
    ];
    // Merging a and b without fr loses its rows; the same values written another way are kept.
    expect(myReorganize('list columns', ['a', 'b'], [{ name: 'ab', values: "'de', 'nl'" }], old).errors).toEqual([
      "Rows with the values 'fr' would be deleted: no new partition lists them. Add them to a new partition.",
    ]);
    expect(myReorganize('list columns', ['a', 'b'], [{ name: 'ab', values: "'de','nl', 'fr'" }], old).errors).toEqual([]);
    expect(myReorganize('list columns', ['c'], [{ name: 'c1', values: "(1, 'x')" }], old).errors[0]).toContain("(2,'y')");
    expect(myReorganize('list columns', ['c'], [{ name: 'c1', values: "(1, 'x'), ( 2,'y' )" }], old).errors).toEqual([]);
    expect(myListValueKey('+007')).toBe(myListValueKey('7'));
    expect(myListValueKey("'a'")).not.toBe(myListValueKey('a'));
    // A DEFAULT partition, or one whose values are not known, is not reorganized here.
    expect(myReorganize('list', ['d'], [{ name: 'd', values: '9' }], [{ name: 'd', description: 'DEFAULT' }]).errors[0]).toMatch(/DEFAULT partition/);
    expect(myReorganize('list', ['e'], [{ name: 'e', values: '9' }], [{ name: 'e', description: null }]).errors[0]).toMatch(/not known/);
    // RANGE is checked by the server (it keeps the total range).
    expect(myReorganize('range', ['p'], [{ name: 'p', values: '5' }], [{ name: 'p', description: '10' }]).errors).toEqual([]);
  });

  it('builds REORGANIZE PARTITION and SUBPARTITION BY, and refuses what the server would', () => {
    expect(myPartitionValues('list', "1, 'a'")).toEqual({ kind: 'in', rows: [[{ kind: 'value', value: { t: 'int', v: 1 } }], [{ kind: 'value', value: { t: 'text', v: 'a' } }]] });
    expect(myPartitionValues('hash', '1')).toBeNull();
    const old = [{ name: 'p0', description: '1,2' }];
    expect(myReorganize('list', ['p0'], [{ name: 'p0', values: '1, 2' }], old).op).toEqual({ op: 'reorganizePartitions', names: ['p0'], into: [{ name: 'p0', values: myPartitionValues('list', '1, 2') }] });
    expect(myReorganize('hash', ['p0'], [{ name: 'p0', values: '' }], old).errors[0]).toMatch(/added or coalesced/);
    expect(myReorganize('range', [], [{ name: 'a', values: '1' }], []).errors).toEqual(['Choose the partitions to reorganize.']);
    expect(
      myReorganize(
        'range columns',
        ['p0'],
        [
          { name: 'a', values: '1' },
          { name: 'A', values: '' },
        ],
        [],
      ).errors,
    ).toEqual(['Two new partitions are called A.', 'New partition A: give its LESS THAN bound.']);
    expect(mySubpartitioning('none', 'id', 2)).toBeNull();
    expect(mySubpartitioning('key', 'a, b', 0)).toEqual({ method: { kind: 'key', columns: ['a', 'b'] }, count: null });
  });

  it('reads partition bounds', () => {
    expect(myBoundValue('MAXVALUE')).toEqual({ kind: 'maxValue' });
    expect(myBoundValue('2027')).toEqual({ kind: 'value', value: { t: 'int', v: 2027 } });
    expect(myBoundValue("'x'")).toEqual({ kind: 'value', value: { t: 'text', v: 'x' } });
    expect(myBoundValue("TO_DAYS('2027-01-01')")).toEqual({ kind: 'expression', sql: "TO_DAYS('2027-01-01')" });
  });
});

describe('table properties', () => {
  it('PostgreSQL: one ALTER TABLE for owner, logging, storage, tablespace and inheritance; comment; rename and SET SCHEMA last', () => {
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('postgres');
    details.object.storageParameters = [
      { name: 'fillfactor', value: '90' },
      { name: 'autovacuum_enabled', value: 'false' },
    ];
    details.object.inherits = [{ schema: 'public', name: 'base' }];
    const original = draftFromDetails(details);
    expect(original.props).toMatchObject({ name: 'orders', schema: 'public', owner: 'migrator', storageParams: [{ name: 'fillfactor', value: '90' }, { name: 'autovacuum_enabled', value: 'false' }], inherits: ['public.base'] });
    const draft = cloneDraft(original);
    draft.columns.push({ ...newColumn('postgres', 'remark'), dataType: 'text' });
    Object.assign(draft.props, {
      name: 'Orders 2',
      schema: 'archive',
      owner: 'app',
      comment: 'All orders',
      unlogged: true,
      tablespace: 'fast',
      inherits: ['other.base2'],
      storageParams: [
        { name: 'fillfactor', value: '70' },
        { name: 'toast.autovacuum_enabled', value: 'off' },
      ],
    });
    expect(changedProps(original, draft).sort()).toEqual(['comment', 'inherits', 'name', 'owner', 'schema', 'storageParams', 'tablespace', 'unlogged']);
    expect(summarize(original, draft).properties.total).toBe(8);
    const { changes, errors } = buildDraftChanges(original, draft);
    expect(errors).toEqual([]);
    expect(changes).toEqual([
      { engine: 'postgres', change: { object: 'table', action: 'alter', name: { schema: 'public', name: 'orders' }, operations: [{ op: 'addColumn', column: expect.objectContaining({ name: 'remark' }) }] } },
      {
        engine: 'postgres',
        change: {
          object: 'table',
          action: 'alter',
          name: { schema: 'public', name: 'orders' },
          operations: [
            { op: 'owner', owner: { kind: 'name', name: 'app' } },
            { op: 'setLogged', logged: false },
            { op: 'resetStorageParams', names: ['autovacuum_enabled'] },
            {
              op: 'setStorageParams',
              params: [
                { name: 'fillfactor', value: '70' },
                { name: 'toast.autovacuum_enabled', value: 'off' },
              ],
            },
            { op: 'setTablespace', tablespace: 'fast' },
            { op: 'noInherit', parent: { schema: 'public', name: 'base' } },
            { op: 'inherit', parent: { schema: 'other', name: 'base2' } },
          ],
        },
      },
      { engine: 'postgres', change: { object: 'comment', target: { kind: 'table', database: null, schema: 'public', parent: null, name: 'orders', signature: null }, comment: 'All orders' } },
      {
        engine: 'postgres',
        change: {
          object: 'table',
          action: 'alter',
          name: { schema: 'public', name: 'orders' },
          operations: [
            { op: 'rename', newName: 'Orders 2' },
            { op: 'setSchema', schema: 'archive' },
          ],
        },
      },
    ]);
  });

  it('PostgreSQL: clears the comment, moves back to pg_default, and refuses bad parameters and parents', () => {
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('postgres');
    details.object.comment = 'old';
    details.object.tablespace = 'fast';
    const original = draftFromDetails(details);
    const draft = cloneDraft(original);
    draft.props.comment = '';
    draft.props.tablespace = '';
    expect(buildDraftChanges(original, draft).changes).toEqual([
      { engine: 'postgres', change: { object: 'table', action: 'alter', name: { schema: 'public', name: 'orders' }, operations: [{ op: 'setTablespace', tablespace: 'pg_default' }] } },
      { engine: 'postgres', change: { object: 'comment', target: expect.objectContaining({ kind: 'table', name: 'orders' }), comment: null } },
    ]);
    draft.props.storageParams = [
      { name: 'fill factor', value: '1' },
      { name: 'fillfactor', value: '' },
    ];
    draft.props.inherits = ['a.b.c'];
    draft.props.name = ' ';
    draft.props.owner = '';
    expect(buildDraftChanges(original, draft).errors).toEqual([
      'Choose the owner role.',
      'Storage parameter "fill factor": use a parameter name such as fillfactor or toast.autovacuum_enabled.',
      'Storage parameter fillfactor: give a value.',
      'Parent table "a.b.c": write it as name or schema.name.',
      'The table needs a name.',
    ]);
  });

  it('MySQL: only the changed options, CONVERT TO when asked, rename last, in an ALTER TABLE of its own', () => {
    const original = draftFromDetails(myTable());
    expect(original.props).toMatchObject({ engine: 'InnoDB', rowFormat: 'dynamic', autoIncrement: '4', charset: 'utf8mb4', collation: 'utf8mb4_0900_ai_ci' });
    const draft = cloneDraft(original);
    draft.props.engine = 'innodb';
    expect(changedProps(original, draft)).toEqual([]);
    Object.assign(draft.props, { engine: 'Aria', rowFormat: 'page', autoIncrement: '1000', comment: 'shop orders', name: 'orders_old' });
    const { changes, errors } = buildDraftChanges(original, draft);
    expect(errors).toEqual([]);
    expect(changes).toEqual([
      {
        engine: 'mysql',
        change: {
          change: 'alterTable',
          database: 'shop',
          name: 'orders',
          operations: [
            { op: 'setOptions', options: { engine: 'Aria', rowFormat: 'page', autoIncrement: 1000, comment: 'shop orders' } },
            { op: 'rename', name: 'orders_old' },
          ],
          systemVersioned: false,
        },
      },
    ]);
    const charset = cloneDraft(original);
    charset.props.charset = 'latin1';
    charset.props.collation = 'latin1_swedish_ci';
    expect(buildDraftChanges(original, charset).changes[0]).toMatchObject({ change: { operations: [{ op: 'setOptions', options: { charset: 'latin1', collation: 'latin1_swedish_ci' } }] } });
    charset.props.convertCharset = true;
    expect(buildDraftChanges(original, charset).changes[0]).toMatchObject({ change: { operations: [{ op: 'convertCharset', charset: 'latin1', collation: 'latin1_swedish_ci' }] } });
    charset.props.autoIncrement = '12.5';
    expect(buildDraftChanges(original, charset).errors).toEqual(['AUTO_INCREMENT must be a whole number from 1 up.']);
  });

  it('MySQL: a new table carries its options', () => {
    const draft = newTableDraft('mysql', 'shop', 'shop');
    Object.assign(draft.props, { engine: 'MyISAM', rowFormat: 'fixed', autoIncrement: '100', charset: 'utf8mb4', collation: 'utf8mb4_bin', comment: 'log' });
    const { changes, errors } = buildDraftChanges(draft, draft);
    expect(errors).toEqual([]);
    expect(changes[0]).toMatchObject({
      change: { change: 'createTable', options: { engine: 'MyISAM', rowFormat: 'fixed', autoIncrement: 100, charset: 'utf8mb4', collation: 'utf8mb4_bin', comment: 'log' } },
    });
    expect(buildDraftChanges(newTableDraft('mysql', 'shop', 'shop'), newTableDraft('mysql', 'shop', 'shop')).changes[0]).toMatchObject({ change: { options: {} } });
  });
});

describe('new PostgreSQL partitioned table', () => {
  it('sends PARTITION BY, the owner and the default partition', () => {
    const draft = newTableDraft('postgres', 'sales', '', true);
    expect(draft.partitionBy).toEqual({ enabled: true, strategy: 'range', keys: [{ text: 'id', expression: false }], defaultPartition: false });
    draft.table = 'events';
    draft.columns.push({ ...newColumn('postgres', 'at'), dataType: 'timestamptz', nullable: false });
    draft.partitionBy.keys = [
      { text: 'id', expression: false },
      { text: 'at', expression: false },
    ];
    for (const k of draft.constraints) if (k.type === 'primaryKey') k.columns = ['id', 'at'];
    draft.partitionBy.defaultPartition = true;
    Object.assign(draft.props, { owner: 'app', tablespace: 'fast', comment: 'events' });
    const { changes, errors } = buildDraftChanges(draft, draft);
    expect(errors).toEqual([]);
    expect(changes).toEqual([
      {
        engine: 'postgres',
        change: expect.objectContaining({
          object: 'table',
          action: 'create',
          name: { schema: 'sales', name: 'events' },
          partitionBy: { strategy: 'range', keys: [{ key: { column: 'id' } }, { key: { column: 'at' } }] },
          unlogged: false,
          inherits: [],
          storageParams: [],
          tablespace: 'fast',
          comment: 'events',
        }),
      },
      { engine: 'postgres', change: { object: 'table', action: 'alter', name: { schema: 'sales', name: 'events' }, operations: [{ op: 'owner', owner: { kind: 'name', name: 'app' } }] } },
      pgCreatePartition('sales', 'events', 'events_default', { kind: 'default' }, null),
    ]);
  });

  it('sends expression keys and refuses what PostgreSQL refuses', () => {
    const draft = newTableDraft('postgres', 'sales', '', true);
    draft.partitionBy.strategy = 'hash';
    draft.partitionBy.keys = [{ text: 'lower(name)', expression: true }];
    draft.partitionBy.defaultPartition = true;
    draft.props.unlogged = true;
    draft.props.storageParams = [{ name: 'fillfactor', value: '80' }];
    expect(buildDraftChanges(draft, draft).errors).toEqual([
      'A hash partitioned table cannot have a default partition.',
      'A partitioned table cannot be unlogged.',
      'A partitioned table takes no storage parameters; set them on its partitions.',
      'The primary key: a partitioned table with an expression key cannot have primary key or unique constraints.',
    ]);
    const list = newTableDraft('postgres', 'sales', '', true);
    list.partitionBy.strategy = 'list';
    list.partitionBy.keys = [
      { text: 'id', expression: false },
      { text: 'nope', expression: false },
    ];
    expect(buildDraftChanges(list, list).errors).toEqual([
      'List partitioning takes exactly one key.',
      'Partition key nope is not a column of the table.',
      'The primary key must include every partition key column (nope).',
    ]);
    const off = newTableDraft('postgres', 'sales', '');
    expect(buildDraftChanges(off, off).changes[0]).toMatchObject({ change: { partitionBy: null } });
    expect(off.partitioned).toBe(false);
  });
});

describe('materialized view indexes', () => {
  function matView(): DesignerDetails {
    return {
      engine: 'postgres',
      object: {
        kind: 'materializedView',
        schema: 'rep',
        name: 'totals',
        owner: 'app',
        populated: true,
        tablespace: null,
        comment: null,
        system: false,
        columns: [pgColumn('day', 1, { dataType: 'date' }), pgColumn('total', 2, { dataType: 'numeric' })],
        indexes: [],
      },
    };
  }

  it('designs only the indexes: a unique index is created on the view, columns are locked', () => {
    const original = draftFromDetails(matView());
    expect(original.kind).toBe('materializedView');
    expect(original.columns.every((c) => c.locked !== null)).toBe(true);
    const draft = cloneDraft(original);
    const i = newIndex('postgres', 'totals_day');
    i.unique = true;
    i.parts = [{ text: 'day', expression: false, descending: false, nulls: '', extra: '' }];
    draft.indexes.push(i);
    const { changes, errors } = buildDraftChanges(original, draft);
    expect(errors).toEqual([]);
    expect(changes).toEqual([
      {
        engine: 'postgres',
        change: expect.objectContaining({ object: 'index', action: 'create', name: 'totals_day', table: { schema: 'rep', name: 'totals' }, unique: true, elements: [{ key: { column: 'day' } }] }),
      },
    ]);
    expect(summarize(original, draft).total).toBe(1);
  });
});

describe('audit fixes (table designer)', () => {
  it('TD-M1: CONVERT TO sends no stale collation, and refuses one of another character set', () => {
    const original = draftFromDetails(myTable());
    const draft = cloneDraft(original);
    draft.props.charset = 'latin1';
    draft.props.convertCharset = true;
    expect(buildDraftChanges(original, draft).changes[0]).toMatchObject({ change: { operations: [{ op: 'convertCharset', charset: 'latin1', collation: null }] } });
    draft.props.convertCharset = false;
    expect(buildDraftChanges(original, draft).changes[0]).toMatchObject({ change: { operations: [{ op: 'setOptions', options: { charset: 'latin1' } }] } });
    expect(JSON.stringify(buildDraftChanges(original, draft).changes)).not.toContain('utf8mb4_0900_ai_ci');
    draft.props.collation = 'utf8mb4_bin';
    expect(buildDraftChanges(original, draft).errors).toEqual([
      "Collation utf8mb4_bin does not belong to character set latin1; choose one of latin1, or leave it empty for the character set's default.",
    ]);
    expect(collationOf('utf8mb4', 'utf8mb4_0900_ai_ci')).toBe(true);
    expect(collationOf('utf8', 'utf8mb3_general_ci')).toBe(true);
    expect(collationOf('utf8mb4', 'uca1400_ai_ci')).toBe(true);
    expect(collationOf('utf8mb4', 'latin1_swedish_ci')).toBe(false);
  });

  it('TD-M3: an engine change resets a row format the new engine refuses', () => {
    const details = myTable();
    if (details.engine !== 'mysql') throw new Error('mysql');
    details.object.engine = 'Aria';
    details.object.rowFormat = 'Page';
    const original = draftFromDetails(details);
    const draft = cloneDraft(original);
    draft.props.engine = 'InnoDB';
    expect(buildDraftChanges(original, draft).changes[0]).toMatchObject({ change: { operations: [{ op: 'setOptions', options: { engine: 'InnoDB', rowFormat: 'default' } }] } });
    draft.props.rowFormat = 'fixed';
    expect(buildDraftChanges(original, draft).errors).toEqual(['Row format FIXED does not work with InnoDB; choose another one.']);
    draft.props.rowFormat = 'compact';
    expect(buildDraftChanges(original, draft).changes[0]).toMatchObject({ change: { operations: [{ op: 'setOptions', options: { engine: 'InnoDB', rowFormat: 'compact' } }] } });
    expect(rowFormatFor('MyISAM', 'dynamic')).toBe('dynamic');
    expect(rowFormatFor('MEMORY', 'dynamic')).toBe('default');
    expect(rowFormatFor('ColumnStore', 'page')).toBe('page');
    const created = newTableDraft('mysql', 'shop', 'shop');
    Object.assign(created.props, { engine: 'InnoDB', rowFormat: 'page' });
    expect(buildDraftChanges(created, created).errors).toEqual(['Row format PAGE does not work with InnoDB; choose another one.']);
  });

  it('TD-H1: names the engines that lose rows', () => {
    expect(engineRowLoss('blackhole')).toBe('discarded');
    expect(engineRowLoss('MEMORY')).toBe('restart');
    expect(engineRowLoss('InnoDB')).toBeNull();
    expect(engineRowLoss('ARCHIVE')).toBeNull();
  });

  it('TD-L5: warns when CONVERT TO makes an InnoDB index too long', () => {
    const details = myTable();
    if (details.engine !== 'mysql') throw new Error('mysql');
    details.object.rowFormat = 'Compact';
    const base = details.object.columns[1];
    if (!base) throw new Error('column');
    details.object.columns.push({ ...base, name: 'code', position: 9, columnType: 'varchar(255)' });
    details.object.indexes.push({ table: 'orders', name: 'by_code', indexKind: 'index', method: 'BTREE', parts: [{ column: 'code', expression: null, prefixLength: null, descending: false }], invisible: false, ignored: false, comment: null, parser: null, methodExplicit: false });
    expect(convertIndexWarnings(details.object, 'utf8mb4')).toEqual([
      'Index by_code: column code takes up to 1020 bytes in utf8mb4, over the 767-byte key part limit; CONVERT TO fails unless the index uses a prefix.',
    ]);
    expect(convertIndexWarnings(details.object, 'latin1')).toEqual([]);
    details.object.rowFormat = 'Dynamic';
    expect(convertIndexWarnings(details.object, 'utf8mb4')).toEqual([]);
  });

  it('TD-L2: the default partition name fits 63 bytes; longer table names are refused', () => {
    expect(defaultPartitionName('events')).toBe('events_default');
    const long = 'a'.repeat(60);
    expect(defaultPartitionName(long)).toBe(`${'a'.repeat(55)}_default`);
    expect(new TextEncoder().encode(defaultPartitionName('é'.repeat(31))).length).toBeLessThanOrEqual(63);
    expect(defaultPartitionName('é'.repeat(31))).toBe(`${'é'.repeat(27)}_default`);
    const draft = newTableDraft('postgres', 's', '', true);
    draft.table = long;
    draft.partitionBy.defaultPartition = true;
    const { changes, errors } = buildDraftChanges(draft, draft);
    expect(errors).toEqual([]);
    expect(changes.at(-1)).toMatchObject({ change: { name: { name: `${'a'.repeat(55)}_default` }, partitionOf: { parent: { name: long } } } });
    draft.table = 'x'.repeat(64);
    expect(buildDraftChanges(draft, draft).errors).toContain('PostgreSQL names are at most 63 bytes; shorten the table name.');
  });

  it('TD-L1: checks hash moduli against the existing ones, and NULL in range bounds', () => {
    const moduli = hashModuli(['FOR VALUES WITH (modulus 4, remainder 0)', 'FOR VALUES WITH (modulus 4, remainder 1)', 'FOR VALUES WITH (modulus 8, remainder 2)']);
    expect(moduli).toEqual([4, 4, 8]);
    expect(pgBoundProblem({ kind: 'hash', modulus: 8, remainder: 6 }, moduli)).toBeNull();
    expect(pgBoundProblem({ kind: 'hash', modulus: 16, remainder: 3 }, moduli)).toBeNull();
    expect(pgBoundProblem({ kind: 'hash', modulus: 6, remainder: 3 }, moduli)).toMatch(/^Every hash modulus must be a factor of the next larger one: 4 does not divide 6/);
    expect(pgBoundProblem({ kind: 'hash', modulus: 4, remainder: 4 }, moduli)).toBe('The remainder must be from 0 to the modulus minus 1.');
    expect(pgBoundProblem({ kind: 'range', from: 'NULL', to: '10' })).toMatch(/cannot hold NULL/);
    expect(pgBoundProblem({ kind: 'range', from: '1, 2', to: '3' })).toBe('FROM and TO need the same number of values.');
    expect(pgBoundProblem({ kind: 'list', values: 'NULL' })).toBeNull();
  });

  it('TD-L4: an emptied tablespace goes back to the database default', () => {
    const details = pgTable();
    if (details.engine !== 'postgres') throw new Error('postgres');
    details.object.tablespace = 'fast';
    const original = draftFromDetails(details);
    original.props.databaseTablespace = 'db_space';
    const draft = cloneDraft(original);
    draft.props.tablespace = '';
    expect(buildDraftChanges(original, draft).changes[0]).toMatchObject({ change: { operations: [{ op: 'setTablespace', tablespace: 'db_space' }] } });
    expect(changedProps(original, draft)).toEqual(['tablespace']);
  });

  it('TD-L7: unique indexes and exclusion constraints of a new partitioned table must cover the key', () => {
    const draft = newTableDraft('postgres', 's', '', true);
    draft.columns.push({ ...newColumn('postgres', 'at'), dataType: 'date' });
    draft.partitionBy.keys = [{ text: 'at', expression: false }];
    draft.constraints = [];
    const unique = newIndex('postgres', 'u_id');
    unique.unique = true;
    unique.parts = [{ text: 'id', expression: false, descending: false, nulls: '', extra: '' }];
    draft.indexes.push(unique);
    const ex = newConstraint('exclusion', 'no_overlap');
    ex.exclusionMethod = 'gist';
    ex.exclusion = [{ text: 'id', expression: false, operator: '=' }];
    draft.constraints.push(ex);
    expect(buildDraftChanges(draft, draft, [], 16).errors).toEqual([
      'Unique index u_id must include every partition key column (at).',
      'Exclusion constraint no_overlap: PostgreSQL 16 has no exclusion constraints on partitioned tables (17 and later do).',
    ]);
    expect(buildDraftChanges(draft, draft, [], 17).errors).toContain('Exclusion constraint no_overlap must compare every partition key column with = (at).');
    unique.parts.push({ text: 'at', expression: false, descending: false, nulls: '', extra: '' });
    ex.exclusion.push({ text: 'at', expression: false, operator: '=' });
    expect(buildDraftChanges(draft, draft, [], 17).errors).toEqual([]);
  });
});
