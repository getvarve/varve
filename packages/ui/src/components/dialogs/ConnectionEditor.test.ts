import type * as RecordsModule from '../../api/records';
import { flushPromises, mount } from '@vue/test-utils';
import { CommandFailure } from '../../api/errors';
import type { ConnectionProfile } from '../../api/records';
import { rememberTypedSecrets, typedSecretsOf } from '../../composables/useConnect';
import ConnectionEditor from './ConnectionEditor.vue';

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
  environment: 'development',
  tls: { mode: 'prefer', caPath: null, clientCertPath: null, clientKeyPath: null },
  ssh: { enabled: false, host: '', port: 22, username: '', auth: 'password', keyPath: null },
  options: { readOnly: false, confirmDestructive: true, statementTimeoutMs: null, keepaliveS: null, initSql: null, showSystemObjects: false, redisDb: 0 },
  savedSecrets: ['ssh_key_passphrase'],
  createdAt: 0,
  updatedAt: 0,
  version: 1,
};

/** The profile as the backend stored it: moved to another host, its old keychain entry dropped. */
const stored: ConnectionProfile = { ...profile, host: 'elsewhere', savedSecrets: [], version: 2 };

const keychainFailure = (withProfile: boolean): CommandFailure =>
  new CommandFailure({ kind: 'keychain', message: 'The keychain is locked.', ...(withProfile ? { profile: stored } : {}) });

const updateConnection = vi.fn(async (..._args: unknown[]): Promise<ConnectionProfile> => {
  throw keychainFailure(true);
});
const saveSecret = vi.fn(async (..._args: unknown[]): Promise<ConnectionProfile> => ({ ...stored, savedSecrets: ['db_password'], version: 3 }));
const fetchTree = vi.fn(async () => ({ folders: [], connections: [{ ...stored, version: 9 }], queries: [] }));
vi.mock('../../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  updateConnection: (...args: unknown[]) => updateConnection(...args),
  saveSecret: (...args: unknown[]) => saveSecret(...args),
  fetchTree: () => fetchTree(),
}));

/** Types the database password (the dialog is teleported to the body) and ticks "Save in keychain". */
async function typePassword(value: string): Promise<void> {
  const input = document.body.querySelector<HTMLInputElement>('input[type="password"]');
  if (!input) throw new Error('no password field');
  input.value = value;
  input.dispatchEvent(new Event('input'));
  const save = input.parentElement?.querySelector<HTMLInputElement>('input[type="checkbox"]');
  if (!save) throw new Error('no save checkbox');
  if (!save.checked) {
    save.checked = true;
    save.dispatchEvent(new Event('change'));
  }
  await flushPromises();
}

