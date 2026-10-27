import { z } from 'zod';
import type {
  ConnectionInput,
  ConnectionProfile,
  Engine,
  Environment,
  SshAuthMethod,
  TlsMode,
} from '../api/records';

/**
 * The connection editor form. Mirrors `validate_connection` in
 * crates/records/src/repo.rs so the user sees the server's rules (and its
 * messages) before saving. Numbers are text in the form; empty means "not set".
 */

const ENGINE = z.enum(['postgres', 'mysql', 'mariadb', 'redis']);
const ENVIRONMENT = z.enum(['development', 'test', 'staging', 'production']);
const TLS_MODE = z.enum(['disable', 'prefer', 'require', 'verify_ca', 'verify_full']);
const SSH_AUTH = z.enum(['password', 'key_file', 'agent']);

/** Color tokens the editor offers; the server accepts any lowercase token. */
export const COLOR_TOKENS = ['red', 'amber', 'green', 'blue', 'violet', 'gray'] as const;

const MAX_NAME = 200;
const MAX_HOST = 255;
const MAX_TIMEOUT_MS = 86_400_000;
const MAX_INIT_SQL_BYTES = 65_536;
const MAX_U32 = 4_294_967_295;

const isDigits = (v: string): boolean => /^\d+$/.test(v);
const inRange = (v: string, min: number, max: number): boolean => isDigits(v) && Number(v) >= min && Number(v) <= max;
const utf8Length = (v: string): number => new TextEncoder().encode(v).length;

// Every rule lives in one `superRefine` over plain field types, so all rules
// always run together: one broken field never hides the rules of another, and
// `isRequired` (schemaRequired.ts) sees conditional requirements at any time.
export const connectionFormSchema = z
  .object({
    name: z.string(),
    folderId: z.string(),
    color: z.string(),
    engine: ENGINE,
    host: z.string(),
    port: z.string(),
    database: z.string(),
    username: z.string(),
    environment: ENVIRONMENT,
    password: z.string(),
    savePassword: z.boolean(),
    tls: z.object({
      mode: TLS_MODE,
      caPath: z.string(),
      clientCertPath: z.string(),
      clientKeyPath: z.string(),
      /** Passphrase of an encrypted client key; never stored in the profile. */
      passphrase: z.string(),
      savePassphrase: z.boolean(),
    }),
    ssh: z.object({
      enabled: z.boolean(),
      host: z.string(),
      port: z.string(),
      username: z.string(),
      auth: SSH_AUTH,
      keyPath: z.string(),
      secret: z.string(),
      saveSecret: z.boolean(),
    }),
    options: z.object({
      readOnly: z.boolean(),
      confirmDestructive: z.boolean(),
      statementTimeoutMs: z.string(),
      keepaliveS: z.string(),
      initSql: z.string(),
      showSystemObjects: z.boolean(),
      redisDb: z.string(),
    }),
  })
  .superRefine((v, ctx) => {
    const issue = (path: (string | number)[], message: string): void => {
      ctx.addIssue({ code: z.ZodIssueCode.custom, path, message });
    };

    const name = v.name.trim();
    if (name === '') issue(['name'], 'A connection needs a name.');
    else if ([...name].length > MAX_NAME) issue(['name'], `Connection names can be at most ${MAX_NAME} characters.`);

    const color = v.color.trim();
    if (color !== '' && !(color.length <= 32 && /^[a-z0-9-]+$/.test(color))) {
      issue(['color'], 'Colors are lowercase tokens such as `violet` or `red-500`.');
    }

    const host = v.host.trim();
    if (host === '') issue(['host'], 'Enter the host name or IP address.');
    else if (host.length > MAX_HOST || /\s/.test(host)) {
      issue(['host'], 'The host name cannot contain spaces and can be at most 255 characters.');
    }
    const port = v.port.trim();
    if (port !== '' && !inRange(port, 1, 65535)) issue(['port'], 'The port must be between 1 and 65535.');
    if (v.engine === 'redis' && v.database.trim() !== '') {
      issue(['database'], 'Redis has no database name; choose the logical database number instead.');
    }

    const cert = v.tls.clientCertPath.trim() !== '';
    const key = v.tls.clientKeyPath.trim() !== '';
    if (cert && !key) issue(['tls', 'clientKeyPath'], 'A client certificate needs its key, and a key needs its certificate.');
    if (key && !cert) issue(['tls', 'clientCertPath'], 'A client certificate needs its key, and a key needs its certificate.');
    if (v.tls.mode === 'disable' && (v.tls.caPath.trim() !== '' || cert)) {
      issue(['tls', 'mode'], 'TLS is disabled, so remove the certificate files or pick another TLS mode.');
    }

    if (v.ssh.enabled) {
      const sshHost = v.ssh.host.trim();
      if (sshHost === '' || /\s/.test(sshHost)) issue(['ssh', 'host'], 'Enter the SSH host.');
      if (!inRange(v.ssh.port.trim(), 1, 65535)) issue(['ssh', 'port'], 'The SSH port must be between 1 and 65535.');
      if (v.ssh.username.trim() === '') issue(['ssh', 'username'], 'Enter the SSH user.');
      if (v.ssh.auth === 'key_file' && v.ssh.keyPath.trim() === '') issue(['ssh', 'keyPath'], 'Choose the SSH private key file.');
    }

    const timeout = v.options.statementTimeoutMs.trim();
    if (timeout !== '' && !inRange(timeout, 1, MAX_TIMEOUT_MS)) {
      issue(['options', 'statementTimeoutMs'], 'The statement timeout must be between 1 ms and 24 hours.');
    }
    const keepalive = v.options.keepaliveS.trim();
    if (keepalive !== '' && !inRange(keepalive, 0, MAX_U32)) {
      issue(['options', 'keepaliveS'], 'The keepalive interval is a whole number of seconds.');
    }
    if (utf8Length(v.options.initSql) > MAX_INIT_SQL_BYTES) issue(['options', 'initSql'], 'Init SQL can be at most 64 KB.');
    const db = v.options.redisDb.trim();
    if (v.engine === 'redis') {
      if (!inRange(db, 0, 15)) issue(['options', 'redisDb'], 'Redis database numbers go from 0 to 15 on a default server.');
    } else if (db !== '' && db !== '0') {
      issue(['options', 'redisDb'], 'A Redis database number only applies to Redis connections.');
    }
  });

