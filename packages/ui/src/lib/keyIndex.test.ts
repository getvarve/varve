import type { KeyEntry } from '../api/redis';
import { isKeyPlaceholder, KeyIndex } from './keyIndex';
import type { KeyRow } from './keyIndex';

function entry(display: string, keyType: KeyEntry['keyType'] = 'string'): KeyEntry {
  const hex = [...new TextEncoder().encode(display)].map((b) => b.toString(16).padStart(2, '0')).join('');
  return { name: hex, display, keyType, ttlMs: null };
}

const labels = (rows: readonly KeyRow[]): string[] =>
  rows.map((r) => (r.node.kind === 'prefix' ? `${r.node.label} (${r.node.count})` : r.node.label));

describe('KeyIndex', () => {
  const keys = [
    entry('session:9f2c1a', 'hash'),
    entry('session:0a17be', 'hash'),
    entry('session:guest:1'),
    entry('cart:7a91e2'),
    entry('feature:flags'),
    entry('lonely'),
  ];

  it('groups by delimiter with recursive counts, prefixes before keys', () => {
    const index = new KeyIndex(':', keys);
    expect(labels(index.rows(new Set()))).toEqual(['cart: (1)', 'feature: (1)', 'session: (3)', 'lonely']);
    const rows = index.rows(new Set(['prefix:session:']));
    expect(rows.map((r) => [r.node.label, r.level])).toEqual([
      ['cart:', 1],
      ['feature:', 1],
      ['session:', 1],
      ['session:guest:', 2],
      ['session:0a17be', 2],
      ['session:9f2c1a', 2],
      ['lonely', 1],
    ]);
    expect(rows[3]).toMatchObject({ posInSet: 1, setSize: 3, parentKey: 'prefix:session:' });
  });

  it('gives a flat list without a delimiter and handles multi-character delimiters', () => {
    expect(labels(new KeyIndex('', keys.slice(0, 2)).rows(new Set()))).toEqual(['session:0a17be', 'session:9f2c1a']);
    expect(labels(new KeyIndex('::', [entry('a::b'), entry('a::')]).rows(new Set()))).toEqual(['a:: (2)']);
    expect(labels(new KeyIndex(':', [entry('user'), entry('user:1')]).rows(new Set()))).toEqual(['user: (1)', 'user']);
  });

  it('adds pages incrementally, skips repeats and keeps the order sorted', () => {
    const index = new KeyIndex(':');
    expect(index.add([entry('b'), entry('d')])).toEqual({ added: 2, capped: false });
    expect(labels(index.rows(new Set()))).toEqual(['b', 'd']);
    expect(index.add([entry('d'), entry('a'), entry('c')])).toEqual({ added: 2, capped: false });
    expect(labels(index.rows(new Set()))).toEqual(['a', 'b', 'c', 'd']);
    expect(index.size).toBe(4);
    expect(index.get(`key:${entry('c').name}`)?.entry.display).toBe('c');
  });

  it('stops at the cap and says so', () => {
    const index = new KeyIndex(':');
    expect(index.add([entry('a'), entry('b'), entry('c')], 2)).toEqual({ added: 2, capped: true });
    expect(index.size).toBe(2);
  });

  it('keeps every key whose name was too large to read as its own entry that cannot be opened', () => {
    const big: KeyEntry = { name: '', display: '(too large to show: 600000000 bytes)', keyType: 'string', ttlMs: null };
    const index = new KeyIndex(':', [big, { ...big }, entry('x:1')]);
    const rows = index.rows(new Set());
    const oversized = rows.filter((r) => r.node.kind === 'key' && !r.node.item.openable);
    expect(oversized).toHaveLength(2);
    expect(new Set(oversized.map((r) => r.node.key)).size).toBe(2);
    expect(index.size).toBe(3);
  });

  it('keeps a real key named like the marker openable', () => {
    const real = entry('(too large to show: 5 bytes)');
    const index = new KeyIndex(':', [real]);
    expect(index.get(`key:${real.name}`)?.openable).toBe(true);
  });

  it('keeps a real key with an empty name openable', () => {
    const empty: KeyEntry = { name: '', display: '', keyType: 'string', ttlMs: null };
    const index = new KeyIndex(':', [empty, { ...empty }]);
    expect(index.size).toBe(1);
    expect(index.get('key:')?.openable).toBe(true);
  });

  it('rebuilds with another delimiter from its entries', () => {
    const index = new KeyIndex(':', keys);
    expect(labels(new KeyIndex('', index.entries()).rows(new Set()))).toHaveLength(6);
  });
});

describe('isKeyPlaceholder', () => {
  it('follows the backend flag when present, and infers it otherwise', () => {
    const marker = '(too large to show: 600000000 bytes)';
    expect(isKeyPlaceholder({ name: '', display: marker, keyType: 'unknown', ttlMs: null, placeholder: true })).toBe(true);
    // The flag wins over the inference in both directions.
    expect(isKeyPlaceholder({ name: '', display: marker, keyType: 'string', ttlMs: null, placeholder: false })).toBe(false);
    expect(isKeyPlaceholder({ name: '', display: marker, keyType: 'string', ttlMs: null })).toBe(true);
    expect(isKeyPlaceholder({ name: '6162', display: marker, keyType: 'string', ttlMs: null })).toBe(false);
  });
});
