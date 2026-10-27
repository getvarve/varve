import type { ConnectionProfile, Engine, Environment, SecretKindName } from './records';
import type { PartialOutput } from './sessions';

/** `ErrorKind` in crates/driver/src/lib.rs. */
export type DriverErrorKind =
  | 'connect'
  | 'auth'
  | 'tls'
  | 'timeout'
  | 'cancelled'
  | 'query'
  | 'read_only'
  | 'unsupported'
  | 'protocol'
  | 'connection_lost';

/** `DriverError` in crates/driver/src/lib.rs. `position` is a 1-based character offset in the submitted text. */
export interface DriverError {
  kind: DriverErrorKind;
  message: string;
  code: string | null;
  detail: string | null;
  hint: string | null;
  position: number | null;
  /** What a failed script produced before it failed; absent or `null` when nothing completed. */
  partial?: PartialOutput | null;
}

/** `SessionError` in app/src-tauri/src/sessions.rs. */
export type SessionError =
  | { kind: 'driver'; error: DriverError }
  | { kind: 'notFound'; sessionId: string }
  | { kind: 'confirmationRequired'; environment: Environment; statements: string[] }
  /**
   * The call must not run inside the transaction the session has open; nothing
   * ran and a stored plan stays available. Show `message` with "commit or roll back first".
   */
  | { kind: 'openTransaction'; message: string }
  | { kind: 'noDriver'; engine: Engine };

/** `CatalogError` in crates/catalog/src/model.rs. */
export type CatalogError =
  | { kind: 'driver'; error: DriverError }
  | { kind: 'notFound' | 'unsupported' | 'invalid' | 'unexpected'; message: string }
  /**
   * EXPLAIN ANALYZE needs confirmation. The app turns this into a session
   * `confirmationRequired` before it reaches the UI; typed for completeness.
   */
  | { kind: 'needsConfirmation'; message: string; warnings: string[] };

/** `LiveError` in crates/live-stats/src/sql/mod.rs. */
export type LiveError =
  | { kind: 'permissionDenied'; message: string; code: string | null }
  | { kind: 'ownConnection'; id: number }
  | { kind: 'driver'; error: DriverError }
  | { kind: 'notFound' | 'unsupported' | 'invalid' | 'unexpected'; message: string };

/** `CmdError` in app/src-tauri/src/commands.rs. */
export type CmdError =
  | { kind: 'invalid'; message: string }
  | { kind: 'notFound'; message: string }
  | { kind: 'session'; error: SessionError }
  | {
      kind: 'keychain';
      message: string;
      /**
       * Set by `update_connection` when the edit was stored and only removing
       * old keychain entries failed: the profile exactly as that call stored it.
       */
      profile?: ConnectionProfile;
    }
  | {
      kind: 'tunnel';
      message: string;
      code: string;
      fingerprints: string[];
      presentedKey: string | null;
      /** SSH host and port that presented the key (host key errors only). */
      host: string | null;
      port: number | null;
    }
  | { kind: 'secretRequired'; secret: SecretKindName }
  /**
   * The tab's connection was edited to another server, account or settings
   * since the tab connected: browser and panel calls refuse. Offer "Reconnect".
   */
  | { kind: 'reconnectRequired'; message: string }
  | { kind: 'readOnly'; message: string }
  | { kind: 'catalog'; error: CatalogError }
  | { kind: 'live'; error: LiveError }
  | { kind: 'internal'; message: string };

/** A failed backend command. `call` rejects with this, so callers always get an `Error`. */
export class CommandFailure extends Error {
  readonly cmd: CmdError;

  constructor(cmd: CmdError) {
    super(errorMessage(cmd));
    this.name = 'CommandFailure';
    this.cmd = cmd;
  }
}

type Obj = Record<string, unknown>;

