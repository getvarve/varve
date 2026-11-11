import { openDatabaseSession, openSession, testConnection } from './sessions';

const invoke = vi.fn(async (_command: string, _args?: Record<string, unknown>): Promise<unknown> => ({}));

vi.mock('@tauri-apps/api/core', async (original) => ({
  ...(await original<object>()),
  invoke: (command: string, args?: Record<string, unknown>) => invoke(command, args),
}));

beforeEach(() => invoke.mockClear());

describe('connect commands', () => {
  it('send, for each typed secret, the profile version it was entered for', async () => {
    const secrets = { password: 'pw', sshSecret: 'ssh', tlsPassphrase: 'tls', typedFor: { password: 1, sshSecret: 2, tlsPassphrase: 3 } };
    await testConnection({ kind: 'saved', id: 'c1' }, secrets);
    await openSession('c1', secrets);
    await openDatabaseSession('c1', 'app', secrets);
    const sent = { password: 'pw', sshSecret: 'ssh', tlsPassphrase: 'tls', typedFor: { password: 1, sshSecret: 2, tlsPassphrase: 3 } };
    expect(invoke.mock.calls).toEqual([
      ['test_connection', { target: { kind: 'saved', id: 'c1' }, ...sent }],
      ['open_session', { id: 'c1', ...sent }],
      ['open_database_session', { id: 'c1', database: 'app', ...sent }],
    ]);
  });

  it('send a version only for the secrets that are passed', async () => {
    await openSession('c1', { sshSecret: 'ssh', typedFor: { sshSecret: 2 } });
    expect(invoke.mock.calls).toEqual([['open_session', { id: 'c1', password: null, sshSecret: 'ssh', tlsPassphrase: null, typedFor: { sshSecret: 2 } }]]);
  });

  it('send null for every secret and version left out', async () => {
    await testConnection({ kind: 'saved', id: 'c1' });
    await openSession('c1');
    await openDatabaseSession('c1', 'app');
    const none = { password: null, sshSecret: null, tlsPassphrase: null, typedFor: null };
    expect(invoke.mock.calls).toEqual([
      ['test_connection', { target: { kind: 'saved', id: 'c1' }, ...none }],
      ['open_session', { id: 'c1', ...none }],
      ['open_database_session', { id: 'c1', database: 'app', ...none }],
    ]);
  });
});
