import { lastSelectedDb, NO_MULTI, redisTokens, runEffect, selectedDbAfterRun, selectEffect } from './redisMode';

describe('Redis console highlighting', () => {
  it('marks the command, strings and numbers', () => {
    expect(redisTokens('SET greeting "hello \\"you\\"" EX 60')).toEqual([
      { text: 'SET', type: 'keyword' },
      { text: 'greeting', type: null },
      { text: '"hello \\"you\\""', type: 'string' },
      { text: 'EX', type: null },
      { text: '60', type: 'number' },
    ]);
  });

  it('marks subcommands of container commands and lower-case names', () => {
    expect(redisTokens('client list')).toEqual([
      { text: 'client', type: 'keyword' },
      { text: 'list', type: 'keyword' },
    ]);
  });

  it('marks comment lines and unknown commands', () => {
    expect(redisTokens('# warm the cache')).toEqual([{ text: '# warm the cache', type: 'comment' }]);
    expect(redisTokens('MYMODULE.DO 1')).toEqual([
      { text: 'MYMODULE.DO', type: 'variableName' },
      { text: '1', type: 'number' },
    ]);
    expect(redisTokens("ZADD z -inf 'a b'")).toEqual([
      { text: 'ZADD', type: 'keyword' },
      { text: 'z', type: null },
      { text: '-inf', type: 'number' },
      { text: "'a b'", type: 'string' },
    ]);
  });
});

describe('lastSelectedDb', () => {
  it('takes the last SELECT of the script', () => {
    expect(lastSelectedDb('GET a\nselect 2\nSET b 1\nSELECT "5"')).toBe(5);
    expect(lastSelectedDb('GET a')).toBeNull();
  });

  it('ignores comments, quoted arguments of other commands and malformed SELECTs', () => {
    expect(lastSelectedDb('# SELECT 3\nECHO "SELECT 4"\nSELECT x\nSELECT 1 2')).toBeNull();
    expect(lastSelectedDb('  SELECT 7  ')).toBe(7);
  });

  it('counts a SELECT queued in MULTI only when EXEC runs the block', () => {
    expect(lastSelectedDb('SELECT 1\nMULTI\nSELECT 3\nDISCARD')).toBe(1);
    expect(lastSelectedDb('MULTI\nSELECT 3\ndiscard')).toBeNull();
    expect(lastSelectedDb('SELECT 1\nMULTI\nSELECT 3\nSET a 1\nEXEC')).toBe(3);
    // The block is still open when the script ends: the SELECT is only queued.
    expect(lastSelectedDb('SELECT 2\nMULTI\nSELECT 5')).toBe(2);
    // A nested MULTI is refused and keeps the queued SELECT.
    expect(lastSelectedDb('MULTI\nSELECT 4\nMULTI\nEXEC')).toBe(4);
    // EXEC without MULTI changes nothing; SELECT after the block counts as usual.
    expect(lastSelectedDb('SELECT 6\nEXEC\nMULTI\nDISCARD\nSELECT 8')).toBe(8);
  });
});

describe('selectedDbAfterRun', () => {
  const script = 'SELECT 3\nGET a\nSELECT 9';
  it('counts every SELECT of a successful run', () => {
    expect(selectedDbAfterRun(script, null)).toBe(9);
  });

  it('counts SELECTs before the command that failed while executing', () => {
    // The failing command is GET a, which starts at character 10.
    expect(selectedDbAfterRun(script, { detail: 'Line 2', position: 10 })).toBe(3);
    expect(selectedDbAfterRun(script, { detail: 'Line 1', position: 1 })).toBeNull();
  });

  it('ignores parse errors and failures before anything ran', () => {
    expect(selectedDbAfterRun(script, { detail: null, position: 12 })).toBeNull();
    expect(selectedDbAfterRun(script, { detail: null, position: null })).toBeNull();
  });
});

describe('a MULTI block across console runs', () => {
  it('carries the open block and its queued SELECT to the next run', () => {
    // Run 1 opens the block; run 2 queues SELECT 3 and discards: the session stays where it was.
    const first = selectEffect('MULTI');
    expect(first).toEqual({ db: null, multi: { open: true, queuedDb: null } });
    expect(lastSelectedDb('SELECT 3\nDISCARD', first.multi)).toBeNull();
    expect(selectEffect('SELECT 3\nDISCARD', first.multi).multi).toEqual(NO_MULTI);
    // Without the carried state the same script would wrongly count the SELECT.
    expect(lastSelectedDb('SELECT 3\nDISCARD')).toBe(3);

    // The SELECT queued in run 2 switches the database when run 3 sends EXEC.
    const second = selectEffect('SELECT 3', first.multi);
    expect(second).toEqual({ db: null, multi: { open: true, queuedDb: 3 } });
    expect(selectEffect('SET a 1\nEXEC', second.multi)).toEqual({ db: 3, multi: NO_MULTI });
    // A SELECT before MULTI counts at once; the block opened after it stays open.
    expect(selectEffect('SELECT 2\nMULTI\nSELECT 5')).toEqual({ db: 2, multi: { open: true, queuedDb: 5 } });
  });

  it('follows a failed run up to the failing command', () => {
    const open = { open: true, queuedDb: 4 };
    // Nothing ran (a parse error): the block is as it was.
    expect(runEffect('EXEC', { detail: null, position: 1 }, open)).toEqual({ db: null, multi: open });
    // EXEC itself failed: the server dropped the block, the queued SELECT never ran.
    expect(runEffect('EXEC', { detail: 'Line 1', position: 1 }, open)).toEqual({ db: null, multi: NO_MULTI });
    // The command after EXEC failed: EXEC ran, so the queued SELECT counts.
    expect(runEffect('EXEC\nGET', { detail: 'Line 2', position: 6 }, open)).toEqual({ db: 4, multi: NO_MULTI });
    expect(selectedDbAfterRun('EXEC\nGET', { detail: 'Line 2', position: 6 }, open)).toBe(4);
    // A command queued in the block failed: the block stays open.
    expect(runEffect('MULTI\nSELECT 7\nBOGUS', { detail: 'Line 3', position: 16 })).toEqual({ db: null, multi: { open: true, queuedDb: 7 } });
  });
});
