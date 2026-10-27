import type * as DdlModule from '../../api/ddl';
import type { EngineChange } from '../../api/ddl';
import type { SessionInfo } from '../../api/sessions';
import { useChangePlan } from './useChangePlan';

const planned: string[] = [];
const applied: string[] = [];
let applyAnswers: unknown[] = [];
let destructive = false;
let confirmAnswer = true;
const asked: { warnings?: string[] }[] = [];

vi.mock('../../api/ddl', async (original) => ({
  ...(await original<typeof DdlModule>()),
  editPlan: vi.fn(async () => {
    const planId = `p${planned.length + 1}`;
    planned.push(planId);
    return { planId, plan: { statements: ['ALTER DATABASE shop RENAME TO store'], transactional: false, destructive, warnings: [] } };
  }),
  editApply: vi.fn(async (_session: string, planId: string, confirmed: boolean) => {
    if (destructive && !confirmed) {
      throw { kind: 'session', error: { kind: 'confirmationRequired', environment: 'development', statements: ['Drop'] } };
    }
    applied.push(planId);
    const answer = applyAnswers.shift();
    if (answer !== undefined) throw answer;
    return { statements: 1, transactional: false, outputs: [], durationMs: 1 };
  }),
}));
vi.mock('../../composables/useDialogs', () => ({
  useDialogs: () => ({
    askDestructive: vi.fn(async (request: { warnings?: string[] }) => {
      asked.push(request);
      return confirmAnswer;
    }),
  }),
}));
vi.mock('../notices/useNotices', () => ({ useNotices: () => ({ refreshRestores: vi.fn(async () => undefined) }) }));

const change = { engine: 'postgres', change: { object: 'database', action: 'alter', name: 'shop', operations: [] } } as unknown as EngineChange;

describe('useChangePlan', () => {
  beforeEach(() => {
    planned.length = 0;
    applied.length = 0;
    asked.length = 0;
    applyAnswers = [];
    destructive = false;
    confirmAnswer = true;
  });

  it('prepares a destructive change only after the user confirmed it, and shows the extra warnings', async () => {
    destructive = true;
    const beforeApply = vi.fn(async () => undefined);
    const changes = useChangePlan({
      session: () => ({ id: 's1' }) as SessionInfo,
      connectionName: () => 'Shop',
      onChanged: () => undefined,
      beforeApply,
      confirmWarnings: () => ['Ends tab "Shop".'],
    });
    await changes.plan([{ kind: 'edit', change }]);
    confirmAnswer = false;
    expect(await changes.apply()).toBe(false);
    // Declined: nothing ran and nothing was prepared (the tree keeps its sessions).
    expect(beforeApply).not.toHaveBeenCalled();
    expect(applied).toEqual([]);
    expect(asked[0]?.warnings).toEqual(['Ends tab "Shop".']);
    confirmAnswer = true;
    expect(await changes.apply()).toBe(true);
    expect(beforeApply).toHaveBeenCalledTimes(1);
    expect(applied).toEqual(['p1']);
  });

  it('prepares a change that needs no confirmation before it runs', async () => {
    const beforeApply = vi.fn(async () => {
      expect(applied).toEqual([]);
    });
    const changes = useChangePlan({ session: () => ({ id: 's1' }) as SessionInfo, connectionName: () => 'Shop', onChanged: () => undefined, beforeApply });
    await changes.plan([{ kind: 'edit', change }]);
    expect(await changes.apply()).toBe(true);
    expect(beforeApply).toHaveBeenCalledTimes(1);
  });

  it('Retry for a database in use plans the change again and runs it', async () => {
    const changes = useChangePlan({ session: () => ({ id: 's1' }) as SessionInfo, connectionName: () => 'Shop', onChanged: () => undefined });
    expect(await changes.plan([{ kind: 'edit', change }])).toBe(true);
    applyAnswers = [
      {
        kind: 'session',
        error: { kind: 'driver', error: { kind: 'query', message: 'database "shop" is being accessed by other users', code: '55006', detail: 'The database is in use by 1 other session.', hint: null, position: null } },
      },
    ];
    expect(await changes.apply()).toBe(false);
    expect(changes.state.problem).toMatchObject({ title: 'Database in use', action: 'rerun', actionLabel: 'Retry' });
    await changes.retry();
    // Planned again (the first plan was spent) and run, without a second click.
    expect(planned).toEqual(['p1', 'p2']);
    expect(applied).toEqual(['p1', 'p2']);
    expect(changes.state.applied).toBe(true);
    expect(changes.state.problem).toBeNull();
  });
});
