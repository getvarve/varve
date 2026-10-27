import { describe, expect, it } from 'vitest';
import type { PartialOutput } from '../api/sessions';
import { describePartial, partialAsOutput } from './partialRun';

function partial(failedStatement: number | null, completed: PartialOutput['completed'], rows = 0): PartialOutput {
  return {
    results: Array.from({ length: rows }, () => ({ kind: 'rows' as const, columns: [], rows: [], truncated: false })),
    notices: [{ severity: 'NOTICE', message: 'n' }],
    elapsedMs: 12,
    failedStatement,
    completed,
  };
}

describe('partial output of a failed run', () => {
  it('becomes the output to show', () => {
    expect(partialAsOutput(null)).toBeNull();
    expect(partialAsOutput(partial(1, ['committed'], 1))).toEqual({
      results: [{ kind: 'rows', columns: [], rows: [], truncated: false }],
      notices: [{ severity: 'NOTICE', message: 'n' }],
      elapsedMs: 12,
    });
  });

  it('says which statements completed and what became of them', () => {
    expect(describePartial(partial(1, ['committed']), 'mysql')).toEqual(['Statement 1 completed before the failure.', 'Its changes are committed.']);
    expect(describePartial(partial(3, ['committed', 'in_transaction', 'in_transaction']), 'mysql')).toEqual([
      'Statements 1 to 3 completed before the failure.',
      '1 statement is committed.',
      '2 statements ran in the transaction that is still open: commit or roll it back.',
    ]);
    // PostgreSQL aborts the transaction on an error.
    expect(describePartial(partial(1, ['in_transaction']), 'postgres')[1]).toBe(
      '1 statement ran in the transaction that is still open: the error aborted that transaction, so it can only be rolled back.',
    );
    expect(describePartial(partial(4, ['committed', 'committed', 'failed', 'committed']), 'redis')).toEqual([
      'Commands 1 to 4 completed before the failure.',
      '3 commands are committed.',
      '1 command failed inside EXEC and changed nothing.',
    ]);
    expect(describePartial(partial(2, ['committed', 'in_transaction']), 'redis')[2]).toBe(
      '1 command ran in the MULTI block that is still open: they are queued until EXEC, or DISCARD drops them.',
    );
    expect(describePartial(partial(2, ['rolled_back', 'unknown']), 'mysql')).toEqual([
      'Statements 1 to 2 completed before the failure.',
      '1 statement was rolled back.',
      'For 1 statement it is not known whether the changes were committed.',
    ]);
    expect(describePartial(partial(0, []), 'mysql')).toEqual(['No statement completed before the failure.']);
    expect(describePartial(partial(0, [], 1), 'mysql')).toEqual(['The first statement sent these results before it failed.']);
    expect(describePartial(partial(null, [], 2), 'mysql')[0]).toContain('not known');
  });
});
