import type { KeyEdit } from '../api/redis';
import {
  commandStatements,
  hexBytes,
  wholeStringPolicy,
  createdKeyType,
  digestNote,
  describeEdit,
  EditDraft,
  editTarget,
  isStreamId,
  jsonProblem,
  minifyJson,
  outcomeView,
  parseBound,
  parseScore,
  targetOf,
  textValue,
  ttlMsFromSeconds,
} from './redisEdit';
import { keyRenameSchema, keyTtlSchema, newKeyEdits, newKeySchema } from './redisForms';
import { entryForKey, keyDisplayFromHex, keyHexFromDisplay } from './redisKeys';

describe('scores and bounds', () => {
  it('parses scores as Redis takes them', () => {
    expect(parseScore('42')).toEqual({ t: 'int', v: 42 });
    expect(parseScore(' -1.5e3 ')).toEqual({ t: 'float', v: -1500 });
    expect(parseScore('+inf')).toEqual({ t: 'other', v: 'inf' });
    expect(parseScore('-INF')).toEqual({ t: 'other', v: '-inf' });
    expect(parseScore('nan')).toBeNull();
    expect(parseScore('12abc')).toBeNull();
    expect(parseScore('99999999999999999999')).toEqual({ t: 'float', v: 1e20 });
  });

  it('reads range ends with ( for exclusive and open ends', () => {
    expect(parseBound('-inf', 'score')).toEqual({ kind: 'unbounded' });
    expect(parseBound('', 'score')).toEqual({ kind: 'unbounded' });
    expect(parseBound('(10', 'score')).toEqual({ kind: 'exclusive', value: { t: 'int', v: 10 } });
    expect(parseBound('2.5', 'score')).toEqual({ kind: 'inclusive', value: { t: 'float', v: 2.5 } });
    expect(parseBound('(x', 'score')).toBeNull();
    expect(parseBound('+', 'lex')).toEqual({ kind: 'unbounded' });
    expect(parseBound('[a', 'lex')).toEqual({ kind: 'inclusive', value: textValue('a') });
    expect(parseBound('(b c', 'lex')).toEqual({ kind: 'exclusive', value: textValue('b c') });
  });

  it('checks stream ids and TTLs', () => {
    expect(isStreamId('1792333914012-0')).toBe(true);
    expect(isStreamId('1792333914012')).toBe(true);
    expect(isStreamId('abc-1')).toBe(false);
    expect(ttlMsFromSeconds('60')).toBe('60000');
    expect(ttlMsFromSeconds('0')).toBeNull();
    expect(ttlMsFromSeconds('1.5')).toBeNull();
  });
});

describe('JSON helpers', () => {
  it('minifies without touching strings or big numbers', () => {
    expect(minifyJson('{\n  "a b": "x  y",\n  "n": 12345678901234567890\n}')).toBe('{"a b":"x  y","n":12345678901234567890}');
    expect(minifyJson('["a\\" b", 1]')).toBe('["a\\" b",1]');
    expect(jsonProblem('{"a":1}')).toBeNull();
    expect(jsonProblem('{a:1}')).not.toBeNull();
  });
});

describe('EditDraft', () => {
  it('keeps one edit per element and lets pushes and appends pile up', () => {
    const draft = new EditDraft();
    const field = textValue('name');
    draft.stage({ op: 'hashSet', field, value: textValue('a'), original: textValue('x') });
    draft.stage({ op: 'hashSet', field, value: textValue('b'), original: textValue('x') });
    draft.stage({ op: 'listPush', end: 'tail', values: [textValue('1')] });
    draft.stage({ op: 'listPush', end: 'tail', values: [textValue('2')] });
    expect(draft.size).toBe(3);
    expect(draft.pending(targetOf.field(field))).toMatchObject({ value: { v: 'b' } });
    draft.stage({ op: 'hashDelete', field, original: textValue('x') });
    expect(draft.pending(targetOf.field(field))?.op).toBe('hashDelete');
    draft.unstage(targetOf.field(field));
    expect(draft.size).toBe(2);
    draft.removeAt(0);
    expect(draft.edits).toEqual([{ op: 'listPush', end: 'tail', values: [textValue('2')] }]);
  });

  it('refuses empty and oversized drafts', () => {
    const draft = new EditDraft();
    expect(draft.problem()).toBe('Nothing to save.');
    draft.stage({ op: 'stringSet', value: textValue('x'.repeat(9 * 1024 * 1024)), original: textValue('') });
    expect(draft.problem()).toContain('8 MiB');
  });

  it('does not count originals: a big string read whole can be edited', () => {
    const draft = new EditDraft();
    draft.stage({ op: 'stringSet', value: textValue('short'), original: textValue('x'.repeat(20 * 1024 * 1024)) });
    expect(draft.problem()).toBeNull();
  });

  it('names targets so one member, index or id is edited once', () => {
    expect(editTarget({ op: 'listSet', index: '3', value: textValue('a'), original: textValue('b') })).toBe(targetOf.index('3'));
    expect(editTarget({ op: 'zsetRemove', member: textValue('m'), original: { t: 'int', v: 1 } })).toBe(targetOf.member(textValue('m')));
    expect(editTarget({ op: 'setRemove', member: textValue('m') })).toBe(targetOf.member(textValue('m')));
    expect(editTarget({ op: 'streamAdd', id: null, fields: [] })).toBeNull();
  });
});