function isObj(value: unknown): value is Obj {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isStr(value: unknown): value is string {
  return typeof value === 'string';
}

function isOptStr(value: unknown): boolean {
  return value === undefined || value === null || typeof value === 'string';
}

const DURABILITIES: readonly string[] = ['committed', 'rolled_back', 'in_transaction', 'failed', 'unknown'];

/** The shape of a `PartialOutput`; the result rows are not inspected. */
export function isPartialOutput(value: unknown): value is PartialOutput {
  return (
    isObj(value) &&
    Array.isArray(value.results) &&
    Array.isArray(value.notices) &&
    typeof value.elapsedMs === 'number' &&
    (value.failedStatement === null || typeof value.failedStatement === 'number') &&
    Array.isArray(value.completed) &&
    value.completed.every((d) => isStr(d) && DURABILITIES.includes(d))
  );
}

const DRIVER_KINDS: readonly string[] = [
  'connect',
  'auth',
  'tls',
  'timeout',
  'cancelled',
  'query',
  'read_only',
  'unsupported',
  'protocol',
  'connection_lost',
];
const SECRET_KINDS: readonly string[] = ['db_password', 'ssh_password', 'ssh_key_passphrase', 'tls_key_passphrase'];
const ENGINES: readonly string[] = ['postgres', 'mysql', 'mariadb', 'redis'];
const ENVIRONMENTS: readonly string[] = ['development', 'test', 'staging', 'production'];

export function isDriverError(value: unknown): value is DriverError {
  return (
    isObj(value) &&
    isStr(value.kind) &&
    DRIVER_KINDS.includes(value.kind) &&
    isStr(value.message) &&
    isOptStr(value.code) &&
    isOptStr(value.detail) &&
    isOptStr(value.hint) &&
    (value.position === undefined || value.position === null || typeof value.position === 'number') &&
    (value.partial === undefined || value.partial === null || isPartialOutput(value.partial))
  );
}

export function isSessionError(value: unknown): value is SessionError {
  if (!isObj(value) || !isStr(value.kind)) return false;
  switch (value.kind) {
    case 'driver':
      return isDriverError(value.error);
    case 'notFound':
      return isStr(value.sessionId);
    case 'confirmationRequired':
      return (
        isStr(value.environment) &&
        ENVIRONMENTS.includes(value.environment) &&
        Array.isArray(value.statements) &&
        value.statements.every(isStr)
      );
    case 'openTransaction':
      return isStr(value.message);
    case 'noDriver':
      return isStr(value.engine) && ENGINES.includes(value.engine);
    default:
      return false;
  }
}

const MESSAGE_KINDS: readonly string[] = ['notFound', 'unsupported', 'invalid', 'unexpected'];

export function isCatalogError(value: unknown): value is CatalogError {
  if (!isObj(value) || !isStr(value.kind)) return false;
  if (value.kind === 'driver') return isDriverError(value.error);
  if (value.kind === 'needsConfirmation') return isStr(value.message) && Array.isArray(value.warnings) && value.warnings.every(isStr);
  return MESSAGE_KINDS.includes(value.kind) && isStr(value.message);
}

export function isLiveError(value: unknown): value is LiveError {
  if (!isObj(value) || !isStr(value.kind)) return false;
  if (value.kind === 'permissionDenied') return isStr(value.message) && isOptStr(value.code);
  if (value.kind === 'ownConnection') return typeof value.id === 'number';
  if (value.kind === 'driver') return isDriverError(value.error);
  return MESSAGE_KINDS.includes(value.kind) && isStr(value.message);
}

/** True when `value` has the exact shape of a `CmdError` from the backend. */
export function isCmdError(value: unknown): value is CmdError {
  if (!isObj(value) || !isStr(value.kind)) return false;
  switch (value.kind) {
    case 'invalid':
    case 'notFound':
    case 'keychain':
    case 'readOnly':
    case 'reconnectRequired':
    case 'internal':
      return isStr(value.message);
    case 'catalog':
      return isCatalogError(value.error);
    case 'live':
      return isLiveError(value.error);
    case 'session':
      return isSessionError(value.error);
    case 'tunnel':
      return (
        isStr(value.message) &&
        isStr(value.code) &&
        Array.isArray(value.fingerprints) &&
        value.fingerprints.every(isStr) &&
        isOptStr(value.presentedKey) &&
        isOptStr(value.host) &&
        (value.port === undefined || value.port === null || typeof value.port === 'number')
      );
    case 'secretRequired':
      return isStr(value.secret) && SECRET_KINDS.includes(value.secret);
    default:
      return false;
  }
}

/**
 * Turns whatever a command rejected with into a `CmdError`. Anything that is
 * not a backend error (a string from Tauri, a thrown `Error`) becomes `internal`.
 */
export function toCmdError(value: unknown): CmdError {
  if (value instanceof CommandFailure) return value.cmd;
  if (isCmdError(value)) {
    if (value.kind === 'tunnel') {
      return { ...value, presentedKey: value.presentedKey ?? null, host: value.host ?? null, port: value.port ?? null };
    }
    if (value.kind === 'live' && value.error.kind === 'permissionDenied') {
      return { kind: 'live', error: { ...value.error, code: value.error.code ?? null } };
    }
    if (value.kind === 'session' && value.error.kind === 'driver') {
      return { kind: 'session', error: { kind: 'driver', error: normalizeDriverError(value.error.error) } };
    }
    if (value.kind === 'catalog' && value.error.kind === 'driver') {
      return { kind: 'catalog', error: { kind: 'driver', error: normalizeDriverError(value.error.error) } };
    }
    if (value.kind === 'live' && value.error.kind === 'driver') {
      return { kind: 'live', error: { kind: 'driver', error: normalizeDriverError(value.error.error) } };
    }
    return value;
  }
  if (value instanceof Error) return { kind: 'internal', message: value.message };
  if (isStr(value)) return { kind: 'internal', message: value };
  if (isObj(value) && isStr(value.message)) return { kind: 'internal', message: value.message };
  return { kind: 'internal', message: 'Something went wrong in the app backend.' };
}

/** A driver error with every optional field present (`null` when empty). */
function normalizeDriverError(e: DriverError): DriverError {
  return {
    kind: e.kind,
    message: e.message,
    code: e.code ?? null,
    detail: e.detail ?? null,
    hint: e.hint ?? null,
    position: e.position ?? null,
    partial: e.partial ?? null,
  };
}

/**
 * Driver error codes the UI reacts to. `MULTI_OPEN`: a Redis console `MULTI`
 * block is open, so the key browser and database switch refuse (run EXEC or
 * DISCARD first). `TRANSACTION_RESET`: a Redis reconnect dropped an open
 * `MULTI` or `WATCH`; the refused line is named in the message and `position`.
 */
export const DRIVER_CODES = {
  multiOpen: 'MULTI_OPEN',
  transactionReset: 'TRANSACTION_RESET',
  cursorExpired: 'CURSOR_EXPIRED',
} as const;

/** True when the error is `reconnectRequired`: the tab must reconnect before browser and panel calls work again. */
export function isReconnectRequired(error: CmdError): boolean {
  return error.kind === 'reconnectRequired';
}

/** The `openTransaction` message when the call was refused because of the session's open transaction. */
export function openTransactionOf(error: CmdError): string | null {
  return error.kind === 'session' && error.error.kind === 'openTransaction' ? error.error.message : null;
}

/** The driver error inside a session, catalog or live error, if any. */
export function anyDriverErrorOf(error: CmdError): DriverError | null {
  if (error.kind === 'session' && error.error.kind === 'driver') return error.error.error;
  if ((error.kind === 'catalog' || error.kind === 'live') && error.error.kind === 'driver') return error.error.error;
  return null;
}

/** The driver error inside a session error, if any. */
export function driverErrorOf(error: CmdError): DriverError | null {
  return error.kind === 'session' && error.error.kind === 'driver' ? error.error.error : null;
}

const SECRET_LABELS: Record<SecretKindName, string> = {
  db_password: 'database password',
  ssh_password: 'SSH password',
  ssh_key_passphrase: 'SSH key passphrase',
  tls_key_passphrase: 'TLS key passphrase',
};

export function secretLabel(kind: SecretKindName): string {
  return SECRET_LABELS[kind];
}

function errorMessage(error: CmdError): string {
  return presentError(error).message;
}

/** What the UI shows for an error. */
export interface PresentedError {
  title: string;
  message: string;
  code?: string;
  detail?: string;
  hint?: string;
}

const DRIVER_TITLES: Record<DriverErrorKind, string> = {
  connect: 'Could not connect',
  auth: 'Authentication failed',
  tls: 'TLS failed',
  timeout: 'Timed out',
  cancelled: 'Cancelled',
  query: 'Query failed',
  read_only: 'Read-only connection',
  unsupported: 'Not supported',
  protocol: 'Protocol error',
  connection_lost: 'Connection lost',
};

function presentNested(error: CatalogError | Exclude<LiveError, { kind: 'permissionDenied' | 'ownConnection' }>, title: string): PresentedError {
  if (error.kind === 'driver') return presentError({ kind: 'session', error: { kind: 'driver', error: error.error } });
  if (error.kind === 'needsConfirmation') {
    return { title: 'Confirmation needed', message: error.warnings.length > 0 ? error.warnings.join(' ') : error.message };
  }
  return { title: error.kind === 'unsupported' ? 'Not supported' : title, message: error.message };
}

/** Title, message and extra lines for an error, for toasts, panels and dialogs. */
export function presentError(error: CmdError): PresentedError {
  switch (error.kind) {
    case 'invalid':
      return { title: 'Check the input', message: error.message };
    case 'notFound':
      return { title: 'Not found', message: error.message };
    case 'keychain':
      return { title: 'Keychain error', message: error.message };
    case 'readOnly':
      return { title: 'Read-only connection', message: error.message };
    case 'reconnectRequired':
      return { title: 'Reconnect needed', message: error.message };
    case 'catalog':
      return presentNested(error.error, 'Could not read the database objects');
    case 'live':
      if (error.error.kind === 'permissionDenied') {
        const out: PresentedError = { title: 'Permission denied', message: error.error.message };
        if (error.error.code) out.code = error.error.code;
        return out;
      }
      if (error.error.kind === 'ownConnection') {
        return { title: 'Not possible here', message: `Connection ${error.error.id} is this panel's own connection and cannot be killed from here.` };
      }
      return presentNested(error.error, 'The server panel failed');
    case 'internal':
      return { title: 'Something went wrong', message: error.message };
    case 'secretRequired':
      return { title: 'Secret needed', message: `Enter the ${secretLabel(error.secret)} to continue.` };
    case 'tunnel':
      return { title: 'SSH tunnel failed', message: error.message, code: error.code };
    case 'session': {
      const s = error.error;
      switch (s.kind) {
        case 'notFound':
          return { title: 'Session closed', message: 'This session is no longer open. Reconnect to continue.' };
        case 'noDriver':
          return { title: 'No driver', message: `This app has no driver for ${s.engine}.` };
        case 'openTransaction':
          return { title: 'Transaction open', message: s.message };
        case 'confirmationRequired':
          return {
            title: 'Confirmation needed',
            message: `This script contains destructive statements (${s.statements.join(', ')}).`,
          };
        case 'driver': {
          const d = s.error;
          const out: PresentedError = { title: DRIVER_TITLES[d.kind], message: d.message };
          if (d.code) out.code = d.code;
          if (d.detail) out.detail = d.detail;
          if (d.hint) out.hint = d.hint;
          return out;
        }
      }
    }
  }
}
