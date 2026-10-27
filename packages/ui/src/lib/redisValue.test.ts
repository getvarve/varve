import { chunkEnd, isDeadCursor, joinStringChunks } from './redisValue';

describe('joinStringChunks', () => {
  it('joins text chunks as text', () => {
    expect(joinStringChunks({ t: 'text', v: 'ab' }, { t: 'text', v: 'cd' })).toEqual({ t: 'text', v: 'abcd' });
    expect(joinStringChunks(null, { t: 'text', v: 'x' })).toEqual({ t: 'text', v: 'x' });
  });

  it('switches the whole value to bytes once a chunk is bytes', () => {
    expect(joinStringChunks({ t: 'text', v: 'é' }, { t: 'bytes', v: 'ff' })).toEqual({ t: 'bytes', v: 'c3a9ff' });
    expect(joinStringChunks({ t: 'bytes', v: '00' }, { t: 'text', v: 'A' })).toEqual({ t: 'bytes', v: '0041' });
  });
});

describe('chunkEnd', () => {
  it('counts bytes, not characters', () => {
    expect(chunkEnd('0', { t: 'text', v: 'é😀' })).toBe(6n);
    expect(chunkEnd('524288', { t: 'bytes', v: '00ff' })).toBe(524290n);
  });
});

describe('isDeadCursor', () => {
  const driver = (code: string | null, message: string) => ({
    kind: 'session' as const,
    error: { kind: 'driver' as const, error: { kind: 'query' as const, message, code, detail: null, hint: null, position: null } },
  });
  it('recognizes expired and unknown page cursors only', () => {
    expect(isDeadCursor(driver('CURSOR_EXPIRED', 'The page cursor expired'))).toBe(true);
    expect(isDeadCursor(driver(null, 'Invalid page cursor "h:3"'))).toBe(true);
    expect(isDeadCursor(driver('WRONGTYPE', 'Operation against a key holding the wrong kind of value'))).toBe(false);
    expect(isDeadCursor({ kind: 'internal', message: 'Invalid page cursor' })).toBe(false);
  });
});
