import { flushPromises } from '@vue/test-utils';
import type * as RedisModule from '../../api/redis';
import type { RedisEditOutcome, RedisEditRequest } from '../../api/redis';
import { useDialogs } from '../../composables/useDialogs';
import type { OutcomeView } from '../../lib/redisEdit';
import { useKeyEdit } from './useKeyEdit';

const applyCalls: { planId: string; confirmed: boolean }[] = [];
let applyAnswers: unknown[] = [];
let planAnswer: unknown = null;

vi.mock('../../api/redis', async (original) => ({
  ...(await original<typeof RedisModule>()),
  redisEditPlan: vi.fn(async (_session: string, request: RedisEditRequest) => {
    if (planAnswer) throw planAnswer;
    return {
      planId: 'p1',
      plan: { key: request.key, db: 0, commands: ['HDEL user:1 name'], checks: ['user:1 is a hash'], risk: 'write', destructive: true, warnings: [] },
    };
  }),
  redisEditApply: vi.fn(async (_session: string, planId: string, confirmed: boolean) => {
    applyCalls.push({ planId, confirmed });
    const answer = applyAnswers.shift();
    if (answer instanceof Error || (typeof answer === 'object' && answer !== null && 'kind' in answer)) throw answer;
    return answer as RedisEditOutcome;
  }),
}));

const request: RedisEditRequest = {
  key: 'user:1',
  expect: { state: 'present', keyType: 'hash', version: { keyType: 'hash', length: '2', digest: 'abc', digestMissing: null, expireAtMs: null } },
  edits: [{ op: 'hashDelete', field: { t: 'text', v: 'name' }, original: { t: 'text', v: 'Kim' } }],
};

function setup() {
  const done: OutcomeView[] = [];
  const edit = useKeyEdit({ tabId: () => 'tab-x', sessionId: () => 's1', connectionName: () => 'Cache', done: (v) => done.push(v) });
  return { edit, done };
}

beforeEach(() => {
  applyCalls.length = 0;
  applyAnswers = [];
  planAnswer = null;
});

