import { lastSelectedDb, redisTokens, selectedDbAfterRun } from './redisMode';

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
