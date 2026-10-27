import { anyDriverErrorOf, DRIVER_CODES, openTransactionOf, presentError } from '../api/errors';
import type { CmdError, DriverError, PresentedError } from '../api/errors';

/** What a Redis view offers after an error. */
export type RedisErrorAction = 'reconnect' | 'retry' | null;

/** A Redis error as a Redis view shows it. */
export interface RedisErrorView {
  title: string;
  message: string;
  /** What to do about it, when there is something specific. */
  hint: string | null;
  action: RedisErrorAction;
  /** True when the server or session refused before anything ran. */
  refused: boolean;
}

/**
 * Commands a read-only Redis connection refuses although the server flags
 * them read-only, with the reason (crates/driver-redis `READ_ONLY_DENY` and
 * the `@admin` / `@dangerous` rule).
 */
const READ_ONLY_REASONS: Record<string, string> = {
  KEYS: 'KEYS is refused on read-only connections: it blocks the whole server while it walks every key. Use the key browser, or SCAN with MATCH, instead.',
  SORT_RO:
    'SORT_RO is refused on read-only connections: the server flags it @dangerous (its BY and GET options read other keys). Read the values with LRANGE, SMEMBERS or ZRANGE instead.',
  EVAL_RO: 'Read-only scripts are refused on read-only connections: a script can still run admin commands that do not write data.',
  EVALSHA_RO: 'Read-only scripts are refused on read-only connections: a script can still run admin commands that do not write data.',
  FCALL_RO: 'Read-only functions are refused on read-only connections: a function can still run admin commands that do not write data.',
};

/** The command a read-only refusal names (`Line 2: KEYS changes data ...`). */
export function refusedCommand(message: string): string | null {
  const match = /^(?:Line \d+: )?([A-Za-z][\w.|-]*)/.exec(message);
  return match?.[1] ? match[1].toUpperCase() : null;
}

/** The hint for a read-only refusal of a Redis command, if Varve has a specific one. */
export function readOnlyHint(message: string): string | null {
  const command = refusedCommand(message);
  return command ? (READ_ONLY_REASONS[command] ?? null) : null;
}

/**
 * Turns any error of a Redis call into what the view shows: a console
 * `MULTI` block that blocks the browser, a lost `WATCH`/`MULTI`, read-only
 * refusals with their reason, a profile edited to another server.
 */
export function redisErrorView(error: CmdError): RedisErrorView {
  const presented = presentError(error);
  if (error.kind === 'reconnectRequired') {
    return { title: 'Reconnect needed', message: error.message, hint: null, action: 'reconnect', refused: true };
  }
  if (error.kind === 'readOnly') {
    return { title: 'Read-only connection', message: error.message, hint: readOnlyHint(error.message), action: null, refused: true };
  }
  if (error.kind === 'notFound') {
    // A plan or record that no longer exists: refused before anything ran.
    return { title: presented.title, message: presented.message, hint: presented.hint ?? null, action: null, refused: true };
  }
  const openTx = openTransactionOf(error);
  if (openTx !== null) {
    return { title: 'Transaction open', message: openTx, hint: 'Commit or roll back first.', action: 'retry', refused: true };
  }
  const driver = anyDriverErrorOf(error);
  if (driver?.code === DRIVER_CODES.multiOpen) {
    return {
      title: 'MULTI open in the console',
      message: driver.message,
      hint: 'Run EXEC or DISCARD in the console tab of this connection, then try again.',
      action: 'retry',
      refused: true,
    };
  }
  if (driver?.code === 'WATCH_OPEN') {
    return {
      title: 'WATCH open in the console',
      message: driver.message,
      hint: 'Run EXEC, DISCARD or UNWATCH in the console tab that holds the WATCH, then open the key again. Reading a time series with TS.INFO ends a WATCH on that key for every client, including other programs.',
      action: 'retry',
      refused: true,
    };
  }
  if (driver?.code === DRIVER_CODES.transactionReset) {
    return {
      title: 'Transaction state lost',
      message: driver.message,
      hint: 'The connection was replaced, so the server forgot the WATCH or MULTI. Start the transaction again: WATCH (or UNWATCH), MULTI, then the commands.',
      action: null,
      refused: true,
    };
  }
  if (driver?.code === 'NOPERM') {
    // An ACL refusal before anything ran (for example a user without EVAL for the group checks).
    return {
      title: 'No permission',
      message: driver.message,
      hint: driver.hint ?? 'Your ACL user may not run this command. Ask for the permission, or run the change as another user.',
      action: null,
      refused: true,
    };
  }
  if (driver?.kind === 'read_only') {
    return { title: 'Read-only connection', message: driver.message, hint: readOnlyHint(driver.message), action: null, refused: true };
  }
  const transaction = driver ? transactionHint(driver) : null;
  if (driver && transaction) {
    return { title: transaction.title, message: driver.message, hint: transaction.hint, action: null, refused: transaction.refused };
  }
  return { title: presented.title, message: presented.message, hint: presented.hint ?? presented.detail ?? null, action: null, refused: false };
}

/**
 * Hints for the server's own transaction errors in the console: `EXEC` or
 * `DISCARD` without `MULTI`, a nested `MULTI`, `WATCH` inside `MULTI`, and
 * `EXECABORT` (a queued command was refused, so nothing ran).
 */
export function transactionHint(driver: DriverError): { title: string; hint: string; refused: boolean } | null {
  const text = driver.message.toLowerCase();
  if (driver.code === 'EXECABORT' || text.includes('execabort')) {
    return {
      title: 'Transaction discarded',
      hint: 'The server refused a command while it was queued (a wrong command name or argument count), so EXEC ran nothing. Fix that command and run MULTI ... EXEC again.',
      refused: true,
    };
  }
  if (text.includes('exec without multi') || text.includes('discard without multi')) {
    return {
      title: 'No transaction open',
      hint: 'EXEC and DISCARD end a transaction that MULTI started. Run MULTI first, queue the commands, then EXEC (or DISCARD to drop them).',
      refused: true,
    };
  }
  if (text.includes('multi calls can not be nested')) {
    return {
      title: 'MULTI already open',
      hint: 'A transaction is already open on this connection. Queue the commands, then run EXEC to run them or DISCARD to drop them.',
      refused: true,
    };
  }
  if (text.includes('watch inside multi')) {
    return {
      title: 'WATCH inside MULTI',
      hint: 'WATCH has to come before MULTI. Run DISCARD, then WATCH the keys, then MULTI and the commands.',
      refused: true,
    };
  }
  return null;
}

/**
 * The error of a Redis console run as the results panel shows it: the
 * {@link redisErrorView} title and hint (read-only refusals of KEYS,
 * SORT_RO, EVAL_RO, MULTI and WATCH states), with the code and detail of
 * the plain presentation.
 */
export function presentRedisFailure(error: CmdError): PresentedError {
  const presented = presentError(error);
  const view = redisErrorView(error);
  const out: PresentedError = { title: view.title, message: view.message };
  if (presented.code) out.code = presented.code;
  if (presented.detail) out.detail = presented.detail;
  const hint = view.hint ?? presented.hint;
  if (hint) out.hint = hint;
  return out;
}
