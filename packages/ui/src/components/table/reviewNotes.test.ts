/**
 * The checks the designer runs on the server before a save is reviewed:
 * rows an engine change loses (TD-H1), an AUTO_INCREMENT the server raises
 * and indexes CONVERT TO makes too long (TD-L5).
 */
import type { EngineChange, MyTableInfo } from '../../api/grid';
import { draftFromDetails } from '../../lib/gridDesigner';
import { countedNote, reviewNotes } from './reviewNotes';

function table(): { engine: 'mysql'; object: MyTableInfo } {
  return {
    engine: 'mysql',
    object: {
      kind: 'table',
      database: 'shop',
      name: 'orders',
      tableType: 'BASE TABLE',
      engine: 'InnoDB',
      rowFormat: 'Compact',
      autoIncrement: 13,
      collation: 'latin1_swedish_ci',
      charset: 'latin1',
      rowEstimate: 2,
      dataLength: 0,
      indexLength: 0,
      comment: null,
      systemVersioning: null,
      columns: [
        { table: 'orders', name: 'id', position: 1, dataType: 'int', columnType: 'int', nullable: false, default: null, autoIncrement: true, onUpdate: null, generated: null, invisible: false, charset: null, collation: null, comment: null, srid: null, json: false, check: null, key: 'PRI' },
        { table: 'orders', name: 'code', position: 2, dataType: 'varchar', columnType: 'varchar(255)', nullable: true, default: null, autoIncrement: false, onUpdate: null, generated: null, invisible: false, charset: 'latin1', collation: 'latin1_swedish_ci', comment: null, srid: null, json: false, check: null, key: 'MUL' },
      ],
      indexes: [{ table: 'orders', name: 'by_code', indexKind: 'index', method: 'BTREE', parts: [{ column: 'code', expression: null, prefixLength: null, descending: false }], invisible: false, ignored: false, comment: null, parser: null, methodExplicit: false }],
      constraints: [],
      partitioning: null,
      system: false,
    },
  };
}

function alter(operations: unknown[]): EngineChange {
  return { engine: 'mysql', change: { change: 'alterTable', database: 'shop', name: 'orders', operations, systemVersioned: false } as never };
}

describe('review notes', () => {
  it('TD2-M1: gives the estimate for rows an engine change loses without counting, and lists the counts to come', async () => {
    const query = vi.fn(async () => '2');
    const details = table();
    const draft = draftFromDetails(details);
    const { notes, counts } = await reviewNotes(draft, details, [alter([{ op: 'setOptions', options: { engine: 'BLACKHOLE' } }]), alter([{ op: 'setOptions', options: { engine: 'MEMORY' } }]), alter([{ op: 'setOptions', options: { engine: 'MyISAM' } }])], query);
    expect(query).not.toHaveBeenCalled();
    expect(notes).toEqual([
      ["orders holds about 2 rows (the server's estimate): BLACKHOLE keeps none of them. The rows will be lost."],
      ["orders holds about 2 rows (the server's estimate): MEMORY keeps them only in memory, so they will be lost at the next server restart."],
      [],
    ]);
    expect(counts.map((c) => [c.step, c.note, c.engine])).toEqual([
      [0, 0, 'BLACKHOLE'],
      [1, 0, 'MEMORY'],
    ]);
    const first = counts[0];
    if (!first) throw new Error('count');
    expect(countedNote(first, 1234567)).toBe('orders holds 1234567 rows: BLACKHOLE keeps none of them. The rows will be lost.');
    expect(countedNote(first, null)).toBe("orders holds about 2 rows (the server's estimate; an exact count took too long): BLACKHOLE keeps none of them. The rows will be lost.");
  });

  it('says so when there is no estimate', async () => {
    const details = table();
    details.object.rowEstimate = null;
    const { notes } = await reviewNotes(draftFromDetails(details), details, [alter([{ op: 'setOptions', options: { engine: 'BLACKHOLE' } }])], async () => null);
    expect(notes).toEqual([['orders holds its rows (not counted): BLACKHOLE keeps none of them. The rows will be lost.']]);
  });

  it('warns when AUTO_INCREMENT is not above the largest value, and when CONVERT TO makes an index too long', async () => {
    const details = table();
    const draft = draftFromDetails(details);
    const query = vi.fn(async (sql: string) => (sql.startsWith('SELECT MAX(`id`)') ? '12' : null));
    const { notes } = await reviewNotes(draft, details, [alter([{ op: 'setOptions', options: { autoIncrement: 5 } }, { op: 'convertCharset', charset: 'utf8mb4', collation: null }])], query);
    expect(notes).toEqual([
      [
        'AUTO_INCREMENT 5 is not above the largest id (12): the server keeps it at 13 instead.',
        'Index by_code: column code takes up to 1020 bytes in utf8mb4, over the 767-byte key part limit; CONVERT TO fails unless the index uses a prefix.',
      ],
    ]);
    expect((await reviewNotes(draft, details, [alter([{ op: 'setOptions', options: { autoIncrement: 100 } }])], query)).notes).toEqual([[]]);
  });
});
