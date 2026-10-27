import { flushPromises, mount } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { CommandHelp } from '../../api/redis';
import RedisCommandHelp from './RedisCommandHelp.vue';

function help(partial: Partial<CommandHelp>): CommandHelp {
  return {
    name: 'SET',
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
    ...partial,
  };
}

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisCommandHelp: vi.fn(async (sessionId: string, name: string) =>
    sessionId === 'old'
      ? help({ name, fromDocs: false, arity: -3, flags: ['write', 'denyoom'], note: 'This server has no COMMAND DOCS (it needs Redis 7.0 or newer, or Valkey)' })
      : help({ name, summary: 'Sets the string value of a key', syntax: 'SET key value [NX | XX]', group: 'string', complexity: 'O(1)' }),
  ),
}));

describe('RedisCommandHelp', () => {
  it('shows COMMAND DOCS text', async () => {
    const wrapper = mount(RedisCommandHelp, { props: { sessionId: 'new', command: 'SET' } });
    await flushPromises();
    expect(wrapper.text()).toContain('SET key value [NX | XX]');
    expect(wrapper.text()).toContain('Sets the string value of a key');
    expect(wrapper.text()).toContain('O(1)');
  });

  it('falls back to COMMAND INFO with the note on older servers', async () => {
    const wrapper = mount(RedisCommandHelp, { props: { sessionId: 'old', command: 'SET' } });
    await flushPromises();
    expect(wrapper.text()).toContain('at least 3 words');
    expect(wrapper.text()).toContain('write, denyoom');
    expect(wrapper.text()).toContain('needs Redis 7.0');
  });

  it('asks for a command when the line is empty', async () => {
    const wrapper = mount(RedisCommandHelp, { props: { sessionId: 'new', command: null } });
    await flushPromises();
    expect(wrapper.text()).toContain('press F1');
    await wrapper.find('button[aria-label="Close help"]').trigger('click');
    expect(wrapper.emitted('close')).toHaveLength(1);
  });
});
