import { reactive } from 'vue';
import { redisEditApply, redisEditPlan } from '../../api/redis';
import type { Environment } from '../../api/records';
import type { KeyEdit, Planned, RedisEditPlan, RedisEditRequest } from '../../api/redis';
import { useDialogs } from '../../composables/useDialogs';
import { useTabs } from '../../composables/useTabs';
import { runWithConfirmation } from '../../lib/destructive';
import { commandStatements, outcomeView } from '../../lib/redisEdit';
import type { OutcomeView } from '../../lib/redisEdit';
import { redisErrorView } from '../../lib/redisErrors';
import type { RedisErrorView } from '../../lib/redisErrors';

export interface KeyEditState {
  /** The plan being previewed, with the edits it was made from. */
  planned: Planned<RedisEditPlan> | null;
  edits: KeyEdit[];
  planning: boolean;
  applying: boolean;
  /** A refusal or failure of the last plan or apply. */
  error: RedisErrorView | null;
}

export interface KeyEditOptions {
  /** The tab whose session runs the edits; lost sessions and reconnects are reported to it. */
  tabId: () => string;
  sessionId: () => string;
  connectionName: () => string;
  /** Called after an apply ran (applied, conflict or failed) or failed after it may have run. */
  done: (view: OutcomeView) => void;
}

/**
 * Plan, preview and apply of Redis key edits: the plan is shown first, a
 * destructive one asks for confirmation, and every outcome (applied,
 * conflict, failed) re-reads the key.
 */
export function useKeyEdit(options: KeyEditOptions) {
  const state = reactive<KeyEditState>({ planned: null, edits: [], planning: false, applying: false, error: null });

  /** Plans `request` and opens the preview. Resolves false when the backend refused. */
  function review(request: RedisEditRequest): Promise<boolean> {
    return reviewWith(() => redisEditPlan(options.sessionId(), request), request.edits);
  }

  /**
   * Plans with `planner` (value edits, or a stream group change) and opens the
   * preview; `edits` name what the plan does in the outcome message.
   */
  async function reviewWith(planner: () => Promise<Planned<RedisEditPlan>>, edits: readonly KeyEdit[] = []): Promise<boolean> {
    if (state.planning || state.applying) return false;
    state.error = null;
    return planWith(planner, edits);
  }

  /** The planner of the shown plan, so an expired plan can be planned again. */
  let lastPlanner: (() => Promise<Planned<RedisEditPlan>>) | null = null;

  async function planWith(planner: () => Promise<Planned<RedisEditPlan>>, edits: readonly KeyEdit[]): Promise<boolean> {
    state.planning = true;
    try {
      const planned = await planner();
      state.planned = planned;
      state.edits = [...edits];
      lastPlanner = planner;
      return true;
    } catch (e: unknown) {
      state.error = redisErrorView(useTabs().reportTabError(options.tabId(), e));
      return false;
    } finally {
      state.planning = false;
    }
  }

  /** Closes the preview without running the plan (it expires on the server). */
  function cancel(): void {
    if (state.applying) return;
    state.planned = null;
    state.edits = [];
    lastPlanner = null;
  }

  /**
   * Asks before a plan runs. Command names go to the destructive dialog (it
   * asks for the connection name on production), with the warnings the
   * backend sent along (an unchecked plan says why it cannot be checked).
   * An unchecked plan whose only reason to ask is its warning gets a plain
   * confirmation with that warning.
   */
  function confirmPlan(plan: RedisEditPlan, environment: Environment, statements: string[]): Promise<boolean> {
    const names = commandStatements(statements);
    if (names.length > 0) {
      const sentences = statements.filter((s) => !names.includes(s));
      const warnings = sentences.length > 0 ? sentences : plan.unchecked ? plan.warnings : [];
      return useDialogs().askDestructive({ connectionName: options.connectionName(), environment, statements: names, warnings });
    }
    const warnings = statements.length > 0 ? statements : plan.warnings;
    return useDialogs().askConfirm({
      title: 'Apply without a check for changes by others?',
      message: warnings.join('\n\n'),
      confirmLabel: 'Apply',
      danger: true,
    });
  }

  /** Runs the previewed plan, asking first when the backend wants confirmation. */
  async function apply(): Promise<void> {
    const planned = state.planned;
    if (!planned || state.applying) return;
    const sessionId = options.sessionId();
    state.applying = true;
    state.error = null;
    try {
      const outcome = await runWithConfirmation(
        (confirmed) => redisEditApply(sessionId, planned.planId, confirmed),
        (request) => confirmPlan(planned.plan, request.environment, request.statements),
      );
      // Declined: the plan stays available, the preview stays open.
      if (outcome === null) return;
      const view = outcomeView(outcome, state.edits);
      state.planned = null;
      state.edits = [];
      options.done(view);
    } catch (e: unknown) {
      const cmd = useTabs().reportTabError(options.tabId(), e);
      const view = redisErrorView(cmd);
      // The plan expired or was evicted: refused before anything ran. The
      // edits stay staged; plan them again for a fresh preview.
      if (cmd.kind === 'notFound') {
        state.planned = null;
        const planner = lastPlanner;
        const replanned = planner !== null && (await planWith(planner, state.edits));
        // planWith sets the error when planning again failed.
        const failed = state.error as RedisErrorView | null;
        if (replanned) {
          state.error = {
            title: 'The preview expired',
            message: 'The previous preview expired before it ran, so nothing was changed. This is a new preview of the same edits; review it and apply again.',
            hint: null,
            action: null,
            refused: true,
          };
        } else if (failed === null || planner === null) {
          state.error = { ...view, title: 'The preview expired', message: 'The preview expired before it ran, so nothing was changed. Your edits are still staged; review and save them again.' };
        } else {
          state.error = { ...failed, message: `The preview expired before it ran, so nothing was changed, and planning the edits again failed: ${failed.message}` };
        }
        return;
      }
      // Refused before anything ran (read-only, MULTI_OPEN, session gone): keep the preview to retry.
      if (view.refused) {
        state.error = view;
        return;
      }
      // After an error the plan is spent; a cancel or timeout after EXEC was sent may have applied it.
      state.planned = null;
      state.edits = [];
      options.done({
        tone: 'danger',
        title: view.title,
        message: `${view.message}${view.hint ? ` ${view.hint}` : ''} The key was read again to show its current state.`,
        reread: true,
      });
    } finally {
      state.applying = false;
    }
  }

  return { state, review, reviewWith, cancel, apply };
}
