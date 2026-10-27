import type * as RedisModule from '../api/redis';
import type { CommandHelp } from '../api/redis';
import { arityText, commandHelp, commandOfLine, forgetCommandHelp } from './redisHelp';

const lookups: string[] = [];
let fail = false;

vi.mock('../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisCommandHelp: vi.fn(async (_session: string, name: string): Promise<CommandHelp> => {
    lookups.push(name);
    if (fail) throw { kind: 'internal', message: 'lost' };
    return {
      name,
      known: true,
      fromDocs: true,
      summary: null,
      syntax: null,
      since: null,
      group: null,
      complexity: null,
      deprecatedSince: null,
      replacedBy: null,
      subcommands: [],
      arity: null,
      flags: [],
      note: null,
    };
  }),
}));

describe('commandOfLine', () => {
  it('takes the command and a subcommand-like second word', () => {
    expect(commandOfLine('  config get maxmemory')).toBe('CONFIG GET');
    expect(commandOfLine('get user:1')).toBe('GET');
    // The backend answers with GET's help when MYKEY is no subcommand.
    expect(commandOfLine('get mykey')).toBe('GET MYKEY');
    expect(commandOfLine('SET "a b" c')).toBe('SET');
    expect(commandOfLine('json.get doc $')).toBe('JSON.GET DOC');
    expect(commandOfLine('')).toBeNull();
    expect(commandOfLine('# a comment')).toBeNull();
    expect(commandOfLine('"quoted"')).toBeNull();
  });
});

describe('commandHelp', () => {
  beforeEach(() => {
    lookups.length = 0;
    fail = false;
    forgetCommandHelp('s1');
  });

  it('asks the server once per session and command', async () => {
    await commandHelp('s1', 'get');
    await commandHelp('s1', 'GET');
    await commandHelp('s2', 'GET');
    expect(lookups).toEqual(['GET', 'GET']);
  });

  it('does not keep a failed lookup', async () => {
    fail = true;
    await expect(commandHelp('s1', 'SET')).rejects.toMatchObject({ kind: 'internal' });
    fail = false;
    await commandHelp('s1', 'SET');
    expect(lookups).toEqual(['SET', 'SET']);
  });

  it('words the arity', () => {
    expect(arityText(-3)).toBe('at least 3 words, the command included');
    expect(arityText(2)).toBe('exactly 2 words, the command included');
    expect(arityText(null)).toBeNull();
  });
});
