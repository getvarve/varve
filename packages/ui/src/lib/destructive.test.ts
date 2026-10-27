import { CommandFailure } from '../api/errors';
import type { CmdError } from '../api/errors';
import { canConfirm, describeStatements, needsTypedName, runWithConfirmation } from './destructive';
import type { TlsMode } from '../api/records';
import { identityFingerprint, isLostSession, nextConnectStep, reusableTypedSecrets } from './connectFlow';

const confirmationError: CmdError = {
  kind: 'session',
  error: { kind: 'confirmationRequired', environment: 'production', statements: ['Drop', 'DeleteAll', 'Drop'] },
};

describe('destructive confirmation', () => {
  it('asks for the typed connection name on production only', () => {
    expect(needsTypedName('production')).toBe(true);
    expect(needsTypedName('staging')).toBe(false);
    expect(canConfirm('staging', 'Orders', '')).toBe(true);
    expect(canConfirm('production', 'Orders primary', 'Orders')).toBe(false);
    expect(canConfirm('production', 'Orders primary', '')).toBe(false);
    expect(canConfirm('production', 'Orders primary', ' Orders primary ')).toBe(true);
    expect(canConfirm('production', 'Orders primary', 'orders primary')).toBe(false);
  });

  it('describes statement kinds with counts', () => {
    expect(describeStatements(['Drop', 'DeleteAll', 'Drop', 'FLUSHALL'])).toEqual([
      { label: 'DROP', count: 2 },
      { label: 'DELETE without WHERE', count: 1 },
      { label: 'FLUSHALL', count: 1 },
    ]);
  });

  it('runs once when no confirmation is needed', async () => {
    const run = vi.fn(async (confirmed: boolean) => (confirmed ? 'confirmed' : 'plain'));
    const confirm = vi.fn(async () => true);
    await expect(runWithConfirmation(run, confirm)).resolves.toBe('plain');
    expect(run).toHaveBeenCalledTimes(1);
    expect(confirm).not.toHaveBeenCalled();
  });

  it('asks, then repeats the call with confirmed = true', async () => {
    const run = vi.fn(async (confirmed: boolean) => {
      if (!confirmed) throw new CommandFailure(confirmationError);
      return 'done';
    });
    const confirm = vi.fn(async () => true);
    await expect(runWithConfirmation(run, confirm)).resolves.toBe('done');
    expect(confirm).toHaveBeenCalledWith({ environment: 'production', statements: ['Drop', 'DeleteAll', 'Drop'] });
    expect(run.mock.calls).toEqual([[false], [true]]);
  });

  it('does not run when the user declines', async () => {
    const run = vi.fn(async (confirmed: boolean) => {
      if (!confirmed) throw confirmationError;
      return 'done';
    });
    await expect(runWithConfirmation(run, async () => false)).resolves.toBeNull();
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('passes other errors through', async () => {
    const run = vi.fn(async () => {
      throw new CommandFailure({ kind: 'internal', message: 'boom' });
    });
    await expect(runWithConfirmation(run, async () => true)).rejects.toMatchObject({ cmd: { kind: 'internal' } });
  });
});

describe('connect recovery steps', () => {
  const ssh = { ssh: { enabled: true, host: 'b', port: 22, username: 'u', auth: 'key_file' as const, keyPath: '/k' } };

  it('asks for the secret the backend names', () => {
    expect(nextConnectStep({ kind: 'secretRequired', secret: 'ssh_password' }, ssh)).toEqual({
      step: 'askSecret',
      secret: 'ssh_password',
      reason: null,
    });
  });

  it('offers to trust unknown and changed host keys', () => {
    expect(
      nextConnectStep(
        { kind: 'tunnel', message: 'm', code: 'host_key_changed', fingerprints: ['old', 'new'], presentedKey: 'ssh-ed25519 A', host: 'jump', port: 2200 },
        ssh,
      ),
    ).toEqual({ step: 'trustHostKey', code: 'host_key_changed', fingerprints: ['old', 'new'], presentedKey: 'ssh-ed25519 A', host: 'jump', port: 2200 });
    // Without the host and port of the presenting server there is nothing safe to trust.
    expect(
      nextConnectStep(
        { kind: 'tunnel', message: 'm', code: 'host_key_unknown', fingerprints: ['f'], presentedKey: 'k', host: null, port: null },
        ssh,
      ),
    ).toEqual({ step: 'fail' });
  });

  it('asks again after an authentication failure', () => {
    expect(nextConnectStep({ kind: 'tunnel', message: 'denied', code: 'auth', fingerprints: [], presentedKey: null, host: null, port: null }, ssh)).toEqual({
      step: 'askSecret',
      secret: 'ssh_key_passphrase',
      reason: 'denied',
    });
    expect(
      nextConnectStep(
        {
          kind: 'session',
          error: { kind: 'driver', error: { kind: 'auth', message: 'bad password', code: '28P01', detail: null, hint: null, position: null } },
        },
        ssh,
      ),
    ).toEqual({ step: 'askSecret', secret: 'db_password', reason: 'bad password' });
  });

  it('fails on everything else', () => {
    expect(nextConnectStep({ kind: 'internal', message: 'x' }, ssh)).toEqual({ step: 'fail' });
    // Other TLS errors are not answered with a prompt.
    expect(
      nextConnectStep(
        {
          kind: 'session',
          error: { kind: 'driver', error: { kind: 'tls', message: 'bad certificate', code: null, detail: null, hint: null, position: null } },
        },
        ssh,
      ),
    ).toEqual({ step: 'fail' });
    expect(nextConnectStep({ kind: 'tunnel', message: 'm', code: 'connect', fingerprints: [], presentedKey: null, host: null, port: null }, ssh)).toEqual({
      step: 'fail',
    });
  });
});

describe('lost sessions', () => {
  it('recognizes a dropped connection and an unknown session', () => {
    expect(isLostSession({ kind: 'session', error: { kind: 'notFound', sessionId: 's' } })).toBe(true);
    expect(
      isLostSession({
        kind: 'session',
        error: { kind: 'driver', error: { kind: 'connection_lost', message: 'x', code: null, detail: null, hint: null, position: null } },
      }),
    ).toBe(true);
    expect(
      isLostSession({
        kind: 'session',
        error: { kind: 'driver', error: { kind: 'query', message: 'x', code: null, detail: null, hint: null, position: null } },
      }),
    ).toBe(false);
    expect(isLostSession({ kind: 'internal', message: 'x' })).toBe(false);
  });
});

describe('reusableTypedSecrets (the backend table in commands.rs)', () => {
  const saved = {
    engine: 'postgres' as const,
    host: 'db.internal',
    port: 5432,
    username: 'app' as string | null,
    tls: { mode: 'prefer' as TlsMode, caPath: null as string | null, clientCertPath: '/c.pem', clientKeyPath: '/k.pem' },
    ssh: { enabled: true, host: 'bastion', port: 22, username: 'ops', auth: 'password' as const, keyPath: null },
  };
  const secrets = { password: 'pw', sshSecret: 'ssh' };

  it('keeps both for the same server with another name or host case', () => {
    expect(reusableTypedSecrets(saved, { ...saved, host: 'DB.internal' }, secrets)).toEqual(secrets);
  });

  it('drops the password for another host or user, keeping the SSH secret', () => {
    expect(reusableTypedSecrets(saved, { ...saved, host: 'evil.example' }, secrets)).toEqual({ sshSecret: 'ssh' });
    expect(reusableTypedSecrets(saved, { ...saved, username: 'root' }, secrets)).toEqual({ sshSecret: 'ssh' });
    expect(reusableTypedSecrets(saved, { ...saved, port: 6543 }, secrets)).toEqual({ sshSecret: 'ssh' });
    expect(reusableTypedSecrets(saved, { ...saved, engine: 'mysql' }, secrets)).toEqual({ sshSecret: 'ssh' });
  });

  it('drops both for another bastion, and the password when the tunnel is turned off', () => {
    expect(reusableTypedSecrets(saved, { ...saved, ssh: { ...saved.ssh, host: 'elsewhere' } }, secrets)).toEqual({});
    expect(reusableTypedSecrets(saved, { ...saved, ssh: { ...saved.ssh, enabled: false } }, secrets)).toEqual({});
  });

  it('refuses a weaker TLS mode, allows a stricter one, and needs the same CA to verify', () => {
    expect(reusableTypedSecrets(saved, { ...saved, tls: { ...saved.tls, mode: 'disable' } }, secrets)).toEqual({ sshSecret: 'ssh' });
    expect(reusableTypedSecrets(saved, { ...saved, tls: { ...saved.tls, mode: 'require' } }, secrets)).toEqual(secrets);
    expect(
      reusableTypedSecrets(saved, { ...saved, tls: { ...saved.tls, mode: 'verify_full', caPath: '/other-ca.pem' } }, secrets),
    ).toEqual({ sshSecret: 'ssh' });
    const verifying = { ...saved, tls: { ...saved.tls, mode: 'verify_ca' as TlsMode, caPath: '/ca.pem' } };
    expect(reusableTypedSecrets(verifying, { ...verifying, tls: { ...verifying.tls, mode: 'verify_full' } }, secrets)).toEqual(secrets);
  });

  it('compares host case for ASCII letters only, like eq_ignore_ascii_case', () => {
    const unicode = { ...saved, host: 'café.internal' };
    expect(reusableTypedSecrets(unicode, { ...unicode, host: 'CAFÉ.internal' }, secrets)).toEqual({ sshSecret: 'ssh' });
  });

  it('asks for the TLS key passphrase when it is missing, and again when it is wrong', () => {
    expect(nextConnectStep({ kind: 'secretRequired', secret: 'tls_key_passphrase' }, saved)).toEqual({
      step: 'askSecret',
      secret: 'tls_key_passphrase',
      reason: null,
    });
    const wrong: CmdError = {
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
    };
    expect(nextConnectStep(wrong, saved)).toEqual({
      step: 'askSecret',
      secret: 'tls_key_passphrase',
      reason: 'the passphrase does not decrypt the client key file',
    });
  });

  it('keeps a typed TLS key passphrase only for the same key file', () => {
    const withKey = { ...secrets, tlsPassphrase: 'kp' };
    // The passphrase belongs to the key file, not to the server.
    expect(reusableTypedSecrets(saved, { ...saved, host: 'evil.example' }, withKey)).toEqual({ sshSecret: 'ssh', tlsPassphrase: 'kp' });
    expect(reusableTypedSecrets(saved, { ...saved, tls: { ...saved.tls, clientKeyPath: '/other.pem' } }, withKey)).toEqual(secrets);
    const noKey = { ...saved, tls: { ...saved.tls, clientCertPath: null, clientKeyPath: null } };
    expect(reusableTypedSecrets(noKey, noKey, withKey)).toEqual(secrets);
    expect(identityFingerprint(saved)).not.toBe(identityFingerprint({ ...saved, tls: { ...saved.tls, clientKeyPath: '/other.pem' } }));
  });

  it('fingerprints change exactly when reuse could change', () => {
    expect(identityFingerprint(saved)).toBe(identityFingerprint({ ...saved, host: 'DB.internal' }));
    expect(identityFingerprint(saved)).not.toBe(identityFingerprint({ ...saved, tls: { ...saved.tls, mode: 'require' } }));
  });
});

describe('confirmation labels', () => {
  it('names every label the backend sends, including AlterDatabase', () => {
    for (const kind of ['AlterDatabase', 'Select', 'Transaction', 'Explain', 'Session', 'Call', 'Copy', 'Maintenance', 'Other']) {
      expect(describeStatements([kind])[0]?.label).not.toBe(kind);
    }
    expect(describeStatements(['AlterDatabase'])[0]?.label).toMatch(/^ALTER DATABASE/);
  });
});
