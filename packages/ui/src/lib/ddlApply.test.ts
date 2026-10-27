import { describe, expect, it, vi } from 'vitest';
import type { DdlApplied, EditApplied, EditPlan } from '../api/ddl';
import { CommandFailure } from '../api/errors';
import type { CmdError } from '../api/errors';
import { applyProblem, applySteps, isMissing, planKept, stepsNotes, stepsSql, stepsWarnings } from './ddlApply';
import type { PlannedStep } from './ddlApply';

const plan = (statements: string[], extra: Partial<EditPlan> = {}): EditPlan => ({ statements, transactional: true, destructive: false, warnings: [], ...extra });
const change = { engine: 'postgres' as const, change: { object: 'extension' as const, action: 'update' as const, name: 'x' } };
const editStep = (id: string, p: EditPlan = plan(['SELECT 1'])): PlannedStep => ({ spec: { kind: 'edit', change }, planId: id, plan: p, status: 'pending' });
const applied: EditApplied = { statements: 1, transactional: true, outputs: [], durationMs: 5 };
const ddlApplied: DdlApplied = { output: { results: [], notices: [], elapsedMs: 1 }, session: null, collationDatabase: null, warnings: ['restore pending'], durationMs: 7 };
const fail = (cmd: CmdError): CommandFailure => new CommandFailure(cmd);
const confirmation = fail({ kind: 'session', error: { kind: 'confirmationRequired', environment: 'production', statements: ['Drop'] } });
const driver = (detail: string | null, kind: 'query' | 'cancelled' = 'query', code: string | null = null): CommandFailure =>
  fail({ kind: 'session', error: { kind: 'driver', error: { kind, message: 'boom', code, detail, hint: null, position: null } } });

describe('applySteps', () => {
  it('runs every step, asking for confirmation when the backend wants it', async () => {
    const editApply = vi.fn(async (id: string, confirmed: boolean) => {
      if (id === 'b' && !confirmed) throw confirmation;
      return applied;
    });
    const ddlApply = vi.fn(async () => ddlApplied);
    const confirm = vi.fn(async () => true);
    const steps = [editStep('a'), editStep('b'), { spec: { kind: 'ddl' as const, ddl: 'CREATE INDEX i ON t (a);', database: null, label: 'x' }, planId: null, plan: plan(['CREATE INDEX i ON t (a)']), status: 'pending' as const }];
    const run = await applySteps(steps, { editApply, ddlApply, confirm });
    expect(run).toMatchObject({ ran: 3, declined: false, failedAt: null, warnings: ['restore pending'], durationMs: 17 });
    expect(confirm).toHaveBeenCalledWith({ environment: 'production', statements: ['Drop'] });
    expect(editApply.mock.calls).toEqual([['a', false], ['b', false], ['b', true]]);
    expect(steps.every((s) => s.status === 'done')).toBe(true);
  });

  it('stops before a declined step and after a failed one', async () => {
    const declinedSteps = [editStep('a'), editStep('b')];
    const declined = await applySteps(declinedSteps, {
      editApply: async (id, confirmed) => {
        if (id === 'b' && !confirmed) throw confirmation;
        return applied;
      },
      ddlApply: async () => ddlApplied,
      confirm: async () => false,
    });
    expect(declined).toMatchObject({ ran: 1, declined: true });
    expect(declinedSteps.map((s) => s.status)).toEqual(['done', 'pending']);

    const failedSteps = [editStep('a'), editStep('b'), editStep('c')];
    const failed = await applySteps(failedSteps, {
      editApply: async (id) => {
        if (id === 'b') throw driver('Statement 1 of 2 failed');
        return applied;
      },
      ddlApply: async () => ddlApplied,
      confirm: async () => true,
    });
    expect(failed.failedAt).toBe(1);
    expect(failedSteps.map((s) => s.status)).toEqual(['done', 'failed', 'pending']);
  });

  it('skips steps that already ran', async () => {
    const steps = [{ ...editStep('a'), status: 'done' as const }, editStep('b')];
    const editApply = vi.fn(async () => applied);
    await applySteps(steps, { editApply, ddlApply: async () => ddlApplied, confirm: async () => true });
    expect(editApply.mock.calls).toEqual([['b', false]]);
  });
});

describe('applyProblem', () => {
  it('keeps the plan on an open transaction and offers a retry', () => {
    const error = fail({ kind: 'session', error: { kind: 'openTransaction', message: 'MySQL would commit your transaction.' } });
    expect(applyProblem(error)).toMatchObject({ title: 'Commit or roll back first', action: 'retry' });
    expect(planKept(error)).toBe(true);
  });

  it('plans again when the plan is gone, and retries a database in use', () => {
    expect(applyProblem(fail({ kind: 'notFound', message: 'unknown plan' })).action).toBe('replan');
    const inUse = 'The database is in use by another connection; nothing changed';
    expect(applyProblem(driver(inUse, 'query', '1205 (HY000)'))).toMatchObject({ title: 'Database in use', action: 'rerun', actionLabel: 'Retry' });
    expect(applyProblem(driver('There is 1 other session using the database.', 'query', '55006'))).toMatchObject({ action: 'rerun' });
    // Only those codes re-run without a new review: a detail that merely says "in use" plans again.
    expect(applyProblem(driver(inUse, 'query', '42P01'))).toMatchObject({ action: 'replan' });
    expect(applyProblem(driver(inUse))).toMatchObject({ action: 'replan' });
    expect(applyProblem(driver(null, 'cancelled')).title).toBe('Cancelled');
    expect(planKept(driver(null))).toBe(false);
  });

  it('explains read-only, reconnect, invalid and missing objects', () => {
    expect(applyProblem(fail({ kind: 'readOnly', message: 'The session is read only.' }))).toMatchObject({ action: 'none', message: 'The session is read only. Nothing ran.' });
    expect(applyProblem(fail({ kind: 'reconnectRequired', message: 'edited' })).action).toBe('reconnect');
    expect(applyProblem(fail({ kind: 'catalog', error: { kind: 'invalid', message: 'a view needs a query' } }))).toMatchObject({ action: 'none', message: 'a view needs a query' });
    const missing = fail({ kind: 'catalog', error: { kind: 'notFound', message: 'gone' } });
    expect(applyProblem(missing).action).toBe('refresh');
    expect(isMissing(missing)).toBe(true);
    expect(isMissing(driver(null))).toBe(false);
  });
});

describe('step summaries', () => {
  it('joins SQL, warnings and notes', () => {
    const steps = [editStep('a', plan(['DROP VIEW v', 'CREATE VIEW v AS SELECT 1;'], { destructive: true, warnings: ['w'] })), editStep('b', plan(['COMMENT ON VIEW v IS NULL'], { warnings: ['w'] }))];
    expect(stepsSql(steps)).toBe('DROP VIEW v;\n\nCREATE VIEW v AS SELECT 1;\n\nCOMMENT ON VIEW v IS NULL;');
    expect(stepsWarnings(steps)).toEqual(['w']);
    expect(stepsNotes(steps, 'postgres')).toEqual([
      'Runs in 2 steps. Each step commits on its own; a failure stops the steps after it.',
      'Drops or rewrites objects or data: you confirm before it runs.',
    ]);
    expect(stepsNotes([editStep('a', plan(['A', 'B'], { transactional: false }))], 'mysql')[0]).toMatch(/commit DDL implicitly/);
    expect(stepsNotes([], 'postgres')).toEqual([]);
  });
});
