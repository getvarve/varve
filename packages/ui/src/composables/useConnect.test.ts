import type * as RecordsModule from '../api/records';
import { CommandFailure } from '../api/errors';
import type { ConnectionProfile } from '../api/records';
import type { TypedSecrets } from '../api/sessions';
import {
  followExternalEdit,
  forgetAllTypedSecrets,
  forgetTypedSecrets,
  rememberTypedSecrets,
  retainTypedSecretsAfterEdit,
  typedSecretIds,
  typedSecretsOf,
  withConnectPrompts,
} from './useConnect';
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
    expect(run).toHaveBeenCalledWith({ password: 'typed', typedFor: { password: 1 } });
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

  it('sends the version a remembered secret was typed for, and remembers it with that version', async () => {
    const before = { ...profile, id: 'p30', version: 3 };
    const seen: unknown[] = [];
    const run = vi.fn(async (secrets: { password?: string }) => {
      seen.push({ ...secrets });
      if (secrets.password === undefined) throw authFailure;
      return 'session';
    });
    const pending = withConnectPrompts(before, run);
    await answerPrompt('pw', false);
    await expect(pending).resolves.toBe('session');
    expect(seen).toEqual([{}, { password: 'pw', typedFor: { password: 3 } }]);
    // A rename later: the password still belongs to version 3.
    const renamed = { ...before, name: 'Renamed', version: 4 };
    retainTypedSecretsAfterEdit(before, renamed);
    await withConnectPrompts(renamed, run);
    expect(seen.at(-1)).toEqual({ password: 'pw', typedFor: { password: 3 } });
  });

  it('drops a remembered secret the backend refuses as typed for older settings, and asks again for that one only', async () => {
    rememberTypedSecrets('p31', { password: 'old-pw', sshSecret: 'old-ssh' }, 2);
    const moved = { ...profile, id: 'p31', version: 5 };
    const seen: unknown[] = [];
    // The server changed at version 5; the bastion did not.
    const run = vi.fn(async (secrets: TypedSecrets) => {
      seen.push({ ...secrets });
      if (secrets.typedFor?.password !== undefined && secrets.typedFor.password < 5) {
        throw new CommandFailure({ kind: 'secretRequired', secret: 'db_password' });
      }
      if (secrets.password === undefined) throw authFailure;
      return 'session';
    });
    const pending = withConnectPrompts(moved, run);
    await answerPrompt('new-pw', false);
    await expect(pending).resolves.toBe('session');
    expect(seen).toEqual([
      { password: 'old-pw', sshSecret: 'old-ssh', typedFor: { password: 2, sshSecret: 2 } },
      { password: 'new-pw', sshSecret: 'old-ssh', typedFor: { password: 5, sshSecret: 2 } },
    ]);
    expect(typedSecretsOf('p31')).toEqual({ password: 'new-pw', sshSecret: 'old-ssh' });
  });

  it('connects with a password typed before an SSH setting changed and a passphrase typed after it', async () => {
    // Version 1: the password is typed and kept in memory. Version 2: another
    // SSH key file, and its passphrase is typed in the editor form.
    const tunnelled: ConnectionProfile = {
      ...profile,
      id: 'p37',
      ssh: { enabled: true, host: 'bastion', port: 22, username: 'ops', auth: 'key_file', keyPath: '/keys/old' },
    };
    const edited: ConnectionProfile = { ...tunnelled, ssh: { ...tunnelled.ssh, keyPath: '/keys/new' }, version: 2 };
    rememberTypedSecrets('p37', { password: 'pw' }, 1);
    retainTypedSecretsAfterEdit(tunnelled, edited);
    rememberTypedSecrets('p37', { sshSecret: 'passphrase' }, edited.version);
    // The backend: the SSH kinds changed at version 2, the password never.
    const run = vi.fn(async (secrets: TypedSecrets) => {
      if (secrets.sshSecret !== undefined && (secrets.typedFor?.sshSecret ?? 0) < 2) {
        throw new CommandFailure({ kind: 'secretRequired', secret: 'ssh_key_passphrase' });
      }
      return 'session';
    });
    await expect(withConnectPrompts(edited, run)).resolves.toBe('session');
    expect(run).toHaveBeenCalledTimes(1);
    expect(run).toHaveBeenCalledWith({ password: 'pw', sshSecret: 'passphrase', typedFor: { password: 1, sshSecret: 2 } });
    // And again, from another tab: nothing was dropped.
    await expect(withConnectPrompts(edited, run)).resolves.toBe('session');
    expect(run).toHaveBeenLastCalledWith({ password: 'pw', sshSecret: 'passphrase', typedFor: { password: 1, sshSecret: 2 } });
    expect(useDialogs().dialogs).toHaveLength(0);
  });

  it('asks again for each outdated secret in its turn and keeps the one that still belongs', async () => {
    rememberTypedSecrets('p38', { password: 'old-pw', sshSecret: 'old-ssh', tlsPassphrase: 'key-pass' }, 1);
    const moved: ConnectionProfile = {
      ...profile,
      id: 'p38',
      ssh: { enabled: true, host: 'bastion', port: 22, username: 'ops', auth: 'password', keyPath: null },
      version: 3,
    };
    const seen: unknown[] = [];
    // Server and bastion changed at version 3; the TLS key file did not.
    const run = vi.fn(async (secrets: TypedSecrets) => {
      seen.push({ ...secrets });
      if (secrets.password !== undefined && (secrets.typedFor?.password ?? 0) < 3) {
        throw new CommandFailure({ kind: 'secretRequired', secret: 'db_password' });
      }
      if (secrets.sshSecret !== undefined && (secrets.typedFor?.sshSecret ?? 0) < 3) {
        throw new CommandFailure({ kind: 'secretRequired', secret: 'ssh_password' });
      }
      return 'session';
    });
    const pending = withConnectPrompts(moved, run);
    await answerPrompt('new-pw', false);
    await answerPrompt('new-ssh', false);
    await expect(pending).resolves.toBe('session');
    expect(seen).toEqual([
      { password: 'old-pw', sshSecret: 'old-ssh', tlsPassphrase: 'key-pass', typedFor: { password: 1, sshSecret: 1, tlsPassphrase: 1 } },
      { password: 'new-pw', sshSecret: 'old-ssh', tlsPassphrase: 'key-pass', typedFor: { password: 3, sshSecret: 1, tlsPassphrase: 1 } },
      { password: 'new-pw', sshSecret: 'new-ssh', tlsPassphrase: 'key-pass', typedFor: { password: 3, sshSecret: 3, tlsPassphrase: 1 } },
    ]);
    expect(typedSecretsOf('p38')).toEqual({ password: 'new-pw', sshSecret: 'new-ssh', tlsPassphrase: 'key-pass' });
  });

  it('stops when a secret typed for the profile shown is refused: the profile is outdated', async () => {
    const shown = { ...profile, id: 'p32', version: 7 };
    const run = vi.fn(async () => {
      throw new CommandFailure({ kind: 'secretRequired', secret: 'db_password' });
    });
    await expect(withConnectPrompts(shown, run, { initial: { password: 'pw' } })).rejects.toMatchObject({
      cmd: { kind: 'internal', message: expect.stringMatching(/settings changed/) },
    });
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('remembers editor secrets for the version the editor saved', () => {
    const before = { ...profile, id: 'p33', version: 1 };
    const saved = { ...before, host: 'elsewhere', version: 2 };
    retainTypedSecretsAfterEdit(before, saved);
    rememberTypedSecrets('p33', { password: 'for-elsewhere' });
    // An outside edit that keeps the server leaves it; one that moves it drops it.
    followExternalEdit(saved, { ...saved, name: 'Renamed', version: 3 });
    expect(typedSecretsOf('p33')).toEqual({ password: 'for-elsewhere' });
    followExternalEdit({ ...saved, version: 3 }, { ...saved, host: 'attacker.example', version: 4 });
    expect(typedSecretsOf('p33')).toEqual({});
  });

  it('keeps a secret typed for the new settings when an older change is applied after it', () => {
    const before = { ...profile, id: 'p34', version: 1 };
    const after = { ...before, host: 'elsewhere', version: 2 };
    rememberTypedSecrets('p34', { password: 'typed-for-2' }, 2);
    followExternalEdit(before, after);
    expect(typedSecretsOf('p34')).toEqual({ password: 'typed-for-2' });
  });

  it('lists the connections it holds something for', () => {
    rememberTypedSecrets('p39', { password: 'a' }, 1);
    retainTypedSecretsAfterEdit({ ...profile, id: 'p40' }, { ...profile, id: 'p40', name: 'Renamed', version: 2 });
    expect(typedSecretIds()).toEqual(expect.arrayContaining(['p39', 'p40']));
    forgetTypedSecrets('p39');
    forgetTypedSecrets('p40');
    expect(typedSecretIds()).not.toContain('p39');
    expect(typedSecretIds()).not.toContain('p40');
  });

  it('forgets every typed secret on request', () => {
    rememberTypedSecrets('p35', { password: 'a' }, 1);
    rememberTypedSecrets('p36', { tlsPassphrase: 'b' }, 1);
    forgetAllTypedSecrets();
    expect(typedSecretsOf('p35')).toEqual({});
    expect(typedSecretsOf('p36')).toEqual({});
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
