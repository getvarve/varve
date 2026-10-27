import type { ObjectKind, ObjectSummary } from '../../api/catalog';
import {
  DEFAULT_SORT,
  defaultDatabase,
  effectiveKinds,
  firstRowInView,
  defaultSchema,
  filterRows,
  formatBytes,
  formatRowCount,
  kindChips,
  overviewKinds,
  overviewRows,
  rowNode,
  sortRows,
} from './overview';

function summary(kind: ObjectKind, name: string, more: Partial<ObjectSummary> = {}): ObjectSummary {
  return {
    object: { kind, database: 'shop', schema: 'public', parent: null, name, signature: null },
    comment: null,
    detail: null,
    flags: [],
    hasChildren: false,
    system: false,
    rowEstimate: null,
    sizeBytes: null,
    ...more,
  };
}

const rows = overviewRows([
  summary('view', 'active_customers', { comment: 'Customers who bought this year' }),
  summary('table', 'orders', { rowEstimate: 4_100_000, sizeBytes: 900 * 1024 * 1024 }),
  summary('materializedView', 'daily_revenue', { sizeBytes: 2048 }),
  summary('table', 'order_lines', { rowEstimate: 18_000_000, sizeBytes: 3 * 1024 ** 3 }),
  summary('function', 'f', { object: { kind: 'function', database: 'shop', schema: 'public', parent: null, name: 'f', signature: 'integer' } }),
  summary('table', 'archive'),
]);
const names = (list: { name: string }[]): string[] => list.map((r) => r.name);

describe('overview defaults', () => {
  it('starts on public, else the first user schema', () => {
    expect(defaultSchema([summary('schema', 'pg_catalog', { system: true }), summary('schema', 'sales'), summary('schema', 'public')])).toBe('public');
    expect(defaultSchema([summary('schema', 'pg_catalog', { system: true }), summary('schema', 'sales')])).toBe('sales');
    expect(defaultSchema([summary('schema', 'pg_catalog', { system: true })])).toBe('pg_catalog');
    expect(defaultSchema([])).toBeNull();
  });

  it('starts on the session default database, else the first user database', () => {
    const dbs = [summary('database', 'mysql', { system: true }), summary('database', 'shop'), summary('database', 'crm')];
    expect(defaultDatabase(dbs, 'crm')).toBe('crm');
    expect(defaultDatabase(dbs, null)).toBe('shop');
    expect(defaultDatabase(dbs, 'gone')).toBe('shop');
  });

  it('lists each present kind once, tables first, never schemas or databases', () => {
    expect(
      overviewKinds([
        { kind: 'function', count: 3 },
        { kind: 'view', count: 1 },
        { kind: 'table', count: 2 },
        { kind: 'schema', count: 4 },
        { kind: 'sequence', count: 0 },
      ]),
    ).toEqual(['table', 'view', 'function']);
  });
});

describe('overview rows', () => {
  it('sorts by kind then name by default, tables first', () => {
    expect(names(sortRows(rows, DEFAULT_SORT))).toEqual(['archive', 'order_lines', 'orders', 'active_customers', 'daily_revenue', 'f(integer)']);
  });

  it('sorts numbers with unknown values last in both directions', () => {
    expect(names(sortRows(rows, { column: 'rows', descending: true })).slice(0, 2)).toEqual(['order_lines', 'orders']);
    const ascending = names(sortRows(rows, { column: 'size', descending: false }));
    expect(ascending.slice(0, 3)).toEqual(['daily_revenue', 'orders', 'order_lines']);
    expect(ascending.slice(3)).toEqual(['archive', 'active_customers', 'f(integer)']);
    const descending = names(sortRows(rows, { column: 'size', descending: true }));
    expect(descending.slice(0, 3)).toEqual(['order_lines', 'orders', 'daily_revenue']);
  });

  it('filters by name or comment and by kind', () => {
    expect(names(filterRows(rows, { text: 'ORDER', kinds: new Set() }))).toEqual(['orders', 'order_lines']);
    expect(names(filterRows(rows, { text: 'this year', kinds: new Set() }))).toEqual(['active_customers']);
    expect(names(filterRows(rows, { text: '', kinds: new Set<ObjectKind>(['view', 'materializedView']) }))).toEqual([
      'active_customers',
      'daily_revenue',
    ]);
  });

  it('offers one chip per kind with counts, in display order', () => {
    expect(kindChips(rows).map((c) => `${c.label} ${c.count}`)).toEqual(['Tables 3', 'Views 1', 'Materialized views 1', 'Functions 1']);
  });

  it('reads row and size estimates when the backend sends them', () => {
    const orders = rows.find((r) => r.name === 'orders');
    expect(orders?.rows).toBe(4_100_000);
    expect(rows.find((r) => r.name === 'archive')?.rows).toBeNull();
    expect(formatBytes(900 * 1024 * 1024)).toBe('900.0 MiB');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(null)).toBe('');
    expect(formatRowCount(null)).toBe('');
    const node = orders && rowNode('pg', orders);
    expect(node?.type).toBe('object');
    expect(node?.summary.object.name).toBe('orders');
  });
});

describe('overview filters and keyboard start', () => {
  it('ignores chosen kinds that are no longer in the list', () => {
    const tablesOnly = rows.filter((r) => r.kind === 'table');
    expect([...effectiveKinds(new Set<ObjectKind>(['view']), tablesOnly)]).toEqual([]);
    expect([...effectiveKinds(new Set<ObjectKind>(['view', 'table']), tablesOnly)]).toEqual(['table']);
    // Applied, an absent kind no longer hides everything.
    expect(filterRows(tablesOnly, { text: '', kinds: effectiveKinds(new Set<ObjectKind>(['view']), tablesOnly) }).length).toBe(3);
  });

  it('finds the first row in view', () => {
    expect(firstRowInView(0, 28, 10)).toBe(0);
    expect(firstRowInView(28 * 4, 28, 10)).toBe(4);
    expect(firstRowInView(28 * 4 + 5, 28, 10)).toBe(5);
    expect(firstRowInView(28 * 40, 28, 10)).toBe(9);
    expect(firstRowInView(0, 28, 0)).toBe(-1);
  });
});
