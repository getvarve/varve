import { call } from './invoke';
import type { SecretKindName } from './records';

/**
 * `PendingRestore` in app/src-tauri/src/m2/mod.rs: a database whose default
 * collation a DDL run left changed and could not set back.
 */
export interface PendingRestore {
  /**
   * The server as it identifies itself, without the route: `mysql:<server_uuid>`
   * or `mariadb:<server facts>`. Entries of very old versions hold `host:port`
   * and can only be dismissed.
   */
  server: string;
  /** The route the run reached the server through (`host:port`, plus ` via bastion:port`). */
  route: string;
  database: string;
  /** The database's own default collation, to restore. */
  collation: string;
  /** Every collation a run left the database at. */
  temporaries: string[];
  /** False when the server identity can repeat across servers (MariaDB): restoring then asks first. */
  verified: boolean;
  /** The profile that recorded the entry. */
  connectionId: string;
  /** Unix milliseconds; 0 for entries of older versions. */
  recordedAt: number;
}

/** `PendingDeletion` in app/src-tauri/src/keychain_cleanup.rs: a keychain entry still to be removed. */
export interface PendingDeletion {
  connectionId: string;
  kind: SecretKindName;
}

/** Databases whose collation is still to be restored. */
export function ddlPendingRestores(): Promise<PendingRestore[]> {
  return call<PendingRestore[]>('ddl_pending_restores');
}

/**
 * Sets the database back to its own collation through `sessionId`. Rejects with
 * `confirmationRequired` when Varve cannot prove the session reaches the server
 * the entry was recorded on; call again with `confirmed: true` after a yes.
 */
export async function ddlRestoreCollation(sessionId: string, database: string, confirmed: boolean): Promise<void> {
  await call<unknown>('ddl_restore_collation', { sessionId, database, confirmed });
}

/** Forgets a pending restore without touching any server. */
export async function ddlDismissPendingRestore(server: string, database: string): Promise<void> {
  await call<unknown>('ddl_dismiss_pending_restore', { server, database });
}

/** Keychain entries Varve could not remove yet. */
export function pendingKeychainDeletions(): Promise<PendingDeletion[]> {
  return call<PendingDeletion[]>('pending_keychain_deletions');
}

/** Retries removing them now; returns one message per entry that still fails. */
export function retryKeychainCleanup(): Promise<string[]> {
  return call<string[]>('retry_keychain_cleanup');
}