async function submit(): Promise<void> {
  document.body.querySelector('form')?.dispatchEvent(new Event('submit'));
  for (let i = 0; i < 20; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
  await flushPromises();
}

describe('ConnectionEditor', () => {
  beforeEach(() => {
    updateConnection.mockReset();
    updateConnection.mockImplementation(async () => {
      throw keychainFailure(true);
    });
    saveSecret.mockClear();
  });

  it('treats a keychain error after a stored edit as saved and drops typed secrets of the old server', async () => {
    rememberTypedSecrets('p1', { password: 'typed-for-db' });
    const wrapper = mount(ConnectionEditor, { attachTo: document.body, props: { profile, engine: 'postgres', folderId: null } });
    await submit();
    expect(updateConnection).toHaveBeenCalledTimes(1);
    // The typed password belonged to `db`; the stored profile points at `elsewhere`.
    expect(typedSecretsOf('p1').password).toBeUndefined();
    // The dialog stays open with the warning instead of reporting an unsaved edit.
    expect(wrapper.emitted('done')).toBeUndefined();
    expect(document.body.textContent).toContain('The connection was saved');
    wrapper.unmount();
  });

  it('saves a typed password with the version the failed update stored, never a reloaded one', async () => {
    const wrapper = mount(ConnectionEditor, { attachTo: document.body, props: { profile, engine: 'postgres', folderId: null } });
    await typePassword('pw-for-elsewhere');
    await submit();
    expect(saveSecret).toHaveBeenCalledWith('p1', 'db_password', 'pw-for-elsewhere', 2);
    wrapper.unmount();
  });

  it('drops all typed secrets of the old server when the error does not carry the stored profile', async () => {
    updateConnection.mockImplementation(async () => {
      throw keychainFailure(false);
    });
    rememberTypedSecrets('p1', { password: 'typed-for-db', sshSecret: 'typed-ssh' });
    const wrapper = mount(ConnectionEditor, { attachTo: document.body, props: { profile, engine: 'postgres', folderId: null } });
    await submit();
    expect(typedSecretsOf('p1')).toEqual({});
    expect(wrapper.emitted('done')).toBeUndefined();
    expect(document.body.textContent).toContain('The connection was saved');
    wrapper.unmount();
  });

  it('says the connection was saved when the backend refuses the secret', async () => {
    updateConnection.mockImplementation(async () => ({ ...profile, version: 2 }));
    saveSecret.mockImplementationOnce(async () => {
      throw new CommandFailure({ kind: 'invalid', message: 'Enter it again for the new settings.' });
    });
    const wrapper = mount(ConnectionEditor, { attachTo: document.body, props: { profile, engine: 'postgres', folderId: null } });
    await typePassword('pw');
    await submit();
    expect(saveSecret).toHaveBeenCalledWith('p1', 'db_password', 'pw', 2);
    expect(document.body.textContent).toContain('The connection was saved, but the secret was not');
    expect(wrapper.emitted('done')).toBeUndefined();
    wrapper.unmount();
  });

  it('shows a later validation error as unsaved after an earlier stored save (R8-3)', async () => {
    updateConnection.mockImplementation(async () => ({ ...profile, version: 2 }));
    saveSecret.mockImplementationOnce(async () => {
      throw new CommandFailure({ kind: 'invalid', message: 'Enter it again for the new settings.' });
    });
    const wrapper = mount(ConnectionEditor, { attachTo: document.body, props: { profile, engine: 'postgres', folderId: null } });
    await typePassword('pw');
    await submit();
    expect(document.body.textContent).toContain('The connection was saved, but the secret was not');
    // Next attempt: the edit itself is refused.
    updateConnection.mockImplementation(async () => {
      throw new CommandFailure({ kind: 'invalid', message: 'The port is out of range.' });
    });
    await submit();
    expect(document.body.textContent).toContain('The port is out of range.');
    expect(document.body.textContent).not.toContain('The connection was saved, but the secret was not: The port');
    wrapper.unmount();
  });

  it('shows the saved database password even when the SSH secret then fails (R8-4)', async () => {
    const withSsh: ConnectionProfile = {
      ...profile,
      ssh: { enabled: true, host: 'bastion', port: 22, username: 'ops', auth: 'password', keyPath: null },
      savedSecrets: [],
    };
    updateConnection.mockImplementation(async () => ({ ...withSsh, version: 2 }));
    saveSecret.mockImplementationOnce(async () => ({ ...withSsh, savedSecrets: ['db_password'], version: 3 }));
    saveSecret.mockImplementationOnce(async () => {
      throw new CommandFailure({ kind: 'keychain', message: 'The keychain is locked.' });
    });
    const wrapper = mount(ConnectionEditor, { attachTo: document.body, props: { profile: withSsh, engine: 'postgres', folderId: null } });
    const fields = document.body.querySelectorAll<HTMLInputElement>('input[type="password"]');
    expect(fields.length).toBe(2);
    for (const field of fields) {
      field.value = 'secret';
      field.dispatchEvent(new Event('input'));
      const save = field.parentElement?.querySelector<HTMLInputElement>('input[type="checkbox"]');
      if (save && !save.checked) {
        save.checked = true;
        save.dispatchEvent(new Event('change'));
      }
    }
    await flushPromises();
    await submit();
    expect(saveSecret).toHaveBeenCalledTimes(2);
    expect(document.body.textContent).toContain('The keychain is locked.');
    // The database password shows as saved (its field is gone), the SSH secret does not.
    expect(document.body.querySelectorAll('input[type="password"]').length).toBe(1);
    wrapper.unmount();
  });

  it('offers a TLS key passphrase field only with a client key, and saves it in the keychain', async () => {
    const keyed: ConnectionProfile = {
      ...profile,
      tls: { mode: 'require', caPath: null, clientCertPath: '/c.pem', clientKeyPath: '/k.pem' },
      savedSecrets: [],
    };
    updateConnection.mockImplementation(async () => ({ ...keyed, version: 2 }));
    saveSecret.mockImplementationOnce(async () => ({ ...keyed, savedSecrets: ['tls_key_passphrase'], version: 3 }));
    const wrapper = mount(ConnectionEditor, { attachTo: document.body, props: { profile: keyed, engine: 'postgres', folderId: null } });
    const field = document.body.querySelector<HTMLInputElement>('input[placeholder^="Leave empty for an unencrypted key"]');
    if (!field) throw new Error('no TLS passphrase field');
    field.value = 'k3y-pass';
    field.dispatchEvent(new Event('input'));
    await flushPromises();
    await submit();
    expect(saveSecret).toHaveBeenCalledWith('p1', 'tls_key_passphrase', 'k3y-pass', 2);
    expect(saveSecret).toHaveBeenCalledTimes(1);
    expect(wrapper.emitted('done')?.[0]?.[0]).toMatchObject({ savedSecrets: ['tls_key_passphrase'] });
    wrapper.unmount();

    // Without a client key there is nothing to unlock.
    const plain = mount(ConnectionEditor, { attachTo: document.body, props: { profile, engine: 'postgres', folderId: null } });
    expect(document.body.querySelector('input[placeholder^="Leave empty for an unencrypted key"]')).toBeNull();
    plain.unmount();
  });

  it('shows a saved TLS key passphrase until the key file changes', async () => {
    const keyed: ConnectionProfile = {
      ...profile,
      tls: { mode: 'require', caPath: null, clientCertPath: '/c.pem', clientKeyPath: '/k.pem' },
      savedSecrets: ['tls_key_passphrase'],
    };
    const wrapper = mount(ConnectionEditor, { attachTo: document.body, props: { profile: keyed, engine: 'postgres', folderId: null } });
    const passphrase = (): HTMLInputElement | null =>
      document.body.querySelector<HTMLInputElement>('input[placeholder^="Leave empty for an unencrypted key"]');
    expect(passphrase()).toBeNull();
    expect(document.body.textContent).toContain('Saved in keychain');
    const keyInput = [...document.body.querySelectorAll<HTMLInputElement>('input.mono')].find((i) => i.value === '/k.pem');
    if (!keyInput) throw new Error('no key path field');
    keyInput.value = '/other.pem';
    keyInput.dispatchEvent(new Event('input'));
    await flushPromises();
    expect(passphrase()).not.toBeNull();
    wrapper.unmount();
  });
});

