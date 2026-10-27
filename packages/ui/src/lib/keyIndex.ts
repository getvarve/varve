import type { KeyEntry } from '../api/redis';
import { OVERSIZED_MARK } from './values';

/**
 * True for a scan entry standing in for a key whose name was too large to
 * read. Uses the backend's `placeholder` flag; without it (older backends) a
 * placeholder has an empty name AND the oversized marker, since a real key
 * may have an empty name or be named like the marker.
 */
export function isKeyPlaceholder(entry: KeyEntry): boolean {
  if (entry.placeholder !== undefined) return entry.placeholder;
  return entry.name === '' && entry.display.startsWith(OVERSIZED_MARK);
}

/** Keys a key browser keeps at most; beyond this the user narrows the MATCH pattern. */
export const MAX_SCANNED_KEYS = 100_000;

/** One scanned key with a stable identity. */
export interface IndexedKey {
  /** `key:<hex name>`; keys whose name was too large to read get `oversized:<n>` each. */
  id: string;
  entry: KeyEntry;
  /** False for keys whose name was too large to read: they cannot be opened. */
  openable: boolean;
}

export type KeyRowNode =
  | { kind: 'prefix'; key: string; label: string; count: number }
  | { kind: 'key'; key: string; label: string; item: IndexedKey };

export interface KeyRow {
  node: KeyRowNode;
  level: number;
  posInSet: number;
  setSize: number;
  parentKey: string | null;
  expanded: boolean;
}

interface Level {
  prefixes: Map<string, Level>;
  label: string;
  count: number;
  /** Keys in order, plus keys added since the last sort (merged in on demand). */
  sorted: IndexedKey[];
  pending: IndexedKey[];
  /** Sorted prefix names; `null` after a new prefix was added. */
  prefixOrder: string[] | null;
}

function newLevel(label: string): Level {
  return { prefixes: new Map(), label, count: 0, sorted: [], pending: [], prefixOrder: null };
}

const byDisplay = (a: IndexedKey, b: IndexedKey): number => (a.entry.display < b.entry.display ? -1 : a.entry.display > b.entry.display ? 1 : 0);

function mergeSorted(sorted: IndexedKey[], pending: IndexedKey[]): IndexedKey[] {
  pending.sort(byDisplay);
  const out: IndexedKey[] = new Array<IndexedKey>(sorted.length + pending.length);
  let i = 0;
  let j = 0;
  let k = 0;
  while (i < sorted.length && j < pending.length) {
    const a = sorted[i] as IndexedKey;
    const b = pending[j] as IndexedKey;
    if (byDisplay(a, b) <= 0) {
      out[k] = a;
      i += 1;
    } else {
      out[k] = b;
      j += 1;
    }
    k += 1;
  }
  while (i < sorted.length) out[k++] = sorted[i++] as IndexedKey;
  while (j < pending.length) out[k++] = pending[j++] as IndexedKey;
  return out;
}

/**
 * Scanned keys grouped by a delimiter (design doc 4.2), built incrementally:
 * adding a page costs its own size times the prefix depth, and levels are
 * sorted only when they are shown (new keys are merged into the sorted
 * list). `session:guest:1` sits under `session:` and then `session:guest:`;
 * prefix counts include every key below them; prefixes come before keys.
 * An empty delimiter gives a flat list.
 */
export class KeyIndex {
  readonly delimiter: string;
  private readonly root = newLevel('');
  private readonly byId = new Map<string, IndexedKey>();
  private readonly order: IndexedKey[] = [];
  private oversized = 0;

  constructor(delimiter: string, entries: readonly KeyEntry[] = []) {
    this.delimiter = delimiter;
    this.add(entries, Number.POSITIVE_INFINITY);
  }

  get size(): number {
    return this.order.length;
  }

  /** O(1) lookup by key id. */
  get(id: string): IndexedKey | null {
    return this.byId.get(id) ?? null;
  }

  /** Every entry in scan order, for rebuilding with another delimiter. */
  entries(): KeyEntry[] {
    return this.order.map((k) => k.entry);
  }

  /**
   * Adds a scan page. SCAN can return a key more than once; repeats are
   * skipped by name. Keys whose name was too large to read (an empty name
   * together with the oversized marker) each keep their own entry. Stops at `cap` keys.
   */
  add(page: readonly KeyEntry[], cap = MAX_SCANNED_KEYS): { added: number; capped: boolean } {
    let added = 0;
    for (const entry of page) {
      const openable = !isKeyPlaceholder(entry);
      const id = openable ? `key:${entry.name}` : `oversized:${(this.oversized += 1)}`;
      if (this.byId.has(id)) continue;
      if (this.order.length >= cap) return { added, capped: true };
      const item: IndexedKey = { id, entry, openable };
      this.byId.set(id, item);
      this.order.push(item);
      this.insert(item);
      added += 1;
    }
    return { added, capped: this.order.length >= cap };
  }

  private insert(item: IndexedKey): void {
    let level = this.root;
    if (this.delimiter !== '' && item.openable) {
      const parts = item.entry.display.split(this.delimiter);
      let prefix = '';
      for (let i = 0; i < parts.length - 1; i += 1) {
        prefix += `${parts[i] ?? ''}${this.delimiter}`;
        let next = level.prefixes.get(prefix);
        if (!next) {
          next = newLevel(prefix);
          level.prefixes.set(prefix, next);
          level.prefixOrder = null;
        }
        next.count += 1;
        level = next;
      }
    }
    level.pending.push(item);
  }

  private prefixNames(level: Level): string[] {
    if (!level.prefixOrder) level.prefixOrder = [...level.prefixes.keys()].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    return level.prefixOrder;
  }

  private keysOf(level: Level): IndexedKey[] {
    if (level.pending.length > 0) {
      level.sorted = mergeSorted(level.sorted, level.pending);
      level.pending = [];
    }
    return level.sorted;
  }

  /** The visible rows: only expanded prefixes are walked (and sorted). */
  rows(expanded: ReadonlySet<string>): KeyRow[] {
    const out: KeyRow[] = [];
    const walk = (level: Level, depth: number, parentKey: string | null): void => {
      const names = this.prefixNames(level);
      const keys = this.keysOf(level);
      const setSize = names.length + keys.length;
      let pos = 0;
      for (const name of names) {
        const child = level.prefixes.get(name) as Level;
        const key = `prefix:${name}`;
        pos += 1;
        const open = expanded.has(key);
        out.push({ node: { kind: 'prefix', key, label: child.label, count: child.count }, level: depth, posInSet: pos, setSize, parentKey, expanded: open });
        if (open) walk(child, depth + 1, key);
      }
      for (const item of keys) {
        pos += 1;
        out.push({
          node: { kind: 'key', key: item.id, label: item.entry.display, item },
          level: depth,
          posInSet: pos,
          setSize,
          parentKey,
          expanded: false,
        });
      }
    };
    walk(this.root, 1, null);
    return out;
  }
}

/** The public face of a key index, as stored in reactive tab state. */
export type KeyIndexApi = Pick<KeyIndex, 'delimiter' | 'size' | 'get' | 'entries' | 'add' | 'rows'>;