export type ConnectionFormValues = z.input<typeof connectionFormSchema>;

/** What the editor submits: the profile input and the secrets typed in the form. */
export interface ConnectionSubmit {
  input: ConnectionInput;
  password: string | null;
  savePassword: boolean;
  sshSecret: string | null;
  saveSshSecret: boolean;
  /** The TLS client key passphrase typed in the form (only with a client key and TLS on). */
  tlsPassphrase: string | null;
  saveTlsPassphrase: boolean;
}

const optText = (v: string): string | null => (v.trim() === '' ? null : v.trim());
const optNumber = (v: string): number | null => (v.trim() === '' ? null : Number(v.trim()));

/** Converts valid form values into the backend input (the same normalization the server applies). */
export function toConnectionSubmit(v: ConnectionFormValues): ConnectionSubmit {
  const engine: Engine = v.engine;
  const sshEnabled = v.ssh.enabled;
  const input: ConnectionInput = {
    folderId: v.folderId === '' ? null : v.folderId,
    name: v.name.trim(),
    color: optText(v.color),
    engine,
    host: v.host.trim(),
    port: optNumber(v.port),
    database: engine === 'redis' ? null : optText(v.database),
    username: optText(v.username),
    environment: v.environment,
    tls: {
      mode: v.tls.mode,
      caPath: optText(v.tls.caPath),
      clientCertPath: optText(v.tls.clientCertPath),
      clientKeyPath: optText(v.tls.clientKeyPath),
    },
    ssh: {
      enabled: sshEnabled,
      host: v.ssh.host.trim(),
      port: optNumber(v.ssh.port) ?? 22,
      username: v.ssh.username.trim(),
      auth: v.ssh.auth,
      keyPath: optText(v.ssh.keyPath),
    },
    options: {
      readOnly: v.options.readOnly,
      confirmDestructive: v.environment === 'production' ? true : v.options.confirmDestructive,
      statementTimeoutMs: optNumber(v.options.statementTimeoutMs),
      keepaliveS: optNumber(v.options.keepaliveS),
      initSql: optText(v.options.initSql),
      showSystemObjects: v.options.showSystemObjects,
      redisDb: engine === 'redis' ? (optNumber(v.options.redisDb) ?? 0) : 0,
    },
  };
  const sshUsesSecret = sshEnabled && v.ssh.auth !== 'agent';
  const usesClientKey = v.tls.mode !== 'disable' && v.tls.clientKeyPath.trim() !== '';
  return {
    input,
    password: v.password === '' ? null : v.password,
    savePassword: v.savePassword,
    sshSecret: sshUsesSecret && v.ssh.secret !== '' ? v.ssh.secret : null,
    saveSshSecret: v.ssh.saveSecret,
    tlsPassphrase: usesClientKey && v.tls.passphrase !== '' ? v.tls.passphrase : null,
    saveTlsPassphrase: v.tls.savePassphrase,
  };
}

