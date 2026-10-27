import type { ObjectRef } from '../api/catalog';
import type { ObjectDetails } from '../api/ddl';
import { buildObjectComment, commentChanged, commentOf, commentSupported, normalizeComment } from './objectComment';

const constraint: ObjectRef = { kind: 'constraint', database: 'shop', schema: 'public', parent: 'orders', name: 'orders_pk', signature: null };

describe('commentSupported', () => {
  it('takes every PostgreSQL kind the browser lists', () => {
    for (const kind of ['constraint', 'partition', 'aggregate', 'operator', 'cast', 'collation', 'publication', 'subscription', 'foreignDataWrapper', 'foreignServer', 'foreignTable', 'tablespace', 'index', 'extension'] as const) {
      expect(commentSupported('postgres', kind)).toBe(true);
    }
  });

  it('refuses kinds PostgreSQL does not have and every MySQL or MariaDB kind (their comments live in their own editors)', () => {
    expect(commentSupported('postgres', 'event')).toBe(false);
    expect(commentSupported('postgres', 'package')).toBe(false);
    for (const engine of ['mysql', 'mariadb'] as const) {
      for (const kind of ['function', 'procedure', 'event', 'view', 'trigger', 'package', 'constraint', 'table'] as const) expect(commentSupported(engine, kind)).toBe(false);
    }
    expect(commentSupported('redis', 'table')).toBe(false);
  });
});

describe('commentOf', () => {
  it('reads relations at comment and other kinds at meta.comment', () => {
    expect(commentOf({ engine: 'postgres', object: { kind: 'constraint', comment: 'key' } } as unknown as ObjectDetails)).toBe('key');
    expect(commentOf({ engine: 'postgres', object: { kind: 'cast', meta: { owner: null, comment: null, extension: null, system: false } } } as unknown as ObjectDetails)).toBeNull();
    expect(commentOf({ engine: 'postgres', object: { kind: 'aggregate', meta: { comment: 'sum' } } } as unknown as ObjectDetails)).toBe('sum');
  });

  it('is undefined when the role cannot read the comment, even with a null comment', () => {
    expect(commentOf({ engine: 'postgres', object: { kind: 'subscription', commentVisible: false, meta: { comment: null } } } as unknown as ObjectDetails)).toBeUndefined();
    expect(commentOf({ engine: 'postgres', object: { kind: 'subscription', commentVisible: true, meta: { comment: 'feeds' } } } as unknown as ObjectDetails)).toBe('feeds');
  });

  it('is undefined when the details do not carry a comment', () => {
    expect(commentOf({ engine: 'postgres', object: { kind: 'operator' } } as unknown as ObjectDetails)).toBeUndefined();
    expect(commentOf({ engine: 'postgres', object: { kind: 'operator', meta: {} } } as unknown as ObjectDetails)).toBeUndefined();
  });
});

describe('the comment change', () => {
  it('sets exactly as typed and removes when empty', () => {
    expect(buildObjectComment('postgres', constraint, "  it's the key \n")).toEqual({ engine: 'postgres', change: { object: 'comment', target: constraint, comment: "  it's the key \n" } });
    expect(buildObjectComment('postgres', constraint, '   ')).toEqual({ engine: 'postgres', change: { object: 'comment', target: constraint, comment: null } });
    expect(buildObjectComment('mysql', constraint, 'x')).toBeNull();
    expect(normalizeComment('')).toBeNull();
  });

  it('has no MySQL or MariaDB change (routines and events change their comment in their editors)', () => {
    const proc: ObjectRef = { kind: 'procedure', database: 'shop', schema: null, parent: null, name: 'p', signature: null };
    expect(buildObjectComment('mysql', proc, 'audit')).toBeNull();
    expect(buildObjectComment('mariadb', { ...proc, kind: 'event' }, 'x')).toBeNull();
  });

  it('is a change only when the stored text differs', () => {
    expect(commentChanged('key ', 'key')).toBe(true);
    expect(commentChanged('key', 'key')).toBe(false);
    expect(commentChanged('  ', null)).toBe(false);
    // The textarea gives CRLF back as LF: not a change until edited (CM-R4).
    expect(commentChanged('a\nb', 'a\r\nb')).toBe(false);
    expect(commentChanged('a\nbc', 'a\r\nb')).toBe(true);
    expect(commentChanged('', null)).toBe(false);
    expect(commentChanged('x', null)).toBe(true);
    expect(commentChanged('', 'x')).toBe(true);
  });
});
