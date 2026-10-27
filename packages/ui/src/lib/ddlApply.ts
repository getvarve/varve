/**
 * Running object editor plans and DDL text (M2 contract, "Object editors"
 * and "Running DDL from the browser"): steps run in order, each through the
 * destructive confirmation flow, and stop at the first failure. Errors are
 * turned into what the editor shows and which button it offers.
 */
import { anyDriverErrorOf, openTransactionOf, presentError, toCmdError } from '../api/errors';
import type { CmdError } from '../api/errors';
import type { DdlApplied, EditApplied, EditPlan, EngineChange } from '../api/ddl';
import { runWithConfirmation } from './destructive';
import type { ConfirmationRequest } from './destructive';

/** One step of an editor's save: a typed change planned on the server, or catalog DDL text run as is. */
export type StepSpec =
  | { kind: 'edit'; change: EngineChange }
  | {
      kind: 'ddl';
      /** DDL text from the catalog (for example index definitions of a recreated materialized view). */
      ddl: string;
      database: string | null;
      label: string;
    };

export type StepStatus = 'pending' | 'done' | 'failed';

export interface PlannedStep {
  spec: StepSpec;
  /** Server plan id (edit steps). */
  planId: string | null;
  /** The SQL the step runs; for DDL steps the text itself. */
  plan: EditPlan;
  status: StepStatus;
}

/** What the editor offers after a failure. */
export type ProblemAction =
  /** Nothing to do from here (read-only, invalid input). */
  | 'none'
  /** Run the same plans again (they were kept: open transaction). */
  | 'retry'
  /** Plan the remaining steps again (the plan was used or expired, or failed); the user reviews and applies. */
  | 'replan'
  /**
   * Plan the steps that did not run again and run them at once: the server
   * refused because the database was in use, nothing changed, and the user
   * already reviewed this change (confirmations are asked again as needed).
   */
  | 'rerun'
  /** The tab must reconnect first. */
  | 'reconnect'
  /** The object is gone: refresh the tree. */
  | 'refresh';

export interface ApplyProblem {
  title: string;
  message: string;
  code?: string;
  detail?: string;
  hint?: string;
  action: ProblemAction;
  /** Label of the action button. */
  actionLabel: string;
}

/** PostgreSQL SQLSTATE `object_in_use` (`DROP DATABASE` while sessions are connected). */
const OBJECT_IN_USE = '55006';
/** MySQL and MariaDB `ER_LOCK_WAIT_TIMEOUT`. */
const LOCK_WAIT_TIMEOUT = '1205';

/** The MySQL error number of a driver error code (`1205` or `1205 (HY000)`). */
function mysqlErrorNumber(code: string | null): string | null {
  return code?.trim().split(/\s+/)[0] ?? null;
}

/** The editor's view of a failed plan, apply or DDL run. */
export function applyProblem(error: unknown): ApplyProblem {
  const cmd: CmdError = toCmdError(error);
  const presented = presentError(cmd);
  const base: ApplyProblem = { ...presented, action: 'none', actionLabel: '' };
  const open = openTransactionOf(cmd);
  if (open !== null) {
    return {
      title: 'Commit or roll back first',
      message: `${open} Nothing ran; the plan is kept. End the transaction in this tab's console, then try again.`,
      action: 'retry',
      actionLabel: 'Try again',
    };
  }
  switch (cmd.kind) {
    case 'readOnly':
      return { title: 'Read-only connection', message: `${cmd.message} Nothing ran.`, action: 'none', actionLabel: '' };
    case 'notFound':
      return {
        title: 'Plan no longer available',
        message: 'The plan expired, was already used or belongs to another session. Plan the change again and review it before applying.',
        action: 'replan',
        actionLabel: 'Plan again',
      };
    case 'reconnectRequired':
      return { ...base, action: 'reconnect', actionLabel: 'Reconnect' };
    case 'catalog':
      if (cmd.error.kind === 'notFound') return { ...base, title: 'Object not found', action: 'refresh', actionLabel: 'Refresh' };
      if (cmd.error.kind === 'invalid') return { ...base, title: 'Check the editor', action: 'none', actionLabel: '' };
      break;
    default:
      break;
  }
  const driver = anyDriverErrorOf(cmd);
  if (driver) {
    if (driver.kind === 'cancelled') return { ...base, title: 'Cancelled', action: 'replan', actionLabel: 'Plan again' };
    // Nothing changed and the same change can run again once the database is free: PostgreSQL `object_in_use`
    // (sessions on a database being dropped or renamed) and MySQL / MariaDB lock wait timeouts (the app's
    // ALTER DATABASE gives up quickly while the database is locked). Nothing else re-runs without a new review.
    if (driver.code === OBJECT_IN_USE || mysqlErrorNumber(driver.code) === LOCK_WAIT_TIMEOUT) {
      return {
        ...base,
        title: 'Database in use',
        hint: 'Close the other sessions (tabs, other clients) and retry, or choose to terminate them where the drop offers it.',
        action: 'rerun',
        actionLabel: 'Retry',
      };
    }
    return { ...base, action: 'replan', actionLabel: 'Plan again' };
  }
  return base;
}