/** Form values for a new connection. */
export function emptyConnectionForm(engine: Engine = 'postgres', folderId: string | null = null): ConnectionFormValues {
  return {
    name: '',
    folderId: folderId ?? '',
    color: '',
    engine,
    host: 'localhost',
    port: '',
    database: '',
    username: '',
    environment: 'development' satisfies Environment,
    password: '',
    savePassword: true,
    tls: {
      mode: 'prefer' satisfies TlsMode,
      caPath: '',
      clientCertPath: '',
      clientKeyPath: '',
      passphrase: '',
      savePassphrase: true,
    },
    ssh: {
      enabled: false,
      host: '',
      port: '22',
      username: '',
      auth: 'password' satisfies SshAuthMethod,
      keyPath: '',
      secret: '',
      saveSecret: true,
    },
    options: {
      readOnly: false,
      confirmDestructive: true,
      statementTimeoutMs: '',
      keepaliveS: '',
      initSql: '',
      showSystemObjects: false,
      redisDb: '0',
    },
  };
}

/** Form values that edit a saved profile. Secrets are never loaded; the form shows whether they are saved. */
export function connectionFormFromProfile(p: ConnectionProfile): ConnectionFormValues {
  const num = (n: number | null): string => (n === null ? '' : String(n));
  return {
    name: p.name,
    folderId: p.folderId ?? '',
    color: p.color ?? '',
    engine: p.engine,
    host: p.host,
    port: String(p.port),
    database: p.database ?? '',
    username: p.username ?? '',
    environment: p.environment,
    password: '',
    savePassword: true,
    tls: {
      mode: p.tls.mode,
      caPath: p.tls.caPath ?? '',
      clientCertPath: p.tls.clientCertPath ?? '',
      clientKeyPath: p.tls.clientKeyPath ?? '',
      passphrase: '',
      savePassphrase: true,
    },
    ssh: {
      enabled: p.ssh.enabled,
      host: p.ssh.host,
      port: String(p.ssh.port),
      username: p.ssh.username,
      auth: p.ssh.auth,
      keyPath: p.ssh.keyPath ?? '',
      secret: '',
      saveSecret: true,
    },
    options: {
      readOnly: p.options.readOnly,
      confirmDestructive: p.options.confirmDestructive,
      statementTimeoutMs: num(p.options.statementTimeoutMs),
      keepaliveS: num(p.options.keepaliveS),
      initSql: p.options.initSql ?? '',
      showSystemObjects: p.options.showSystemObjects,
      redisDb: String(p.options.redisDb),
    },
  };
}

/** Server messages from `validate_connection`, mapped to the field they belong to. */
const SERVER_MESSAGE_FIELDS: [RegExp, string][] = [
  [/needs a name|names can be at most/, 'name'],
  [/^Colors are/, 'color'],
  [/host name or IP|host name cannot/, 'host'],
  [/^The port must/, 'port'],
  [/Redis has no database name/, 'database'],
  [/client certificate needs its key/, 'tls.clientCertPath'],
  [/TLS is disabled/, 'tls.mode'],
  [/SSH host/, 'ssh.host'],
  [/SSH port/, 'ssh.port'],
  [/SSH user/, 'ssh.username'],
  [/SSH private key/, 'ssh.keyPath'],
  [/statement timeout/, 'options.statementTimeoutMs'],
  [/Init SQL/, 'options.initSql'],
  [/^Init command on line/, 'options.initSql'],
  [/Redis database number/, 'options.redisDb'],
];

/** The form field a server `invalid` message belongs to, or `null` for a form-level message. */
export function fieldForServerMessage(message: string): string | null {
  for (const [pattern, field] of SERVER_MESSAGE_FIELDS) if (pattern.test(message)) return field;
  return null;
}
