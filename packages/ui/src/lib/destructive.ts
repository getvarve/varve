import type { Environment } from '../api/records';
import { CommandFailure, toCmdError } from '../api/errors';

/** What the destructive confirmation dialog shows. */
export interface ConfirmationRequest {
  environment: Environment;
  statements: string[];
}

const LABELS: Record<string, string> = {
  UpdateAll: 'UPDATE without WHERE',
  DeleteAll: 'DELETE without WHERE',
  AlterDrop: 'ALTER ... DROP',
  Drop: 'DROP',
  Truncate: 'TRUNCATE',
  Delete: 'DELETE',
  Update: 'UPDATE',
  Insert: 'INSERT',
  Merge: 'MERGE',
  Ddl: 'DDL',
  Grant: 'GRANT or REVOKE',
  Select: 'SELECT',
  Transaction: 'transaction control',
  Explain: 'EXPLAIN',
  Session: 'session setting',
  Call: 'CALL',
  Copy: 'COPY',
  Maintenance: 'maintenance (VACUUM, ANALYZE, ...)',
  Other: 'other statement',
  // ddl_apply: the DDL runs while its database's default collation is changed.
  AlterDatabase: "ALTER DATABASE (the database's default collation, changed and set back)",
};

/**
 * Readable statement kinds with counts, in first-seen order. SQL kinds come
 * as Rust names (`DeleteAll`), Redis kinds as command names (`FLUSHALL`).
 */
export function describeStatements(statements: readonly string[]): { label: string; count: number }[] {
  const out: { label: string; count: number }[] = [];
  for (const kind of statements) {
    const label = LABELS[kind] ?? kind;
    const hit = out.find((o) => o.label === label);
    if (hit) hit.count += 1;
    else out.push({ label, count: 1 });
  }
  return out;
}

/** Production asks the user to type the connection name before Run is enabled. */
export function needsTypedName(environment: Environment): boolean {
  return environment === 'production';
}

/** Whether Run is enabled in the confirmation dialog. */
export function canConfirm(environment: Environment, connectionName: string, typed: string): boolean {
  if (!needsTypedName(environment)) return true;
  return typed.trim() !== '' && typed.trim() === connectionName.trim();
}

/** The confirmation request inside a failed call, if it is one. */
export function confirmationOf(error: unknown): ConfirmationRequest | null {
  const cmd = toCmdError(error);
  if (cmd.kind === 'session' && cmd.error.kind === 'confirmationRequired') {
    return { environment: cmd.error.environment, statements: cmd.error.statements };
  }
  return null;
}

/**
 * Runs `run(false)`. When the backend answers that the script needs
 * confirmation, asks with `confirm` and runs `run(true)` only after a yes.
 * Resolves `null` when the user declines; other errors reject unchanged.
 */
export async function runWithConfirmation<T>(
  run: (confirmed: boolean) => Promise<T>,
  confirm: (request: ConfirmationRequest) => Promise<boolean>,
): Promise<T | null> {
  try {
    return await run(false);
  } catch (error: unknown) {
    const request = confirmationOf(error);
    if (!request) throw error instanceof CommandFailure ? error : new CommandFailure(toCmdError(error));
    if (!(await confirm(request))) return null;
    return run(true);
  }
}
