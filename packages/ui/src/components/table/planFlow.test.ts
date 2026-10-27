/**
 * TD-H1: what the designer found out about a step (the rows an engine
 * change loses) goes into the destructive confirmation with the plan's own
 * warnings.
 */
import type { EngineChange } from '../../api/grid';
import { applySteps, planSteps } from './planFlow';

const asked: { statements: string[]; warnings?: string[] }[] = [];

vi.mock('@tauri-apps/api/core', () => ({
  invoke: vi.fn(async (command: string, args: Record<string, unknown>) => {
    if (command === 'edit_plan') {
      return { planId: 'p1', plan: { statements: ['ALTER TABLE `shop`.`orders` ENGINE=`BLACKHOLE`'], transactional: false, destructive: true, warnings: ['BLACKHOLE stores nothing: every row of the table is discarded, and rows written later are thrown away. The rows will be lost'] } };
    }
    if (command === 'edit_apply') {
      if (!args.confirmed) throw { kind: 'session', error: { kind: 'confirmationRequired', environment: 'production', statements: ['ALTER TABLE `shop`.`orders` ENGINE=`BLACKHOLE`'] } };
      return { statements: 1, transactional: false, outputs: [], durationMs: 1 };
    }
    throw new Error(`unexpected ${command}`);
  }),
}));
vi.mock('../../composables/useDialogs', () => ({
  useDialogs: () => ({
    askDestructive: vi.fn(async (request: { statements: string[]; warnings?: string[] }) => {
      asked.push(request);
      return true;
    }),
  }),
}));

describe('plan flow', () => {
  it('shows the step notes (row count) and the plan warnings in the destructive confirmation', async () => {
    const change: EngineChange = { engine: 'mysql', change: { change: 'alterTable', database: 'shop', name: 'orders', operations: [{ op: 'setOptions', options: { engine: 'BLACKHOLE' } }] } };
    const steps = await planSteps('s1', [change], [['orders holds 2 rows: BLACKHOLE keeps none of them. The rows will be lost.']]);
    expect(steps[0]?.notes).toEqual(['orders holds 2 rows: BLACKHOLE keeps none of them. The rows will be lost.']);
    const result = await applySteps('s1', steps, { connectionName: 'Shop', environment: 'production' });
    expect(result).toEqual({ kind: 'done' });
    expect(asked).toHaveLength(1);
    expect(asked[0]?.warnings).toEqual([
      'orders holds 2 rows: BLACKHOLE keeps none of them. The rows will be lost.',
      'BLACKHOLE stores nothing: every row of the table is discarded, and rows written later are thrown away. The rows will be lost',
    ]);
  });
});
