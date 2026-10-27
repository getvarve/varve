import { ref, shallowRef } from 'vue';
import { presentError } from '../../api/errors';
import type { CmdError } from '../../api/errors';
import { redisPlanApply } from '../../api/live';
import type { Planned, RedisPlanView } from '../../api/live';
import type { SessionInfo, Value } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import { runWithConfirmation } from '../../lib/destructive';

/** A Redis config or function plan the panel shows before it runs. */
export interface ShownPlan {
  /** What the plan does, for the preview heading. */
  title: string;
  planId: string;
  plan: RedisPlanView;
}

/**
 * Plan, preview and apply for the Redis config and function panels. `plan`
 * asks the server for the plan (nothing runs); `apply` runs exactly that
 * stored plan, through the destructive confirmation when the server asks
 * (always for a destructive plan).
 *
 * The server takes a plan when it starts running it, so after any failed
 * apply the preview is gone: it is cleared, the error names the failing
 * command (the server's `detail`), and `replan` plans the same change again
 * (`canReplan`). Nothing is applied twice without a new preview.
 */
export function useRedisPlan(session: () => SessionInfo, connectionName: () => string, report: (error: unknown) => CmdError) {
  const dialogs = useDialogs();
  const shown = shallowRef<ShownPlan | null>(null);
  const busy = ref(false);
  const error = ref('');
  /** The last planned change, so a failed apply can be planned again. */
  let last: { title: string; make: () => Promise<Planned<RedisPlanView>> } | null = null;
  const canReplan = ref(false);

  async function plan(title: string, make: () => Promise<Planned<RedisPlanView>>): Promise<boolean> {
    if (busy.value) return false;
    busy.value = true;
    error.value = '';
    canReplan.value = false;
    last = { title, make };
    try {
      const planned = await make();
      shown.value = { title, planId: planned.planId, plan: planned.plan };
      return true;
    } catch (e: unknown) {
      error.value = presentError(report(e)).message;
      return false;
    } finally {
      busy.value = false;
    }
  }

  /** Runs the shown plan; resolves with the replies, or `null` when declined or failed. */
  async function apply(): Promise<Value[] | null> {
    const current = shown.value;
    if (!current || busy.value) return null;
    busy.value = true;
    error.value = '';
    try {
      const replies = await runWithConfirmation(
        (confirmed) => redisPlanApply(session().id, current.planId, confirmed),
        (request) => dialogs.askDestructive({ connectionName: connectionName(), environment: request.environment, statements: request.statements }),
      );
      if (replies !== null) {
        shown.value = null;
        last = null;
      }
      return replies;
    } catch (e: unknown) {
      // The plan was taken when it started (or has expired): it cannot run again.
      const p = presentError(report(e));
      shown.value = null;
      canReplan.value = last !== null;
      error.value = p.detail ? `${p.message} (${p.detail})` : p.message;
      return null;
    } finally {
      busy.value = false;
    }
  }

  /** Plans the change of the failed apply again, for a fresh preview. */
  async function replan(): Promise<boolean> {
    const again = last;
    if (!again) return false;
    return plan(again.title, again.make);
  }

  function discard(): void {
    shown.value = null;
    error.value = '';
    canReplan.value = false;
    last = null;
  }

  return { shown, busy, error, canReplan, plan, apply, replan, discard };
}
