import { describe, expect, it } from 'vitest';
import { dbNumberLimit, dbOptions } from './redisDbOptions';

const dbs = (list: { db: number; label: string }[]): number[] => list.map((o) => o.db);

describe('dbOptions', () => {
  it('lists exactly the databases the server has', () => {
    const options = dbOptions({ mode: 'standalone', count: 32, databases: [] }, [{ db: 20, keys: '3', expires: '0' }], 20);
    expect(options).toHaveLength(32);
    expect(options[20]).toEqual({ db: 20, label: 'db20 · 3 keys' });
    expect(dbs(dbOptions({ mode: 'standalone', count: 4, databases: [] }, [], 0))).toEqual([0, 1, 2, 3]);
  });

  it('offers the databases a cluster has: 0 on Redis, more with Valkey 9 cluster-databases', () => {
    expect(dbs(dbOptions({ mode: 'cluster', count: 1, databases: [] }, [], 0))).toEqual([0]);
    expect(dbs(dbOptions({ mode: 'cluster', count: 4, databases: [] }, [], 0))).toEqual([0, 1, 2, 3]);
  });

  it('lists only 0 to 15, those with keys and the current one for a very large count', () => {
    const databases = { mode: 'standalone' as const, count: 10000, databases: [{ index: 9000, keys: 2, expires: 0, avgTtlMs: 0 }, { index: 20, keys: 0, expires: 0, avgTtlMs: 0 }] };
    expect(dbs(dbOptions(databases, [], 512))).toEqual([...Array.from({ length: 16 }, (_, i) => i), 512, 9000]);
    expect(dbNumberLimit(databases)).toBe(9999);
    expect(dbNumberLimit({ mode: 'standalone', count: 16, databases: [] })).toBeNull();
  });

  it('falls back to 0 to 15 plus databases with keys when the count is unknown', () => {
    const options = dbOptions({ mode: 'standalone', count: null, databases: [{ index: 40, keys: 1, expires: 0, avgTtlMs: 0 }] }, [], 41);
    expect(dbs(options)).toEqual([...Array.from({ length: 16 }, (_, i) => i), 40, 41]);
    expect(dbs(dbOptions(null, [], null))).toHaveLength(16);
  });
});
