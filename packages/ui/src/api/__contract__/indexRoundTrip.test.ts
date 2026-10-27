/**
 * The table designer's index redefinition, from what the servers really
 * report. `samples/<engine>-index-describe.<table>.<server>.json` are written
 * and checked by crates/catalog/tests/ui_index_round_trip.rs against the live
 * servers (PostgreSQL 13 and 18; MySQL 8.0 and 9.7, MariaDB 10.6 and 12.3);
 * this test turns each into the designer's changes and keeps them in
 * `samples/<engine>-index-redefine.<table>.<server>.json`, which the Rust test
 * runs and then checks that every index came back exactly as it was.
 */
import type { EngineChange, MyIndexDef, PgCreateIndex, TableDetails } from '../grid';
import { buildDraftChanges, cloneDraft, draftFromDetails } from '../../lib/gridDesigner';

const described = import.meta.glob<TableDetails>('./samples/*-index-describe.*.json', { import: 'default', eager: true });

/** Every index the designer lets the user redefine, redefined: not those behind a constraint or locked. */
function redefineAll(details: TableDetails) {
  const original = draftFromDetails(details);
  const draft = cloneDraft(original);
  // Not the leftover of an earlier failed swap: the replica identity swap drops it.
  for (const i of draft.indexes) if (!i.constraint && !i.primary && !i.locked && !i.name.includes('_varve_swap')) i.changed = true;
  return buildDraftChanges(original, draft);
}

function pgCreates(changes: EngineChange[]): Map<string, PgCreateIndex> {
  return new Map(
    changes.flatMap((c) => (c.engine === 'postgres' && c.change.object === 'index' && c.change.action === 'create' ? [[c.change.name ?? '', c.change] as const] : [])),
  );
}

function myAdds(changes: EngineChange[]): Map<string, MyIndexDef> {
  return new Map(
    changes.flatMap((c) =>
      c.engine === 'mysql' && c.change.change === 'alterTable'
        ? c.change.operations.flatMap((op) => (op.op === 'addIndex' ? [[op.index.name ?? '', op.index] as const] : []))
        : [],
    ),
  );
}

