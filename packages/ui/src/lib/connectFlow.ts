import type { CmdError } from '../api/errors';
import type { ConnectionProfile, SecretKindName, TlsMode } from '../api/records';
import type { TypedSecrets } from '../api/sessions';

/** What to do after a connect attempt failed. */
export type ConnectStep =
  | { step: 'askSecret'; secret: SecretKindName; reason: string | null }
  | {
      step: 'trustHostKey';
      code: 'host_key_unknown' | 'host_key_changed';
      fingerprints: string[];
      presentedKey: string;
      /** The SSH host and port that presented the key, from the error itself. */
      host: string;
      port: number;
    }
  | { step: 'fail' };

/** Driver error code: the passphrase does not decrypt the TLS client key (`varve_driver::tls_key`). */
export const TLS_PASSPHRASE_WRONG = 'tls_key_passphrase_wrong';

/** The SSH secret the profile authenticates with, or `null` for the agent. */
export function sshSecretKind(profile: Pick<ConnectionProfile, 'ssh'>): SecretKindName | null {
  if (!profile.ssh.enabled) return null;
  if (profile.ssh.auth === 'password') return 'ssh_password';
  if (profile.ssh.auth === 'key_file') return 'ssh_key_passphrase';
  return null;
}

/**
 * Decides how to recover from a failed `open_session` or `test_connection`:
 * prompt for the secret the backend asked for, ask to trust a host key, or
 * prompt again after an authentication failure. Everything else fails.
 *
 * The backend never reports `secretRequired` for the database password; a
 * missing or wrong one comes back as a driver `auth` error, which is
 * answered with the password prompt as well. An encrypted TLS client key
 * without a passphrase is `secretRequired`; a wrong passphrase is a driver
 * `tls` error with code `tls_key_passphrase_wrong`, answered with the
 * passphrase prompt again.
 */
export function nextConnectStep(error: CmdError, profile: Pick<ConnectionProfile, 'ssh'>): ConnectStep {
  switch (error.kind) {
    case 'secretRequired':
      return { step: 'askSecret', secret: error.secret, reason: null };
    case 'tunnel': {
      if (error.code === 'host_key_unknown' || error.code === 'host_key_changed') {
        // Trust exactly the server that presented the key; without it there is nothing safe to trust.
        if (!error.presentedKey || error.host === null || error.port === null) return { step: 'fail' };
        return {
          step: 'trustHostKey',
          code: error.code,
          fingerprints: error.fingerprints,
          presentedKey: error.presentedKey,
          host: error.host,
          port: error.port,
        };
      }
      const kind = sshSecretKind(profile);
      if (error.code === 'auth' && kind) return { step: 'askSecret', secret: kind, reason: error.message };
      return { step: 'fail' };
    }
    case 'session':
      if (error.error.kind === 'driver' && error.error.error.kind === 'auth') {
        return { step: 'askSecret', secret: 'db_password', reason: error.error.error.message };
      }
      if (error.error.kind === 'driver' && error.error.error.code === TLS_PASSPHRASE_WRONG) {
        return { step: 'askSecret', secret: 'tls_key_passphrase', reason: error.error.error.message };
      }
      return { step: 'fail' };
    default:
      return { step: 'fail' };
  }
}

/**
 * True when the session behind a tab is gone: the server dropped the
 * connection, or the backend no longer knows the session id. The tab then
 * offers to connect again instead of showing a query error.
 */
export function isLostSession(error: CmdError): boolean {
  if (error.kind !== 'session') return false;
  if (error.error.kind === 'notFound') return true;
  return error.error.kind === 'driver' && error.error.error.kind === 'connection_lost';
}

/** Lower case for ASCII letters only, like Rust's `eq_ignore_ascii_case`. */
function asciiLower(text: string): string {
  return text.replace(/[A-Z]/g, (c) => c.toLowerCase());
}

type Identity = Pick<ConnectionProfile, 'engine' | 'host' | 'port' | 'username' | 'ssh' | 'tls'>;

const TLS_RANK: Record<TlsMode, number> = { disable: 0, prefer: 1, require: 2, verify_ca: 3, verify_full: 4 };

/**
 * The typed (not saved) secrets that may still be used after a profile was
 * edited, by the same rule as `reusable_secrets` in app/src-tauri/src/commands.rs:
 *
 * - the database password only while engine, host (ASCII letters compared
 *   without case), port and user are equal, the route is the same (tunnel on
 *   or off alike, and when on the same SSH host, port and user), and TLS is
 *   not weaker (disable < prefer < require < verify-ca < verify-full; for the
 *   verifying modes the CA file must be the same);
 * - the SSH secret only while the tunnel stays on with the same host, port,
 *   user, auth method and key file;
 * - the TLS key passphrase only while the client key file is the same.
 *
 * A secret must never reach a server, route or weaker channel the user just typed in.
 */
export function reusableTypedSecrets(saved: Identity, edited: Identity, secrets: TypedSecrets): TypedSecrets {
  const sameRoute =
    saved.ssh.enabled === edited.ssh.enabled &&
    (!saved.ssh.enabled ||
      (asciiLower(saved.ssh.host) === asciiLower(edited.ssh.host) &&
        saved.ssh.port === edited.ssh.port &&
        saved.ssh.username === edited.ssh.username));
  const editedRank = TLS_RANK[edited.tls.mode];
  const sameTrust = editedRank >= TLS_RANK[saved.tls.mode] && (editedRank < 3 || saved.tls.caPath === edited.tls.caPath);
  const sameServer =
    saved.engine === edited.engine &&
    asciiLower(saved.host) === asciiLower(edited.host) &&
    saved.port === edited.port &&
    saved.username === edited.username &&
    sameRoute &&
    sameTrust;
  const sameSsh =
    saved.ssh.enabled &&
    edited.ssh.enabled &&
    asciiLower(saved.ssh.host) === asciiLower(edited.ssh.host) &&
    saved.ssh.port === edited.ssh.port &&
    saved.ssh.username === edited.ssh.username &&
    saved.ssh.auth === edited.ssh.auth &&
    saved.ssh.keyPath === edited.ssh.keyPath;
  const sameTlsKey = saved.tls.clientKeyPath !== null && saved.tls.clientKeyPath === edited.tls.clientKeyPath;
  const out: TypedSecrets = {};
  if (sameServer && secrets.password !== undefined) out.password = secrets.password;
  if (sameSsh && secrets.sshSecret !== undefined) out.sshSecret = secrets.sshSecret;
  if (sameTlsKey && secrets.tlsPassphrase !== undefined) out.tlsPassphrase = secrets.tlsPassphrase;
  return out;
}

/** A fingerprint of everything that decides where typed secrets may go; it changes whenever reuse could. */
export function identityFingerprint(p: Identity): string {
  return JSON.stringify([
    p.engine,
    asciiLower(p.host),
    p.port,
    p.username,
    p.ssh.enabled,
    asciiLower(p.ssh.host),
    p.ssh.port,
    p.ssh.username,
    p.ssh.auth,
    p.ssh.keyPath,
    p.tls.mode,
    p.tls.caPath,
    p.tls.clientKeyPath,
  ]);
}
