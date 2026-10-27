import type { Engine } from '../api/records';
import type { Durability, ExecOutput, PartialOutput } from '../api/sessions';

/** The output to show for a failed run: what its completed statements produced. */
export function partialAsOutput(partial: PartialOutput | null | undefined): ExecOutput | null {
  if (!partial) return null;
  return { results: partial.results, notices: partial.notices, elapsedMs: partial.elapsedMs };
}

function plural(count: number, one: string, many: string): string {
  return count === 1 ? one : many;
}

/**
 * Sentences that say which statements of a failed run completed and what
 * became of their changes (statement numbers are 1-based for people).
 * `engine` decides what can still be done with an open transaction: after
 * an error PostgreSQL has aborted it, so it can only be rolled back.
 */
export function describePartial(partial: PartialOutput, engine: Engine): string[] {
  // Redis runs commands, the SQL engines statements.
  const one = engine === 'redis' ? 'command' : 'statement';
  const many = engine === 'redis' ? 'commands' : 'statements';
  const One = one.charAt(0).toUpperCase() + one.slice(1);
  const Many = many.charAt(0).toUpperCase() + many.slice(1);
  const done = partial.failedStatement;
  if (done === null) {
    return [`The results above came from ${many} before the failure; which ${one} failed is not known, so nothing is said about their changes.`];
  }
  if (done === 0) {
    return partial.results.length > 0 ? [`The first ${one} sent these results before it failed.`] : [`No ${one} completed before the failure.`];
  }
  const lines = [done === 1 ? `${One} 1 completed before the failure.` : `${Many} 1 to ${done} completed before the failure.`];
  const count = (d: Durability): number => partial.completed.filter((c) => c === d).length;
  const committed = count('committed');
  const open = count('in_transaction');
  const rolledBack = count('rolled_back');
  const unknown = count('unknown');
  const failed = count('failed');
  if (committed === partial.completed.length) {
    lines.push(done === 1 ? 'Its changes are committed.' : 'Their changes are committed.');
    return lines;
  }
  if (committed > 0) {
    lines.push(`${committed} ${plural(committed, `${one} is`, `${many} are`)} committed.`);
  }
  if (open > 0) {
    const what =
      engine === 'postgres'
        ? 'the error aborted that transaction, so it can only be rolled back.'
        : engine === 'redis'
          ? 'they are queued until EXEC, or DISCARD drops them.'
          : 'commit or roll it back.';
    const place = engine === 'redis' ? 'in the MULTI block that is still open' : 'in the transaction that is still open';
    lines.push(`${open} ${plural(open, `${one} ran`, `${many} ran`)} ${place}: ${what}`);
  }
  if (rolledBack > 0) {
    lines.push(`${rolledBack} ${plural(rolledBack, `${one} was`, `${many} were`)} rolled back.`);
  }
  if (failed > 0) {
    lines.push(`${failed} ${plural(failed, 'command', 'commands')} failed inside EXEC and changed nothing.`);
  }
  if (unknown > 0) {
    lines.push(`For ${unknown} ${plural(unknown, one, many)} it is not known whether the changes were committed.`);
  }
  return lines;
}
