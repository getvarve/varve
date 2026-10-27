import { CommandFailure, anyDriverErrorOf, presentError, toCmdError } from '../api/errors';
import type { CmdError, PresentedError } from '../api/errors';
import type { Environment } from '../api/records';

/** What the EXPLAIN ANALYZE confirmation dialog lists. */
export interface ExplainConfirmation {
  /** `null` when the catalog asked directly (no environment attached). */
  environment: Environment | null;
  /** Every reason, in the backend's order: the first says the statement runs for real. */
  reasons: string[];
}

/**
 * The confirmation an `explain` call asks for, if it is one. The app sends
 * `confirmationRequired` whose `statements` are the reasons; the catalog's own
 * `needsConfirmation` (message plus warnings) is accepted as well.
 */
export function explainConfirmationOf(error: unknown): ExplainConfirmation | null {
  const cmd = toCmdError(error);
  if (cmd.kind === 'session' && cmd.error.kind === 'confirmationRequired') {
    return { environment: cmd.error.environment, reasons: [...cmd.error.statements] };
  }
  if (cmd.kind === 'catalog' && cmd.error.kind === 'needsConfirmation') {
    return { environment: null, reasons: [cmd.error.message, ...cmd.error.warnings] };
  }
  return null;
}

/** The outcome of an explain run as the panel shows it. */
export type ExplainOutcome<T> =
  | { status: 'done'; value: T }
  | { status: 'declined' }
  | { status: 'cancelled' }
  | { status: 'failed'; error: CmdError };

/** True when the error is the driver's "cancelled" (the tab's Cancel stopped the run). */
export function isCancelled(error: CmdError): boolean {
  return anyDriverErrorOf(error)?.kind === 'cancelled';
}

/**
 * Runs `run(false)`; when the backend asks for confirmation, asks with
 * `confirm` (listing every reason) and runs `run(true)` only after a yes.
 * Never throws: failures, cancels and a declined dialog come back as outcomes.
 */
export async function runExplain<T>(
  run: (confirmed: boolean) => Promise<T>,
  confirm: (request: ExplainConfirmation) => Promise<boolean>,
): Promise<ExplainOutcome<T>> {
  const failure = (error: unknown): ExplainOutcome<T> => {
    const cmd = error instanceof CommandFailure ? error.cmd : toCmdError(error);
    return isCancelled(cmd) ? { status: 'cancelled' } : { status: 'failed', error: cmd };
  };
  let request: ExplainConfirmation | null;
  try {
    return { status: 'done', value: await run(false) };
  } catch (error: unknown) {
    request = explainConfirmationOf(error);
    if (!request) return failure(error);
  }
  if (!(await confirm(request))) return { status: 'declined' };
  try {
    return { status: 'done', value: await run(true) };
  } catch (error: unknown) {
    // A second request for confirmation after a yes is shown as the failure it is, never asked again.
    return failure(error);
  }
}

/** The error as the statement's own error: message plus code, detail, hint and the character position. */
export function presentExplainError(error: CmdError): PresentedError & { position: number | null } {
  const presented = presentError(error);
  const driver = anyDriverErrorOf(error);
  return { ...presented, position: driver?.position ?? null };
}
