import type { Engine, Environment, TlsMode } from '../api/records';
import type { Encryption } from '../api/sessions';

export const ENGINE_INFO: Record<Engine, { code: string; name: string; versions: string; color: string }> = {
  postgres: { code: 'PG', name: 'PostgreSQL', versions: '13 and later', color: 'var(--engine-pg)' },
  mysql: { code: 'MY', name: 'MySQL', versions: '8.0 and later', color: 'var(--engine-my)' },
  mariadb: { code: 'MA', name: 'MariaDB', versions: '10.6 and later', color: 'var(--engine-ma)' },
  redis: { code: 'RD', name: 'Redis', versions: '6.2 and later', color: 'var(--engine-rd)' },
};

export const ENVIRONMENT_INFO: Record<
  Environment,
  { label: string; short: string; tone: 'danger' | 'warning' | 'info' | 'success'; color: string; onColor: string }
> = {
  production: { label: 'Production', short: 'PRODUCTION', tone: 'danger', color: 'var(--env-production)', onColor: 'var(--on-env-production)' },
  staging: { label: 'Staging', short: 'STAGING', tone: 'warning', color: 'var(--env-staging)', onColor: 'var(--on-env-staging)' },
  test: { label: 'Test', short: 'TEST', tone: 'info', color: 'var(--env-test)', onColor: 'var(--on-env-test)' },
  development: { label: 'Development', short: 'DEV', tone: 'success', color: 'var(--env-development)', onColor: 'var(--on-env-development)' },
};

export const ENVIRONMENT_HELP: Record<Environment, string> = {
  development: 'Local or personal databases. No extra safety checks beyond what you turn on.',
  test: 'Shared test databases. Shown in blue so you can tell them apart from production.',
  staging: 'Pre-production copies. Shown in amber in the tree, on tabs and in the status bar.',
  production:
    'Production shows red in the tree, on tabs and in the status bar. Destructive statements always ask for confirmation, and you type the connection name before they run.',
};

export const TLS_MODE_INFO: Record<TlsMode, { label: string; help: string }> = {
  disable: { label: 'disable', help: 'No encryption. Only for local servers or trusted networks.' },
  prefer: {
    label: 'prefer',
    help: 'Uses TLS when the server offers it and falls back to an unencrypted connection otherwise. For Redis, connects without TLS when the port answers plain Redis and with TLS otherwise; for a Redis endpoint that accepts both, choose require so the password is never sent unencrypted.',
  },
  require: { label: 'require', help: 'Always encrypts, but does not verify the server certificate.' },
  verify_ca: { label: 'verify-ca', help: 'Encrypts and verifies the server certificate against the CA certificate below.' },
  verify_full: {
    label: 'verify-full',
    help: 'Encrypts, verifies the certificate against the CA and checks that it was issued for this host name. Recommended for production.',
  },
};

/** What the status bar says about a session's encryption; `null` when the driver cannot tell. */
export interface EncryptionStatus {
  label: string;
  /** Shown next to the label: a short call to action, or `null`. */
  note: string | null;
  /** Tooltip with the explanation. */
  hint: string;
  /** Styled as a warning. */
  warn: boolean;
}

/**
 * The status bar's encryption segment. `viaSsh`: the session runs through
 * an SSH tunnel, which encrypts the way to the bastion but not from the
 * bastion to the server.
 */
export function encryptionStatus(encryption: Encryption, viaSsh: boolean): EncryptionStatus | null {
  if (encryption.tls === null) return null;
  if (encryption.tls) {
    if (encryption.mode === 'prefer' || encryption.mode === 'require') {
      return {
        label: 'Encrypted (TLS, certificate not verified)',
        note: null,
        hint: `The connection to the server uses TLS, but TLS mode ${encryption.mode} does not check the server's certificate. Choose verify-full with the server's CA to check it.`,
        warn: false,
      };
    }
    return {
      label: 'Encrypted (TLS)',
      note: null,
      hint:
        encryption.mode === 'verify_ca'
          ? "The connection to the server uses TLS, and the certificate is checked against the CA (not the host name)."
          : 'The connection to the server uses TLS, and the certificate and host name are checked.',
      warn: false,
    };
  }
  const label = viaSsh ? 'Not encrypted to the server (SSH to the bastion)' : 'Not encrypted';
  const leg = viaSsh
    ? 'SSH encrypts the way to the bastion; from the bastion to the server the connection is not encrypted. '
    : '';
  const password = encryption.passwordUnencrypted
    ? `The password was sent unencrypted${viaSsh ? ' from the bastion to the server' : ''}. `
    : '';
  if (encryption.mode === 'disable') {
    return { label, note: null, hint: `${leg}${password}TLS is off for this connection (TLS mode disable).`, warn: false };
  }
  return {
    label,
    note: 'use require',
    hint: `${leg}${password}The server answered without TLS, so TLS mode prefer connected unencrypted. Choose require in the connection's TLS settings to never connect without TLS.`,
    warn: true,
  };
}

/** Color token of a profile color name such as `red`. */
export function colorVar(color: string | null): string | null {
  if (!color) return null;
  return `var(--tag-${color}, var(--tag-gray))`;
}

export function formatTime(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number): string => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

export function formatDateTime(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number, w = 2): string => String(n).padStart(w, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(
    d.getSeconds(),
  )}.${pad(d.getMilliseconds(), 3)}`;
}

export function formatDay(ms: number): string {
  return new Date(ms).toLocaleDateString('en-GB', { weekday: 'long', day: 'numeric', month: 'long', year: 'numeric' });
}

/** A folder name indented with no-break spaces, for folder pickers in a `<select>`. */
export function indentLabel(name: string, depth: number): string {
  return `${String.fromCharCode(160).repeat(depth * 2)}${name}`;
}

/** `host:port`, with an IPv6 address in brackets (`[::1]:22`) so the port stays readable. */
export function hostPort(host: string, port: number): string {
  const bare = host.startsWith('[') && host.endsWith(']') ? host.slice(1, -1) : host;
  return bare.includes(':') ? `[${bare}]:${port}` : `${host}:${port}`;
}