describe('outcomes', () => {
  const edits: KeyEdit[] = [
    { op: 'hashSet', field: textValue('a'), value: textValue('1'), original: textValue('0') },
    { op: 'hashDelete', field: textValue('b'), original: textValue('2') },
  ];

  it('names the edit whose value changed', () => {
    const view = outcomeView({ status: 'conflict', edit: 1, reason: 'valueChanged', message: 'Field b changed.' }, edits);
    expect(view.tone).toBe('warning');
    expect(view.message).toContain('Delete field b');
    expect(view.message).toContain('Nothing was changed');
    expect(view.reread).toBe(true);
  });

  it('says a failed command left the others applied', () => {
    const view = outcomeView(
      { status: 'failed', command: 0, error: { kind: 'query', message: 'OOM', code: 'OOM', detail: null, hint: null, position: null } },
      edits,
    );
    expect(view.title).toBe('Command 1 failed');
    expect(view.message).toContain('does not roll back');
  });

  it('counts applied changes', () => {
    expect(outcomeView({ status: 'applied', replies: [] }, edits).message).toBe('2 changes applied.');
    expect(outcomeView({ status: 'applied', replies: [] }, []).message).toBe('The change was applied.');
    expect(describeEdit({ op: 'expire', ttlMs: '60000' })).toBe('Expire in 60 s');
    expect(commandStatements(['XGROUP', 'HDEL', 'The digest is missing: a change keeping the length would be lost.'])).toEqual(['XGROUP', 'HDEL']);
    expect(digestNote('tooLarge')).toContain('too large');
    expect(digestNote('notPermitted')).toContain('ACL');
    expect(digestNote(null)).toBeNull();
    expect(digestNote('deferred')).toBeNull();
  });
});

describe('forms', () => {
  it('validates rename and TTL', () => {
    expect(keyRenameSchema('a').safeParse({ newKey: 'a', replace: false }).success).toBe(false);
    expect(keyRenameSchema('a').safeParse({ newKey: 'b', replace: false }).success).toBe(true);
    expect(keyTtlSchema.safeParse({ mode: 'expire', seconds: 'x' }).success).toBe(false);
    expect(keyTtlSchema.safeParse({ mode: 'persist', seconds: '' }).success).toBe(true);
  });

  it('builds the create edits of a new key, with a TTL', () => {
    const values = { key: 'rate:board', type: 'zset' as const, value: 'alice', second: '10', field: '', ttlSeconds: '30' };
    expect(newKeySchema.safeParse(values).success).toBe(true);
    const edits = newKeyEdits(values);
    expect(edits).toEqual([
      { op: 'zsetAdd', member: textValue('alice'), score: { t: 'int', v: 10 } },
      { op: 'expire', ttlMs: '30000' },
    ]);
    expect(createdKeyType(edits)).toBe('zset');
    expect(newKeySchema.safeParse({ ...values, type: 'json', value: '{bad' }).success).toBe(false);
    expect(newKeySchema.safeParse({ ...values, type: 'hash', field: '' }).success).toBe(false);
  });
});

describe('key display text', () => {
  it('round-trips the backend escapes', () => {
    expect(keyHexFromDisplay('a:b')).toBe('613a62');
    expect(keyHexFromDisplay('a\\\\b')).toBe('615c62');
    expect(keyHexFromDisplay('\\xff\\x00')).toBe('ff00');
    expect(keyHexFromDisplay('bad\\q')).toBeNull();
    expect(keyDisplayFromHex('ff00')).toBe('\\xff\\x00');
    expect(keyDisplayFromHex('615c62')).toBe('a\\\\b');
    expect(keyDisplayFromHex('e282ac')).toBe('€');
    expect(keyDisplayFromHex('e2826141')).toBe('\\xe2\\x82aA');
    expect(entryForKey('user:1', 'hash', null)).toEqual({ name: '757365723a31', display: 'user:1', keyType: 'hash', ttlMs: null, placeholder: false });
  });
});

describe('whole string policy', () => {
  it('loads up to 16 MiB, asks up to 64 MiB, refuses above and asks when the size is unknown', () => {
    const MiB = 1024 * 1024;
    expect(wholeStringPolicy(String(16 * MiB))).toBe('load');
    expect(wholeStringPolicy(String(16 * MiB + 1))).toBe('confirm');
    expect(wholeStringPolicy(String(64 * MiB))).toBe('confirm');
    expect(wholeStringPolicy(String(64 * MiB + 1))).toBe('refuse');
    expect(wholeStringPolicy(null)).toBe('confirm');
  });
});

describe('hexBytes', () => {
  it('turns lowercase hex into raw bytes', () => {
    expect([...hexBytes('ff00e282ac')]).toEqual([0xff, 0x00, 0xe2, 0x82, 0xac]);
    expect([...hexBytes('')]).toEqual([]);
  });
});
