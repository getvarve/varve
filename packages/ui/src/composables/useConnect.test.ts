import type * as RecordsModule from '../api/records';
import { CommandFailure } from '../api/errors';
import type { ConnectionProfile } from '../api/records';
import { forgetTypedSecrets, rememberTypedSecrets, retainTypedSecretsAfterEdit, typedSecretsOf, withConnectPrompts } from './useConnect';
import { useDialogs } from './useDialogs';
import { useToasts } from './useToasts';

const saveSecret = vi.fn();
const trustSshHostKey = vi.fn(async (..._args: unknown[]) => undefined);
vi.mock('../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  saveSecret: (...args: unknown[]) => saveSecret(...args),
  trustSshHostKey: (...args: unknown[]) => trustSshHostKey(...args),
  fetchTree: vi.fn(async () => ({ folders: [], connections: [], queries: [] })),
}));

const profile: ConnectionProfile = {
  id: 'p1',
  workspaceId: 'personal',
  folderId: null,
  name: 'Orders',
  color: null,
  sortOrder: 0,
  engine: 'postgres',
  host: 'db',
  port: 5432,
  database: null,
  username: 'kim',
  environment: 'production',
  tls: { mode: 'prefer', caPath: null, clientCertPath: null, clientKeyPath: null },
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { readOnly: false, confirmDestructive: true, statementTimeoutMs: null, keepaliveS: null, initSql: null, showSystemObjects: false, redisDb: 0 },
  savedSecrets: [],
  createdAt: 0,
  updatedAt: 0,
  version: 1,
};

const authFailure = new CommandFailure({
  kind: 'session',
  error: { kind: 'driver', error: { kind: 'auth', message: 'password authentication failed', code: '28P01', detail: null, hint: null, position: null } },
});

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

/** Answers the next secret prompt like a user would. */
async function answerPrompt(value: string, save: boolean): Promise<void> {
  const dialogs = useDialogs();
  for (let i = 0; i < 20 && dialogs.dialogs.length === 0; i += 1) await tick();
  const open = dialogs.dialogs[0];
  if (!open) throw new Error('no prompt opened');
  dialogs.close(open.id, { value, save });
}