/** True when a describe failed only because the object does not exist (or is not of the kind asked for). */
export function isMissing(error: unknown): boolean {
  const cmd = toCmdError(error);
  return cmd.kind === 'notFound' || (cmd.kind === 'catalog' && (cmd.error.kind === 'notFound' || cmd.error.kind === 'unsupported'));
}

/** True when the failed call kept its plan (refused before anything ran). */
export function planKept(error: unknown): boolean {
  const cmd = toCmdError(error);
  return openTransactionOf(cmd) !== null || cmd.kind === 'readOnly' || cmd.kind === 'reconnectRequired';
}

export interface ApplyDeps {
  editApply: (planId: string, confirmed: boolean) => Promise<EditApplied>;
  ddlApply: (ddl: string, database: string | null, confirmed: boolean) => Promise<DdlApplied>;
  confirm: (request: ConfirmationRequest) => Promise<boolean>;
}

export interface ApplyRun {
  /** Steps that ran in this call. */
  ran: number;
  /** The user declined a confirmation; the step and the ones after it did not run. */
  declined: boolean;
  /** Index of the failed step. */
  failedAt: number | null;
  error: unknown;
  /** Warnings of DDL steps (collation restore problems and the like), to show prominently. */
  warnings: string[];
  durationMs: number;
}

/**
 * Runs the pending steps in order. Each step asks for confirmation when
 * the backend wants it; a decline stops before that step. The first
 * failure stops the run and marks that step failed. Steps are updated in
 * place (`status`), so the preview shows what ran.
 */
export async function applySteps(steps: PlannedStep[], deps: ApplyDeps): Promise<ApplyRun> {
  const run: ApplyRun = { ran: 0, declined: false, failedAt: null, error: null, warnings: [], durationMs: 0 };
  for (let i = 0; i < steps.length; i += 1) {
    const step = steps[i] as PlannedStep;
    if (step.status === 'done') continue;
    try {
      if (step.spec.kind === 'edit') {
        const planId = step.planId;
        if (planId === null) throw new Error('The step was not planned.');
        const result = await runWithConfirmation((confirmed) => deps.editApply(planId, confirmed), deps.confirm);
        if (result === null) {
          run.declined = true;
          return run;
        }
        run.durationMs += result.durationMs;
      } else {
        const spec = step.spec;
        const result = await runWithConfirmation((confirmed) => deps.ddlApply(spec.ddl, spec.database, confirmed), deps.confirm);
        if (result === null) {
          run.declined = true;
          return run;
        }
        run.durationMs += result.durationMs;
        run.warnings.push(...result.warnings);
      }
      step.status = 'done';
      run.ran += 1;
    } catch (error: unknown) {
      step.status = 'failed';
      run.failedAt = i;
      run.error = error;
      return run;
    }
  }
  return run;
}

/** An `EditPlan`-shaped preview of a DDL step. */
export function ddlStepPlan(ddl: string): EditPlan {
  return { statements: [ddl.trim().replace(/;\s*$/, '')], transactional: false, destructive: false, warnings: [] };
}

/** All statements of the steps, for "Will run" and Copy, each ending in `;`. */
export function stepsSql(steps: readonly PlannedStep[]): string {
  return steps
    .flatMap((s) => s.plan.statements)
    .map((s) => (s.trimEnd().endsWith(';') ? s.trimEnd() : `${s.trimEnd()};`))
    .join('\n\n');
}

/** Warnings of all steps, without repeats. */
export function stepsWarnings(steps: readonly PlannedStep[]): string[] {
  return [...new Set(steps.flatMap((s) => s.plan.warnings))];
}

/** Sentences describing how the steps run: transactions, separate commits, confirmation. */
export function stepsNotes(steps: readonly PlannedStep[], engine: 'postgres' | 'mysql' | 'mariadb' | 'redis'): string[] {
  const notes: string[] = [];
  const statements = steps.reduce((n, s) => n + s.plan.statements.length, 0);
  if (statements === 0) return notes;
  if (steps.length > 1) {
    notes.push(`Runs in ${steps.length} steps. Each step commits on its own; a failure stops the steps after it.`);
  }
  if (engine === 'postgres') {
    const one = steps.length === 1 ? steps[0] : null;
    if (one && one.plan.transactional && one.plan.statements.length > 1) notes.push('Runs in one transaction: on failure everything is rolled back.');
    if (steps.some((s) => !s.plan.transactional && s.spec.kind === 'edit')) notes.push('Some statements cannot run in a transaction and commit one by one.');
  } else if (engine === 'mysql' || engine === 'mariadb') {
    notes.push('MySQL and MariaDB commit DDL implicitly: statements that ran before a failure stay.');
  }
  if (steps.some((s) => s.plan.destructive)) notes.push('Drops or rewrites objects or data: you confirm before it runs.');
  return notes;
}