beforeEach(() => {
  // The swap name's token is random per attempt; fixed here so the samples are stable.
  vi.spyOn(Math, 'random').mockReturnValue(0.5);
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('index redefinition from real describe output', () => {
  it('has samples for the oldest and newest version of every engine', () => {
    const servers = new Set(Object.keys(described).map((p) => /\.(\w+)\.json$/.exec(p)?.[1]));
    expect([...servers].sort()).toEqual(['ma106', 'ma123', 'my80', 'my97', 'pg13', 'pg18']);
  });

  for (const [path, details] of Object.entries(described)) {
    const [, engine, slug, server] = /\/(\w+)-index-describe\.(\w+)\.(\w+)\.json$/.exec(path) ?? [];
    const major = Number(/\d+/.exec(server ?? '')?.[0] ?? 0);
    it(`rebuilds every index of ${slug ?? ''} as read on ${server ?? ''}`, async () => {
      const { changes, errors } = redefineAll(details);
      expect(errors).toEqual([]);
      if (engine === 'pg' && slug === 'orders') {
        const byName = pgCreates(changes);
        // C1: an access method from an extension travels as custom.
        expect(byName.get('orders_bloom')).toMatchObject({ method: { kind: 'custom', name: 'bloom' }, storageParams: [{ name: 'length', value: '80' }] });
        // H1: names as stored, never quoted twice; M1: everything else as read.
        expect(byName.get('Orders_Mixed')).toMatchObject({
          elements: [
            { key: { column: 'OrderId' }, order: 'desc', nulls: 'last' },
            { key: { column: 'we"ird' }, collation: { schema: 'pg_catalog', name: 'POSIX' }, opclass: { schema: 'pg_catalog', name: 'text_pattern_ops' } },
          ],
          include: ['amount', 'label'],
          storageParams: [
            { name: 'fillfactor', value: '70' },
            { name: 'deduplicate_items', value: 'off' },
          ],
          tablespace: 'm2_uicontract_ts',
          predicate: 'amount > 0::numeric',
        });
        expect(byName.get('orders_body')).toMatchObject({
          method: { kind: 'gist' },
          elements: [{ key: { column: 'body' }, opclass: { schema: 'pg_catalog', name: 'tsvector_ops' }, opclassOptions: [{ name: 'siglen', value: '100' }] }],
        });
        if (major >= 15) expect(byName.get('orders_nnd')).toMatchObject({ unique: true, nullsNotDistinct: true });
        // RA-L3: storage parameters in their stored order.
        expect(byName.get('orders_brin')?.storageParams).toEqual([
          { name: 'pages_per_range', value: '32' },
          { name: 'autosummarize', value: 'on' },
        ]);
        // RA-L1: an explicit COLLATE "default" on an expression stays.
        expect(byName.get('orders_stats')?.elements[1]).toMatchObject({ collation: { schema: 'pg_catalog', name: 'default' } });
        // RA-H3: the replica identity index is built under another name, takes over, then replaces the old one.
        expect(byName.has('orders_replid')).toBe(false);
        // RA3-L3: the leftover of a failed earlier attempt is dropped first, and this attempt has its own name.
        const swapAt = changes.findIndex((c) => c.engine === 'postgres' && c.change.object === 'index' && c.change.action === 'create' && c.change.name === 'orders_replid_varve_swap_i00000');
        expect(changes[swapAt - 1]).toEqual({ engine: 'postgres', change: { object: 'index', action: 'drop', name: { schema: 'm2_uicontract', name: 'orders_replid_varve_swap' }, concurrently: true } });
        expect(changes.slice(swapAt + 1, swapAt + 4)).toEqual([
          { engine: 'postgres', change: { object: 'table', action: 'alter', name: { schema: 'm2_uicontract', name: 'Orders' }, operations: [{ op: 'replicaIdentityUsingIndex', index: 'orders_replid_varve_swap_i00000' }] } },
          { engine: 'postgres', change: { object: 'index', action: 'drop', name: { schema: 'm2_uicontract', name: 'orders_replid' }, concurrently: true } },
          { engine: 'postgres', change: { object: 'index', action: 'alter', name: { schema: 'm2_uicontract', name: 'orders_replid_varve_swap_i00000' }, operations: [{ op: 'rename', newName: 'orders_replid' }] } },
        ]);
        // RA-L2: CLUSTER ON and column statistics come back.
        expect(changes).toContainEqual({
          engine: 'postgres',
          change: { object: 'table', action: 'alter', name: { schema: 'm2_uicontract', name: 'Orders' }, operations: [{ op: 'clusterOn', index: 'orders_clu' }] },
        });
        expect(changes).toContainEqual({
          engine: 'postgres',
          change: { object: 'index', action: 'alter', name: { schema: 'm2_uicontract', name: 'orders_stats' }, operations: [{ op: 'setStatistics', column: 1, target: 500 }] },
        });
        expect(changes).toContainEqual({
          engine: 'postgres',
          change: { object: 'comment', target: { kind: 'index', database: null, schema: 'm2_uicontract', parent: null, name: 'Orders_Mixed', signature: null }, comment: 'mixed keys' },
        });
        // RA-M1: the index a foreign key needs is not offered.
        const draft = draftFromDetails(details);
        expect(draft.indexes.find((i) => i.name === 'orders_ref')?.locked).toMatch(/refs_code_fkey/);
      }
      if (engine === 'pg' && slug === 'parted') {
        // RA-M1: never CONCURRENTLY on a partitioned table.
        expect(changes.map((c) => (c.change as { concurrently?: boolean }).concurrently)).toEqual([false, false]);
      }
      if (engine === 'pg' && slug === 'parted_p1') {
        const draft = draftFromDetails(details);
        expect(draft.indexes.find((i) => !i.constraint)?.locked).toMatch(/partitioned index m2_uicontract\.parted_k/);
      }
      if (engine === 'my' || engine === 'ma') {
        const byName = myAdds(changes);
        // RA-M2: the method as read (MEMORY must stay BTREE) and the full-text parser.
        if (slug === 'mem') expect(byName.get('kb')).toMatchObject({ method: 'btree' });
        if (slug === 'items') {
          expect(byName.get('u_ac')).toMatchObject({ kind: 'unique', method: 'btree' });
          // RA-M4: no prefix on spatial key parts.
          expect(byName.get('sp')).toEqual({ name: 'sp', kind: 'spatial', parts: [{ kind: 'column', name: 'g', length: null, descending: false }] });
          if (engine === 'my') {
            expect(byName.get('ft')).toMatchObject({ kind: 'fulltext', parser: 'ngram' });
            // RA-M3: functional key parts unescaped.
            expect(byName.get('k_concat')?.parts).toEqual([{ kind: 'expression', sql: "concat(`c`,_utf8mb4'x')", descending: false }]);
          }
        }
      }
      await expect(`${JSON.stringify(changes, null, 2)}\n`).toMatchFileSnapshot(`./samples/${engine ?? ''}-index-redefine.${slug ?? ''}.${server ?? ''}.json`);
    });
  }
});