describe('withConnectPrompts', () => {
  beforeEach(() => {
    saveSecret.mockReset();
  });

  it('keeps the open session when the keychain save fails, and reports it separately', async () => {
    saveSecret.mockRejectedValue(new CommandFailure({ kind: 'keychain', message: 'keychain is locked' }));
    const run = vi.fn(async (secrets: { password?: string }) => {
      if (secrets.password !== 'right') throw authFailure;
      return 'session-1';
    });
    const pending = withConnectPrompts(profile, run);
    await answerPrompt('right', true);
    await expect(pending).resolves.toBe('session-1');
    expect(saveSecret).toHaveBeenCalledWith('p1', 'db_password', 'right', 1);
    const toast = useToasts().toasts.at(-1);
    expect(toast?.tone).toBe('danger');
    expect(toast?.title).toMatch(/could not be saved in the keychain/);
    expect(`${toast?.title} ${toast?.message}`).not.toContain('right');
    // Kept in memory for this run instead.
    expect(typedSecretsOf('p1').password).toBe('right');
  });

  it('saves the secret after the connection works', async () => {
    saveSecret.mockResolvedValue(profile);
    const run = vi.fn(async (secrets: { password?: string }) => {
      if (secrets.password !== 'pw2') throw authFailure;
      return 'session-2';
    });
    const pending = withConnectPrompts({ ...profile, id: 'p2' }, run);
    await answerPrompt('pw2', true);
    await expect(pending).resolves.toBe('session-2');
    expect(saveSecret).toHaveBeenCalledWith('p2', 'db_password', 'pw2', 1);
    expect(typedSecretsOf('p2').password).toBeUndefined();
  });

  it('lets the prompt answer replace a wrong password typed in the form, and applies its save choice', async () => {
    saveSecret.mockResolvedValue(profile);
    const seen: (string | undefined)[] = [];
    const run = vi.fn(async (secrets: { password?: string }) => {
      seen.push(secrets.password);
      if (secrets.password !== 'right') throw authFailure;
      return 'session-4';
    });
    const answered = vi.fn();
    const pending = withConnectPrompts({ ...profile, id: 'p4' }, run, { initial: { password: 'typo' }, onAnswered: answered });
    await answerPrompt('right', true);
    await expect(pending).resolves.toBe('session-4');
    expect(seen).toEqual(['typo', 'right']);
    expect(saveSecret).toHaveBeenCalledWith('p4', 'db_password', 'right', 1);
    expect(answered).toHaveBeenCalledWith('db_password', { value: 'right', save: true });
  });

  it('keeps nothing in memory from an editor test', async () => {
    const run = vi.fn(async () => 'ok');
    await expect(withConnectPrompts({ ...profile, id: 'p5' }, run, { initial: { password: 'typed' }, remember: false })).resolves.toBe('ok');
    expect(run).toHaveBeenCalledWith({ password: 'typed' });
    expect(typedSecretsOf('p5')).toEqual({});
  });

  it('drops typed secrets when the saved profile points at another server', () => {
    rememberTypedSecrets('p6', { password: 'pw' });
    const before = { ...profile, id: 'p6' };
    retainTypedSecretsAfterEdit(before, { ...before, host: 'db' });
    expect(typedSecretsOf('p6')).toEqual({ password: 'pw' });
    retainTypedSecretsAfterEdit(before, { ...before, host: 'elsewhere' });
    expect(typedSecretsOf('p6')).toEqual({});
  });

  it('writes nothing back when the connection moved to another server while connecting', async () => {
    saveSecret.mockResolvedValue(profile);
    const before = { ...profile, id: 'p7' };
    let finish: (value: string) => void = () => undefined;
    const run = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          finish = resolve;
        }),
    );
    const pending = withConnectPrompts(before, run, { initial: { password: 'old-server-pw' } });
    retainTypedSecretsAfterEdit(before, { ...before, host: 'new-server' });
    finish('session');
    await expect(pending).resolves.toBe('session');
    expect(typedSecretsOf('p7')).toEqual({});
    expect(saveSecret).not.toHaveBeenCalled();
  });

  it('writes nothing back for a connection deleted while connecting', async () => {
    let finish: (value: string) => void = () => undefined;
    const pending = withConnectPrompts({ ...profile, id: 'p8' }, () => new Promise<string>((resolve) => (finish = resolve)), {
      initial: { password: 'pw' },
    });
    forgetTypedSecrets('p8');
    finish('session');
    await pending;
    expect(typedSecretsOf('p8')).toEqual({});
  });

  it('trusts a host key for the host and port in the error, not the profile', async () => {
    const tunneled = { ...profile, id: 'p9', ssh: { ...profile.ssh, enabled: true, host: 'bastion', port: 2222, username: 'ops' } };
    const run = vi
      .fn()
      .mockRejectedValueOnce(
        new CommandFailure({
          kind: 'tunnel',
          message: 'unknown',
          code: 'host_key_unknown',
          fingerprints: ['SHA256:x'],
          presentedKey: 'ssh-ed25519 AAAA',
          host: 'bastion-2',
          port: 2223,
        }),
      )
      .mockResolvedValueOnce('session');
    const pending = withConnectPrompts(tunneled, run);
    const dialogs = useDialogs();
    for (let i = 0; i < 20 && dialogs.dialogs.length === 0; i += 1) await tick();
    const ask = dialogs.dialogs[0];
    if (ask) dialogs.close(ask.id, true);
    await expect(pending).resolves.toBe('session');
    expect(trustSshHostKey).toHaveBeenCalledWith('bastion-2', 2223, 'ssh-ed25519 AAAA');
  });

  it('never retries with answers given before the connection moved to another server', async () => {
    const before = { ...profile, id: 'p10' };
    const run = vi.fn(async () => {
      throw authFailure;
    });
    const pending = withConnectPrompts(before, run);
    const dialogs = useDialogs();
    for (let i = 0; i < 20 && dialogs.dialogs.length === 0; i += 1) await tick();
    retainTypedSecretsAfterEdit(before, { ...before, host: 'elsewhere' });
    const open = dialogs.dialogs[0];
    if (open) dialogs.close(open.id, { value: 'pw', save: true });
    await expect(pending).rejects.toMatchObject({ cmd: { kind: 'internal', message: expect.stringMatching(/settings changed/) } });
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('resolves null when the prompt is cancelled', async () => {
    const pending = withConnectPrompts({ ...profile, id: 'p3' }, async () => {
      throw authFailure;
    });
    const dialogs = useDialogs();
    for (let i = 0; i < 20 && dialogs.dialogs.length === 0; i += 1) await tick();
    const open = dialogs.dialogs[0];
    if (open) dialogs.close(open.id, null);
    await expect(pending).resolves.toBeNull();
  });

  it('asks for the TLS key passphrase, again when it is wrong, and saves the one that works', async () => {
    saveSecret.mockResolvedValue(profile);
    const keyed: ConnectionProfile = {
      ...profile,
      id: 'p20',
      tls: { mode: 'require', caPath: null, clientCertPath: '/c.pem', clientKeyPath: '/k.pem' },
    };
    const wrong = new CommandFailure({
      kind: 'session',
      error: {
        kind: 'driver',
        error: {
          kind: 'tls',
          message: 'the passphrase does not decrypt the client key file',
          code: 'tls_key_passphrase_wrong',
          detail: null,
          hint: null,
          position: null,
        },
      },
    });
    const run = vi.fn(async (secrets: { tlsPassphrase?: string }) => {
      if (secrets.tlsPassphrase === undefined) throw new CommandFailure({ kind: 'secretRequired', secret: 'tls_key_passphrase' });
      if (secrets.tlsPassphrase !== 'k3y-pass') throw wrong;
      return 'session-tls';
    });
    const pending = withConnectPrompts(keyed, run);
    await answerPrompt('typo', true);
    const dialogs = useDialogs();
    for (let i = 0; i < 20 && dialogs.dialogs.length === 0; i += 1) await tick();
    // The second prompt says why, without echoing the passphrase.
    const second = dialogs.dialogs[0];
    expect(JSON.stringify(second)).toContain('does not decrypt');
    expect(JSON.stringify(second)).not.toContain('typo');
    await answerPrompt('k3y-pass', true);
    await expect(pending).resolves.toBe('session-tls');
    expect(run).toHaveBeenCalledTimes(3);
    expect(saveSecret).toHaveBeenCalledTimes(1);
    expect(saveSecret).toHaveBeenCalledWith('p20', 'tls_key_passphrase', 'k3y-pass', 1);
    expect(typedSecretsOf('p20').tlsPassphrase).toBeUndefined();
  });

  it('keeps an unsaved TLS key passphrase in memory only for the same key file', async () => {
    const keyed: ConnectionProfile = {
      ...profile,
      id: 'p21',
      tls: { mode: 'require', caPath: null, clientCertPath: '/c.pem', clientKeyPath: '/k.pem' },
    };
    const run = vi.fn(async (secrets: { tlsPassphrase?: string }) => {
      if (secrets.tlsPassphrase === undefined) throw new CommandFailure({ kind: 'secretRequired', secret: 'tls_key_passphrase' });
      return 'session';
    });
    const pending = withConnectPrompts(keyed, run);
    await answerPrompt('kept', false);
    await expect(pending).resolves.toBe('session');
    expect(saveSecret).not.toHaveBeenCalled();
    expect(typedSecretsOf('p21').tlsPassphrase).toBe('kept');
    // Another server, same key file: still usable.
    retainTypedSecretsAfterEdit(keyed, { ...keyed, host: 'elsewhere' });
    expect(typedSecretsOf('p21').tlsPassphrase).toBe('kept');
    retainTypedSecretsAfterEdit(keyed, { ...keyed, tls: { ...keyed.tls, clientKeyPath: '/other.pem' } });
    expect(typedSecretsOf('p21').tlsPassphrase).toBeUndefined();
  });
});
