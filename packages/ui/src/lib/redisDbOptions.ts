import type { RedisDatabases } from '../api/live';
import type { DbKeyspace } from '../api/redis';
import { formatCount } from './values';

/** One entry of the Redis database picker. */
export interface DbOption {
  db: number;
  label: string;
}

/** Redis's default number of databases, used only when the server does not say how many it has. */
const DEFAULT_DATABASES = 16;

/** Above this many databases the picker lists only 0 to 15, those with keys and the current one; a number field reaches the rest. */
export const MAX_LISTED_DATABASES = 64;

/**
 * The databases a Redis tab can switch to: `0..count` as the server reports
 * it (`redis_databases`: in cluster mode 1, or Valkey 9's `cluster-databases`;
 * more than 16 when configured), else databases 0 to 15 plus every database
 * that has keys. Above {@link MAX_LISTED_DATABASES} only 0 to 15 and those
 * with keys are listed. The current database is always listed. Key counts
 * come from the keyspace.
 */
export function dbOptions(databases: RedisDatabases | null, keyspace: readonly DbKeyspace[], current: number | null): DbOption[] {
  const dbs = new Set<number>();
  const count = databases?.count ?? null;
  if (count !== null && count <= MAX_LISTED_DATABASES) for (let i = 0; i < count; i += 1) dbs.add(i);
  else {
    for (let i = 0; i < Math.min(DEFAULT_DATABASES, count ?? DEFAULT_DATABASES); i += 1) dbs.add(i);
    for (const d of databases?.databases ?? []) if (d.keys > 0 || count === null) dbs.add(d.index);
  }
  for (const d of keyspace) dbs.add(d.db);
  if (current !== null) dbs.add(current);
  return [...dbs]
    .sort((a, b) => a - b)
    .map((db) => {
      const info = keyspace.find((d) => d.db === db);
      return { db, label: info ? `db${db} · ${formatCount(info.keys)} keys` : `db${db}` };
    });
}

/** The highest database number to offer through the number field, when the list does not show them all; `null` otherwise. */
export function dbNumberLimit(databases: RedisDatabases | null): number | null {
  const count = databases?.count ?? null;
  return count !== null && count > MAX_LISTED_DATABASES ? count - 1 : null;
}
