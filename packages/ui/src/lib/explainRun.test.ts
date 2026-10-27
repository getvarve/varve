import { describe, expect, it, vi } from 'vitest';
import { CommandFailure } from '../api/errors';
import { explainConfirmationOf, presentExplainError, runExplain } from './explainRun';

const confirmationRequired = {
  kind: 'session',
  error: {
    kind: 'confirmationRequired',
    environment: 'production',
    statements: [
      'EXPLAIN ANALYZE runs this Insert statement for real, inside a transaction that is rolled back afterwards.',
      'The statement changes data, so it cannot run in a read-only transaction.',
      'The session is inside a transaction, where the run cannot be made read-only.',
    ],
  },
};

const driverError = (kind: string, extra: Record<string, unknown> = {}) => ({
  kind: 'session',
  error: { kind: 'driver', error: { kind, message: `${kind} happened`, code: null, detail: null, hint: null, position: null, ...extra } },
});

describe('explainConfirmationOf', () => {
  it('lists every reason of confirmationRequired in order', () => {
    expect(explainConfirmationOf(confirmationRequired)).toEqual({
      environment: 'production',
      reasons: confirmationRequired.error.statements,
    });
  });

  it("accepts the catalog's needsConfirmation as message plus warnings", () => {
    const error = { kind: 'catalog', error: { kind: 'needsConfirmation', message: 'Runs for real.', warnings: ['Calls pg_advisory_lock.'] } };
    expect(explainConfirmationOf(error)).toEqual({ environment: null, reasons: ['Runs for real.', 'Calls pg_advisory_lock.'] });
  });

  it('returns null for other errors', () => {
    expect(explainConfirmationOf(driverError('query'))).toBeNull();
    expect(explainConfirmationOf(new Error('boom'))).toBeNull();
  });
});

describe('runExplain', () => {
  it('returns the plan of a run that needs no confirmation without asking', async () => {
    const run = vi.fn(async () => 'plan');
    const confirm = vi.fn(async () => true);
    expect(await runExplain(run, confirm)).toEqual({ status: 'done', value: 'plan' });
    expect(run).toHaveBeenCalledTimes(1);
    expect(run).toHaveBeenCalledWith(false);
    expect(confirm).not.toHaveBeenCalled();
  });

  it('asks with every reason and repeats the call confirmed after a yes', async () => {
    const run = vi.fn(async (confirmed: boolean) => {
      if (!confirmed) throw new CommandFailure(confirmationRequired as never);
      return 'plan';
    });
    const confirm = vi.fn(async () => true);
    expect(await runExplain(run, confirm)).toEqual({ status: 'done', value: 'plan' });
    expect(confirm).toHaveBeenCalledWith({ environment: 'production', reasons: confirmationRequired.error.statements });
    expect(run.mock.calls).toEqual([[false], [true]]);
  });

  it('does not run when the user declines', async () => {
    const run = vi.fn(async () => {
      throw confirmationRequired;
    });
    expect(await runExplain(run, async () => false)).toEqual({ status: 'declined' });
    expect(run).toHaveBeenCalledTimes(1);
  });

  it('reports a cancelled run as cancelled, before or after confirming', async () => {
    expect(
      await runExplain(async () => {
        throw driverError('cancelled');
      }, async () => true),
    ).toEqual({ status: 'cancelled' });

    const run = vi.fn(async (confirmed: boolean) => {
      throw confirmed ? driverError('cancelled') : confirmationRequired;
    });
    expect(await runExplain(run, async () => true)).toEqual({ status: 'cancelled' });
  });

  it("returns the statement's own error, with its position", async () => {
    const outcome = await runExplain(async () => {
      throw driverError('query', { message: 'column "x" does not exist', code: '42703', position: 8, hint: 'Perhaps you meant "y".' });
    }, async () => true);
    expect(outcome.status).toBe('failed');
    if (outcome.status !== 'failed') return;
    const shown = presentExplainError(outcome.error);
    expect(shown).toMatchObject({ title: 'Query failed', message: 'column "x" does not exist', code: '42703', hint: 'Perhaps you meant "y".', position: 8 });
  });

  it('treats a refusal (read-only, invalid) as a failure without asking', async () => {
    const confirm = vi.fn(async () => true);
    const outcome = await runExplain(async () => {
      throw { kind: 'readOnly', message: 'The statement changes data.' };
    }, confirm);
    expect(outcome).toEqual({ status: 'failed', error: { kind: 'readOnly', message: 'The statement changes data.' } });
    expect(confirm).not.toHaveBeenCalled();
  });

  it('fails instead of looping when the confirmed call asks again', async () => {
    const run = vi.fn(async () => {
      throw confirmationRequired;
    });
    const outcome = await runExplain(run, async () => true);
    expect(outcome.status).toBe('failed');
    expect(run).toHaveBeenCalledTimes(2);
  });
});
