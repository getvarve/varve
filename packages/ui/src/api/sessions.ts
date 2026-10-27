import { call } from './invoke';
import type { ConnectionInput, Engine, Environment, TlsMode } from './records';

/** `ServerInfo` in crates/driver/src/lib.rs. */
export interface ServerInfo {
  engine: Engine;
  version: string;
  major: number;
  minor: number;
  patch: number | null;
}

/** `SessionPurpose` in app/src-tauri/src/sessions.rs. */
export type SessionPurpose = 'user' | 'metadata' | 'admin';

/** `SessionInfo` in app/src-tauri/src/sessions.rs. */
export interface SessionInfo {
  id: string;
  connectionId: string;
  engine: Engine;
  server: ServerInfo;
  environment: Environment;
  /** Database (PostgreSQL) or default schema (MySQL, MariaDB) of the session; `null` for Redis or none. */
  database: string | null;
  readOnly: boolean;
  /** Destructive statements ask for confirmation on this session. */
  confirmDestructive: boolean;
  /** Who uses the session; the UI only ever sees `user` sessions (`sessions` lists only those). */
  purpose: SessionPurpose;
  openedAt: number;
  /** Whether the connection is encrypted, as connected. */
  encryption: Encryption;
}

/** `Encryption` in app/src-tauri/src/sessions.rs. */
export interface Encryption {
  /** The connection uses TLS; `null` when the driver cannot tell. */
  tls: boolean | null;
  /** The profile's TLS mode. */
  mode: TlsMode;
  /** A Redis password went to the server unencrypted (`prefer` settled on plain TCP). */
  passwordUnencrypted: boolean;
}

/** `Value` in crates/driver/src/value.rs, tagged `{ t, v }` in camelCase. */
export type Value =
  | { t: 'null' }
  | { t: 'bool'; v: boolean }
  /** A number within +-(2^53 - 1), otherwise the exact decimal digits as a string. */
  | { t: 'int'; v: number | string }
  | { t: 'uInt'; v: number | string }
  /** A number, or `"NaN"`, `"Infinity"`, `"-Infinity"`. */
  | { t: 'float'; v: number | 'NaN' | 'Infinity' | '-Infinity' }
  | { t: 'decimal'; v: string }
  | { t: 'text'; v: string }
  | { t: 'bytes'; v: string }
  | { t: 'date'; v: string }
  | { t: 'time'; v: string }
  | { t: 'timestamp'; v: string }
  | { t: 'timestampTz'; v: string }
  | { t: 'interval'; v: string }
  | { t: 'uuid'; v: string }
  /** The JSON document as the server sent it, as text (exact numbers, key order). */
  | { t: 'json'; v: string }
  | { t: 'array'; v: Value[] }
  | { t: 'other'; v: string };

export interface Column {
  name: string;
  typeName: string;
}

export type StatementResult =
  | { kind: 'rows'; columns: Column[]; rows: Value[][]; truncated: boolean }
  | { kind: 'command'; tag: string; rowsAffected: number | null };

export interface Notice {
  severity: string;
  message: string;
}

export interface ExecOutput {
  results: StatementResult[];
  notices: Notice[];
  elapsedMs: number;
}

/** `Durability` in crates/driver/src/partial.rs: what became of a statement that completed before its script failed. */
export type Durability = 'committed' | 'rolled_back' | 'in_transaction' | 'failed' | 'unknown';

/** `PartialOutput` in crates/driver/src/partial.rs: what a failed script produced before it failed. */
export interface PartialOutput {
  /** Results of the completed statements, then any the failing statement sent before it failed. */
  results: StatementResult[];
  notices: Notice[];
  elapsedMs: number;
  /** 0-based index of the first statement that did not complete; `null` when the driver cannot tell. */
  failedStatement: number | null;
  /** One entry per completed statement, in order. */
  completed: Durability[];
}

export interface TestResult {
  server: ServerInfo;
  latencyMs: number;
}

/**
 * What to test. `basedOn` names the saved profile being edited, so the
 * backend may reuse its keychain secrets while the server and account are unchanged.
 */
export type TestTarget =
  | { kind: 'saved'; id: string }
  | { kind: 'unsaved'; input: ConnectionInput; basedOn?: string };

/** Secrets typed in a prompt or form, passed to one call and never stored by this layer. */
export interface TypedSecrets {
  password?: string;
  sshSecret?: string;
  /** Passphrase of an encrypted TLS client key. */
  tlsPassphrase?: string;
}

export function testConnection(target: TestTarget, secrets: TypedSecrets = {}): Promise<TestResult> {
  return call<TestResult>('test_connection', {
    target,
    password: secrets.password ?? null,
    sshSecret: secrets.sshSecret ?? null,
    tlsPassphrase: secrets.tlsPassphrase ?? null,
  });
}

export function openSession(id: string, secrets: TypedSecrets = {}): Promise<SessionInfo> {
  return call<SessionInfo>('open_session', {
    id,
    password: secrets.password ?? null,
    sshSecret: secrets.sshSecret ?? null,
    tlsPassphrase: secrets.tlsPassphrase ?? null,
  });
}

export function execute(sessionId: string, sql: string, confirmed: boolean, maxRows?: number): Promise<ExecOutput> {
  return call<ExecOutput>('execute', { sessionId, sql, confirmed, maxRows: maxRows ?? null });
}

export async function cancel(sessionId: string): Promise<void> {
  await call<unknown>('cancel', { sessionId });
}

export async function closeSession(sessionId: string): Promise<void> {
  await call<unknown>('close_session', { sessionId });
}

export function listSessions(): Promise<SessionInfo[]> {
  return call<SessionInfo[]>('sessions');
}

/** `ReloadedSession` in app/src-tauri/src/sessions.rs. */
export interface ReloadedSession {
  connectionId: string;
  /** A transaction was open and the close rolled it back; `null` when a statement was still running. */
  inTransaction: boolean | null;
}

/** `ReloadReport`: what reloading the window closed. */
export interface ReloadReport {
  sessions: ReloadedSession[];
  /** Profiles of sessions that were still opening and were closed once open. */
  opening: string[];
}

/** What the last reload of the window closed; read once by the new page. */
export function reloadReport(): Promise<ReloadReport> {
  return call<ReloadReport>('reload_report');
}

/** Profiles of user sessions with a transaction open now (one entry per session). */
export function openTransactions(): Promise<string[]> {
  return call<string[]>('open_transactions');
}

/**
 * Opens a user session of a PostgreSQL profile on another database, to browse
 * that database (same prompts and tunnel as `openSession`).
 */
export function openDatabaseSession(id: string, database: string, secrets: TypedSecrets = {}): Promise<SessionInfo> {
  return call<SessionInfo>('open_database_session', {
    id,
    database,
    password: secrets.password ?? null,
    sshSecret: secrets.sshSecret ?? null,
    tlsPassphrase: secrets.tlsPassphrase ?? null,
  });
}