describe('useKeyEdit', () => {
  it('previews the plan, asks for a destructive confirmation and applies with confirmed', async () => {
    const { edit, done } = setup();
    expect(await edit.review(request)).toBe(true);
    expect(edit.state.planned?.plan.commands).toEqual(['HDEL user:1 name']);
    applyAnswers = [{ kind: 'session', error: { kind: 'confirmationRequired', environment: 'production', statements: ['HDEL'] } }, { status: 'applied', replies: [{ t: 'int', v: 1 }] }];
    const run = edit.apply();
    await flushPromises();
    const dialogs = useDialogs();
    const open = dialogs.dialogs.at(-1);
    expect(open?.spec.type).toBe('destructive');
    if (open) dialogs.close(open.id, true);
    await run;
    expect(applyCalls).toEqual([
      { planId: 'p1', confirmed: false },
      { planId: 'p1', confirmed: true },
    ]);
    expect(done[0]).toMatchObject({ tone: 'success', message: '1 change applied.' });
    expect(edit.state.planned).toBeNull();
  });

  it('keeps the preview when the confirmation is declined', async () => {
    const { edit, done } = setup();
    await edit.review(request);
    applyAnswers = [{ kind: 'session', error: { kind: 'confirmationRequired', environment: 'development', statements: ['HDEL'] } }];
    const run = edit.apply();
    await flushPromises();
    const dialogs = useDialogs();
    const open = dialogs.dialogs.at(-1);
    if (open) dialogs.close(open.id, false);
    await run;
    expect(edit.state.planned?.planId).toBe('p1');
    expect(done).toEqual([]);
  });

  it('reports a conflict and re-reads', async () => {
    const { edit, done } = setup();
    await edit.review(request);
    applyAnswers = [{ status: 'conflict', edit: 0, reason: 'valueChanged', message: 'Field name changed.' }];
    await edit.apply();
    expect(done[0]).toMatchObject({ tone: 'warning', reread: true });
    expect(done[0]?.message).toContain('Delete field name');
  });

  it('keeps the preview on a MULTI_OPEN refusal and shows the hint', async () => {
    const { edit, done } = setup();
    await edit.review(request);
    applyAnswers = [{ kind: 'session', error: { kind: 'driver', error: { kind: 'query', message: 'A MULTI transaction is open', code: 'MULTI_OPEN', detail: null, hint: null, position: null } } }];
    await edit.apply();
    expect(edit.state.planned).not.toBeNull();
    expect(edit.state.error?.hint).toContain('EXEC or DISCARD');
    expect(done).toEqual([]);
  });

  it('asks with the warning for an unchecked plan and applies after a yes', async () => {
    const { edit, done } = setup();
    await edit.review(request);
    const warning = 'This key has no digest (too large), so a change that keeps its length would be lost.';
    applyAnswers = [{ kind: 'session', error: { kind: 'confirmationRequired', environment: 'development', statements: [warning] } }, { status: 'applied', replies: [] }];
    const run = edit.apply();
    await flushPromises();
    const dialogs = useDialogs();
    const open = dialogs.dialogs.at(-1);
    expect(open?.spec.type).toBe('confirm');
    expect(open?.spec.type === 'confirm' ? open.spec.message : '').toBe(warning);
    if (open) dialogs.close(open.id, true);
    await run;
    expect(applyCalls.at(-1)).toEqual({ planId: 'p1', confirmed: true });
    expect(done[0]?.tone).toBe('success');
  });

  it('shows the warning in the destructive dialog of an unchecked plan', async () => {
    const { edit } = setup();
    await edit.review(request);
    const warning = 'The key cannot be checked for changes made since it was read: reading it in the background had not finished.';
    applyAnswers = [{ kind: 'session', error: { kind: 'confirmationRequired', environment: 'production', statements: ['DEL', warning] } }, { status: 'applied', replies: [] }];
    const run = edit.apply();
    await flushPromises();
    const dialogs = useDialogs();
    const open = dialogs.dialogs.at(-1);
    expect(open?.spec.type).toBe('destructive');
    expect(open?.spec.type === 'destructive' ? open.spec.statements : []).toEqual(['DEL']);
    expect(open?.spec.type === 'destructive' ? open.spec.warnings : []).toEqual([warning]);
    if (open) dialogs.close(open.id, true);
    await run;
    expect(applyCalls.at(-1)).toEqual({ planId: 'p1', confirmed: true });
  });

  it('keeps the preview and shows the hint when a group check script is not permitted', async () => {
    const { edit, done } = setup();
    await edit.review(request);
    applyAnswers = [{ kind: 'session', error: { kind: 'driver', error: { kind: 'query', message: 'NOPERM eval', code: 'NOPERM', detail: null, hint: 'Allow EVAL for this user.', position: null } } }];
    await edit.apply();
    expect(edit.state.planned).not.toBeNull();
    expect(edit.state.error?.hint).toBe('Allow EVAL for this user.');
    expect(done).toEqual([]);
  });

  it('shows a read-only refusal of the plan', async () => {
    const { edit } = setup();
    planAnswer = { kind: 'readOnly', message: 'This connection is read-only.' };
    expect(await edit.review(request)).toBe(false);
    expect(edit.state.error?.title).toBe('Read-only connection');
    expect(edit.state.planned).toBeNull();
  });

  it('keeps the staged edits after an expired plan, says nothing ran and plans again', async () => {
    const { edit, done } = setup();
    expect(await edit.review(request)).toBe(true);
    applyAnswers = [{ kind: 'notFound', message: 'This plan is no longer available; plan the change again.' }];
    await edit.apply();
    expect(done).toEqual([]);
    expect(edit.state.planned?.planId).toBe('p1');
    expect(edit.state.edits).toEqual(request.edits);
    expect(edit.state.error?.title).toBe('The preview expired');
    expect(edit.state.error?.message).toMatch(/nothing was changed/);
    expect(edit.state.error?.refused).toBe(true);
    // The fresh preview applies normally.
    applyAnswers = [{ status: 'applied', replies: [{ t: 'int', v: 1 }] }];
    await edit.apply();
    expect(done).toHaveLength(1);
    expect(applyCalls.map((c) => c.planId)).toEqual(['p1', 'p1']);
  });
});
