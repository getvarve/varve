import type * as LiveModule from '../../api/live';
import type { Planned, RedisPlanView } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { toCmdError } from '../../api/errors';
import { useRedisPlan } from './useRedisPlan';

let applyAnswer: unknown = null;
const applied: string[] = [];

vi.mock('../../api/live', async (original) => ({
  ...(await original<typeof LiveModule>()),
  redisPlanApply: vi.fn(async (_session: string, planId: string) => {
    applied.push(planId);
    if (applyAnswer) throw applyAnswer;
    return [];
  }),
}));

const view: RedisPlanView = { display: ['CONFIG SET maxmemory 1gb'], destructive: false, sensitive: false, warnings: [] };
let planned = 0;
const make = (): Promise<Planned<RedisPlanView>> => {
  planned += 1;
  return Promise.resolve({ planId: `p${planned}`, plan: view });
};

function setup() {
  return useRedisPlan(
    () => ({ id: 's1' }) as SessionInfo,
    () => 'Cache',
    (e) => toCmdError(e),
  );
}

beforeEach(() => {
  applyAnswer = null;
  applied.length = 0;
  planned = 0;
});

describe('useRedisPlan', () => {
  it('drops the spent plan after a failed apply, shows the failing command and plans again on request', async () => {
    const plans = setup();
    expect(await plans.plan('Set maxmemory', make)).toBe(true);
    applyAnswer = { kind: 'session', error: { kind: 'driver', error: { kind: 'query', message: 'ERR value out of range', code: 'ERR', detail: 'command 2 of 2 failed', position: null, partial: null } } };
    expect(await plans.apply()).toBeNull();
    // The server took p1 when it started; it is not offered again.
    expect(plans.shown.value).toBeNull();
    expect(plans.error.value).toContain('command 2 of 2 failed');
    expect(plans.canReplan.value).toBe(true);
    expect(await plans.replan()).toBe(true);
    expect(plans.shown.value?.planId).toBe('p2');
    expect(plans.error.value).toBe('');
    applyAnswer = null;
    expect(await plans.apply()).toEqual([]);
    expect(applied).toEqual(['p1', 'p2']);
    expect(plans.canReplan.value).toBe(false);
  });
});
