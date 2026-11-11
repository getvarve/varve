import { scanBatch } from './redisKeys';
import type { ScanStepResult } from './redisKeys';

/** A SCAN that answers from a list of steps and records the cursors it was called with. */
function scanner(steps: ScanStepResult[], clock?: { now: number; perStep: number }) {
  const cursors: string[] = [];
  const step = async (cursor: string): Promise<ScanStepResult | null> => {
    cursors.push(cursor);
    if (clock) clock.now += clock.perStep;
    return steps.shift() ?? null;
  };
  return { cursors, step };
}

describe('scanBatch', () => {
  it('keeps stepping until the target number of keys was added', async () => {
    const { cursors, step } = scanner([
      { added: 0, cursor: '7', capped: false },
      { added: 300, cursor: '9', capped: false },
      { added: 250, cursor: '12', capped: false },
      { added: 1, cursor: '15', capped: false },
    ]);
    await scanBatch({ cursor: '0', target: 500, budgetMs: 2000, step });
    expect(cursors).toEqual(['0', '7', '9']);
  });

  it('stops at the end of the scan and at the key cap', async () => {
    const done = scanner([
      { added: 0, cursor: '4', capped: false },
      { added: 2, cursor: '0', capped: false },
      { added: 9, cursor: '8', capped: false },
    ]);
    await scanBatch({ cursor: '3', target: 500, budgetMs: 2000, step: done.step });
    expect(done.cursors).toEqual(['3', '4']);

    const capped = scanner([
      { added: 10, cursor: '0', capped: true },
      { added: 9, cursor: '8', capped: false },
    ]);
    await scanBatch({ cursor: '5', target: 500, budgetMs: 2000, step: capped.step });
    expect(capped.cursors).toEqual(['5']);
  });

  it('pauses once the time budget is used, and ends at once on a stale step', async () => {
    const clock = { now: 0, perStep: 600 };
    const slow = scanner(
      Array.from({ length: 10 }, (_, i) => ({ added: 0, cursor: String(i + 1), capped: false })),
      clock,
    );
    await scanBatch({ cursor: '0', target: 500, budgetMs: 2000, step: slow.step, now: () => clock.now });
    // Steps end at 600, 1200, 1800 and 2400 ms: the fourth one uses up the budget.
    expect(slow.cursors).toEqual(['0', '1', '2', '3']);

    const stale = scanner([]);
    await scanBatch({ cursor: '0', target: 500, budgetMs: 2000, step: stale.step });
    expect(stale.cursors).toEqual(['0']);
  });
});
