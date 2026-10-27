import type { ConnectionProfile } from '../api/records';
import {
  connectionFormFromProfile,
  connectionFormSchema,
  emptyConnectionForm,
  fieldForServerMessage,
  toConnectionSubmit,
} from './connectionSchema';
import type { ConnectionFormValues } from './connectionSchema';
import { isRequired, requiredFields } from './schemaRequired';

function valid(overrides: Partial<ConnectionFormValues> = {}): ConnectionFormValues {
  return { ...emptyConnectionForm('postgres'), name: 'Orders', host: 'db.internal', ...overrides };
}

function errorsOf(values: ConnectionFormValues): Record<string, string> {
  const result = connectionFormSchema.safeParse(values);
  if (result.success) return {};
  return Object.fromEntries(result.error.issues.map((i) => [i.path.join('.'), i.message]));
}

describe('connection form schema', () => {
  it('accepts a minimal valid connection', () => {
    expect(errorsOf(valid())).toEqual({});
  });

  it('requires a name and a host with the server messages', () => {
    expect(errorsOf(valid({ name: '  ', host: '' }))).toEqual({
      name: 'A connection needs a name.',
      host: 'Enter the host name or IP address.',
    });
  });

  it('rejects long names, spaces in hosts and bad ports', () => {
    const errors = errorsOf(valid({ name: 'x'.repeat(201), host: 'db internal', port: '70000' }));
    expect(Object.keys(errors).sort()).toEqual(['host', 'name', 'port']);
    expect(errorsOf(valid({ port: '0' })).port).toBe('The port must be between 1 and 65535.');
    expect(errorsOf(valid({ port: '5432' }))).toEqual({});
  });

  it('rejects a database name for Redis and checks the Redis database number', () => {
    const redis = { ...emptyConnectionForm('redis'), name: 'Cache', host: 'cache' };
    expect(errorsOf({ ...redis, database: 'shop' }).database).toMatch(/Redis has no database name/);
    expect(errorsOf({ ...redis, options: { ...redis.options, redisDb: '16' } })['options.redisDb']).toMatch(/0 to 15/);
    expect(errorsOf({ ...redis, options: { ...redis.options, redisDb: '3' } })).toEqual({});
    const pg = valid();
    expect(errorsOf({ ...pg, options: { ...pg.options, redisDb: '2' } })['options.redisDb']).toMatch(/only applies to Redis/);
  });

  it('checks TLS file pairs and disabled TLS', () => {
    const base = valid();
    expect(errorsOf({ ...base, tls: { ...base.tls, clientCertPath: '/c.crt' } })['tls.clientKeyPath']).toMatch(/needs its key/);
    expect(errorsOf({ ...base, tls: { ...base.tls, mode: 'disable', caPath: '/ca.pem' } })['tls.mode']).toMatch(/TLS is disabled/);
  });

  it('requires SSH fields only when the tunnel is on', () => {
    const base = valid();
    expect(errorsOf({ ...base, ssh: { ...base.ssh, host: '' } })).toEqual({});
    const errors = errorsOf({ ...base, ssh: { ...base.ssh, enabled: true, auth: 'key_file', port: '' } });
    expect(Object.keys(errors).sort()).toEqual(['ssh.host', 'ssh.keyPath', 'ssh.port', 'ssh.username']);
  });

  it('checks the statement timeout and init SQL size', () => {
    const base = valid();
    expect(errorsOf({ ...base, options: { ...base.options, statementTimeoutMs: '0' } })['options.statementTimeoutMs']).toMatch(
      /between 1 ms and 24 hours/,
    );
    expect(errorsOf({ ...base, options: { ...base.options, statementTimeoutMs: '86400001' } })).not.toEqual({});
    expect(errorsOf({ ...base, options: { ...base.options, initSql: 'é'.repeat(40_000) } })['options.initSql']).toMatch(/64 KB/);
  });

  it('converts to the backend input like the server normalizes it', () => {
    const base = valid({ name: ' Orders ', database: ' shop ', environment: 'production', port: '' });
    const out = toConnectionSubmit({ ...base, options: { ...base.options, confirmDestructive: false }, password: 'pw' });
    expect(out.input).toMatchObject({
      name: 'Orders',
      database: 'shop',
      port: null,
      username: null,
      folderId: null,
      options: { confirmDestructive: true, statementTimeoutMs: null, redisDb: 0 },
      ssh: { enabled: false, port: 22, keyPath: null },
    });
    expect(out.password).toBe('pw');
    expect(out.sshSecret).toBeNull();
  });

  it('round trips a saved profile', () => {
    const submit = toConnectionSubmit(valid({ port: '6543', username: 'kim' }));
    const profile: ConnectionProfile = {
      ...submit.input,
      port: 6543,
      id: 'id',
      workspaceId: 'personal',
      sortOrder: 0,
      savedSecrets: [],
      createdAt: 0,
      updatedAt: 0,
      version: 1,
    };
    expect(toConnectionSubmit(connectionFormFromProfile(profile)).input).toEqual(submit.input);
  });

  it('derives required markers from the schema, including conditional ones', () => {
    const paths = ['name', 'host', 'port', 'database', 'ssh.host', 'ssh.username', 'ssh.keyPath'];
    expect([...requiredFields(connectionFormSchema, valid(), paths)]).toEqual(['name', 'host']);
    const base = valid({ name: '' });
    const ssh = { ...base, ssh: { ...base.ssh, enabled: true, auth: 'key_file' as const } };
    expect([...requiredFields(connectionFormSchema, ssh, paths)]).toEqual(['name', 'host', 'ssh.host', 'ssh.username', 'ssh.keyPath']);
    expect(isRequired(connectionFormSchema, valid(), 'tls.clientKeyPath')).toBe(false);
    const withCert = valid();
    expect(isRequired(connectionFormSchema, { ...withCert, tls: { ...withCert.tls, clientCertPath: '/c' } }, 'tls.clientKeyPath')).toBe(true);
  });

  it('maps server validation messages to fields', () => {
    expect(fieldForServerMessage('Enter the SSH user.')).toBe('ssh.username');
    expect(fieldForServerMessage('The host name cannot contain spaces and can be at most 255 characters.')).toBe('host');
    expect(fieldForServerMessage('Init command on line 2: AUTH cannot be an init command: it puts a password in the init text')).toBe('options.initSql');
    expect(fieldForServerMessage('Something new.')).toBeNull();
  });
});
