/**
 * Planning and applying object edits for the table designer: every change
 * is planned on the server first (`edit_plan`), shown, and then applied in
 * order (`edit_apply`) with the destructive confirmation dialog. A refusal
 * before anything ran (confirmation declined, open transaction) keeps the
 * remaining plans, so "Try again" continues with the same plan ids.
 */
import { reactive } from 'vue';
import { editApply, editPlan } from '../../api/grid';
import type { EditApplied, EditPlan, EngineChange } from '../../api/grid';
import { openTransactionOf, presentError, toCmdError } from '../../api/errors';
import type { Environment } from '../../api/records';
import { useDialogs } from '../../composables/useDialogs';
import { runWithConfirmation } from '../../lib/destructive';

/** Where a planned step stands. */
export type StepStatus = 'pending' | 'running' | 'applied' | 'failed' | 'skipped';

/** One planned change. */
export interface PlanStep {
  label: string;
  change: EngineChange;
  planId: string;
  plan: EditPlan;
  /** What the designer found out about this step on the server (rows a new engine loses, an AUTO_INCREMENT the server raises); shown with the plan and in the confirmation. */
  notes: string[];
  status: StepStatus;
  /** The server's error for a failed step. */
  error: string | null;
  result: EditApplied | null;
}

/** A short label for a change, for the review list. */
export function changeLabel(change: EngineChange): string {
  if (change.engine === 'mysql') {
    const m = change.change;
    if (m.change === 'createTable') return `Create table ${m.name}`;
    if (m.change === 'alterTable') {
      const n = m.operations.length;
      return `Alter table ${m.name} (${n} ${n === 1 ? 'change' : 'changes'})`;
    }
    // Other editors' changes (the designer does not build them); every one names its object.
    return `${m.change} ${m.name}`;
  }
  const c = change.change;
  if (c.object === 'comment') return `Comment on ${c.target.kind} ${c.target.name}`;
  if (c.object === 'table') {
    if (c.action === 'create') return c.partitionOf ? `Create partition ${c.name.name}` : `Create table ${c.name.name}`;
    if (c.action === 'drop') return `Drop table ${c.name.name}`;
    const n = c.operations.length;
    return `Alter table ${c.name.name} (${n} ${n === 1 ? 'change' : 'changes'})`;
  }
  if (c.object === 'index') {
    switch (c.action) {
      case 'create':
        return `Create index ${c.name ?? '(named by the server)'}`;
      case 'drop':
        return `Drop index ${c.name.name}`;
      case 'alter':
        return `Rename index ${c.name.name}`;
      case 'reindex':
        return `Reindex ${c.target.name.name}`;
    }
  }
  // Other editors' changes (the designer does not build them).
  return `Change ${c.object}`;
}

/** Plans every change in order. Rejects with the first planning error, naming the step. */
export async function planSteps(sessionId: string, changes: EngineChange[], notes: readonly (readonly string[])[] = []): Promise<PlanStep[]> {
  const steps: PlanStep[] = [];
  for (const [n, change] of changes.entries()) {
    const label = changeLabel(change);
    try {
      const planned = await editPlan(sessionId, change);
      steps.push({ label, change, planId: planned.planId, plan: planned.plan, notes: [...(notes[n] ?? [])], status: 'pending', error: null, result: null });
    } catch (error: unknown) {
      const p = presentError(toCmdError(error));
      throw new Error(`${label}: ${p.message}`, { cause: error });
    }
  }
  return reactive(steps) as PlanStep[];
}

/** How applying the steps ended. */
export type ApplyResult =
  | { kind: 'done' }
  | { kind: 'declined' }
  | { kind: 'openTransaction'; message: string }
  | { kind: 'failed'; message: string };

/**
 * Applies the pending steps in order. Stops at the first refusal (the
 * remaining steps keep their plans) or failure (the remaining steps are
 * skipped and must be planned again).
 */
export async function applySteps(
  sessionId: string,
  steps: PlanStep[],
  who: { connectionName: string; environment: Environment },
): Promise<ApplyResult> {
  const { askDestructive } = useDialogs();
  for (const step of steps) {
    if (step.status !== 'pending') continue;
    step.status = 'running';
    try {
      const result = await runWithConfirmation(
        (confirmed) => editApply(sessionId, step.planId, confirmed),
        (request) =>
          askDestructive({
            connectionName: who.connectionName,
            environment: request.environment,
            statements: request.statements,
            warnings: [...step.notes, ...step.plan.warnings],
          }),
      );
      if (result === null) {
        step.status = 'pending';
        return { kind: 'declined' };
      }
      step.status = 'applied';
      step.result = result;
    } catch (error: unknown) {
      const cmd = toCmdError(error);
      const open = openTransactionOf(cmd);
      if (open !== null) {
        step.status = 'pending';
        return { kind: 'openTransaction', message: open };
      }
      const p = presentError(cmd);
      step.status = 'failed';
      step.error = [p.message, p.detail, p.hint].filter(Boolean).join(' ');
      for (const rest of steps) if (rest.status === 'pending') rest.status = 'skipped';
      return { kind: 'failed', message: `${step.label} failed: ${step.error}` };
    }
  }
  return { kind: 'done' };
}
