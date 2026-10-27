import { reactive } from 'vue';
import { ddlApply, editApply, editPlan } from '../../api/ddl';
import type { SessionInfo } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import { latestOnly } from '../../lib/async';
import { applyProblem, applySteps, ddlStepPlan, planKept } from '../../lib/ddlApply';
import type { ApplyProblem, PlannedStep, StepSpec } from '../../lib/ddlApply';
import { useNotices } from '../notices/useNotices';

/** Where an editor's plan stands. */
export interface ChangePlanState {
  steps: PlannedStep[];
  /** The specs the steps were planned from, to plan again after a spent plan. */
  specs: StepSpec[];
  planning: boolean;
  /** Planning failed (for example `catalog` / `invalid` from the generator). */
  planProblem: ApplyProblem | null;
  applying: boolean;
  /** The last apply failed or was refused. */
  problem: ApplyProblem | null;
  /** Warnings DDL steps returned, shown until the next plan. */
  runWarnings: string[];
  /** True after every step ran. */
  applied: boolean;
  /** The user declined a confirmation. */
  declined: boolean;
}

export interface ChangePlanContext {
  session: () => SessionInfo;
  connectionName: () => string;
  /** Called with every failed call (tab error reporting). */
  reportError?: (error: unknown) => void;
  /** Called after any step ran (success or failure): refresh the tree and the editor. */
  onChanged: () => void;
  /**
   * Runs before the first step runs (for example: close the app's own sessions on a database being dropped).
   * For destructive steps only once the user confirmed, so declining changes nothing.
   */
  beforeApply?: (() => Promise<void>) | undefined;
  /** Extra sentences for the destructive confirmation (tabs a drop ends, ...). */
  confirmWarnings?: (() => string[]) | undefined;
  /**
   * Runs after every step ran, before `onChanged` refreshes the tree (for
   * example: point the connection at a renamed database first, so the
   * refresh opens the database by its new name).
   */
  afterRun?: (() => Promise<void>) | undefined;
}

/**
 * Plans an editor's changes on the server, previews them and applies them
 * with the contract's rules: confirmation dialog on demand, plans kept on
 * an open transaction, planned again when spent or expired, pending
 * collation restores re-read after DDL steps.
 */
export function useChangePlan(context: ChangePlanContext) {
  const state = reactive<ChangePlanState>({
    steps: [],
    specs: [],
    planning: false,
    planProblem: null,
    applying: false,
    problem: null,
    runWarnings: [],
    applied: false,
    declined: false,
  });
  const latest = latestOnly();

  /** Plans the specs; an empty list clears the preview. Only the latest call writes the state. */
  async function plan(specs: StepSpec[]): Promise<boolean> {
    const token = latest.next();
    state.specs = specs;
    state.problem = null;
    state.applied = false;
    state.declined = false;
    state.runWarnings = [];
    if (specs.length === 0) {
      state.steps = [];
      state.planProblem = null;
      state.planning = false;
      return true;
    }
    state.planning = true;
    const sessionId = context.session().id;
    try {
      const steps: PlannedStep[] = [];
      for (const spec of specs) {
        if (spec.kind === 'edit') {
          const planned = await editPlan(sessionId, spec.change);
          steps.push({ spec, planId: planned.planId, plan: planned.plan, status: 'pending' });
        } else {
          steps.push({ spec, planId: null, plan: ddlStepPlan(spec.ddl), status: 'pending' });
        }
      }
      if (!latest.isCurrent(token)) return false;
      state.steps = steps;
      state.planProblem = null;
      return true;
    } catch (error: unknown) {
      context.reportError?.(error);
      if (!latest.isCurrent(token)) return false;
      state.steps = [];
      state.planProblem = applyProblem(error);
      return false;
    } finally {
      if (latest.isCurrent(token)) state.planning = false;
    }
  }

  /** Runs the pending steps. Resolves true when all of them ran. */
  async function apply(): Promise<boolean> {
    if (state.applying || state.steps.length === 0) return false;
    const session = context.session();
    state.applying = true;
    state.problem = null;
    state.declined = false;
    const dialogs = useDialogs();
    let prepared = false;
    const prepare = async (): Promise<void> => {
      if (prepared) return;
      prepared = true;
      await context.beforeApply?.();
    };
    try {
      // A destructive step always asks first (the backend refuses it unconfirmed): prepare after the yes.
      if (!state.steps.some((s) => s.status !== 'done' && s.plan.destructive)) await prepare();
      const run = await applySteps(state.steps, {
        editApply: async (planId, confirmed) => {
          if (confirmed) await prepare();
          return editApply(session.id, planId, confirmed);
        },
        ddlApply: async (ddl, database, confirmed) => {
          if (confirmed) await prepare();
          return ddlApply(session.id, ddl, database, confirmed);
        },
        confirm: (request) => {
          const warnings = context.confirmWarnings?.() ?? [];
          return dialogs.askDestructive({
            connectionName: context.connectionName(),
            environment: request.environment,
            statements: request.statements,
            ...(warnings.length > 0 ? { warnings } : {}),
          });
        },
      });
      state.runWarnings = run.warnings;
      state.declined = run.declined;
      const ranDdl = state.steps.some((s) => s.spec.kind === 'ddl' && s.status !== 'pending');
      if (run.error === null && !run.declined && state.steps.every((s) => s.status === 'done')) await context.afterRun?.();
      if (run.ran > 0 || (run.error !== null && !planKept(run.error))) context.onChanged();
      if (ranDdl && (run.warnings.length > 0 || run.error !== null)) void useNotices().refreshRestores();
      if (run.error !== null) {
        context.reportError?.(run.error);
        state.problem = applyProblem(run.error);
        return false;
      }
      state.applied = !run.declined && state.steps.every((s) => s.status === 'done');
      return state.applied;
    } finally {
      state.applying = false;
    }
  }

  /** The action of the current problem: run the kept plans again, plan what did not run yet, or (`rerun`) plan it and run it. */
  async function retry(): Promise<void> {
    const problem = state.problem ?? state.planProblem;
    if (!problem) return;
    if (problem.action === 'retry') {
      for (const s of state.steps) if (s.status === 'failed') s.status = 'pending';
      await apply();
      return;
    }
    const remaining = state.steps.length > 0 ? state.steps.filter((s) => s.status !== 'done').map((s) => s.spec) : state.specs;
    // "Retry" for a database in use runs the change again, not only its plan.
    if ((await plan(remaining)) && problem.action === 'rerun') await apply();
  }

  function clear(): void {
    latest.next();
    state.steps = [];
    state.specs = [];
    state.planning = false;
    state.planProblem = null;
    state.problem = null;
    state.runWarnings = [];
    state.applied = false;
    state.declined = false;
  }

  return { state, plan, apply, retry, clear };
}

export type ChangePlan = ReturnType<typeof useChangePlan>;
