import { mount } from '@vue/test-utils';
import { reactive } from 'vue';
import type { EditPlan, EngineChange } from '../../api/ddl';
import type { PlannedStep } from '../../lib/ddlApply';
import PlanPreview from './PlanPreview.vue';
import type { ChangePlan, ChangePlanState } from './useChangePlan';

const change = { engine: 'mysql', change: { change: 'alterView', name: 'v' } } as unknown as EngineChange;

function planOf(plans: EditPlan[]): ChangePlan {
  const steps: PlannedStep[] = plans.map((plan, i) => ({ spec: { kind: 'edit', change }, planId: `p${i}`, plan, status: 'pending' }));
  const state: ChangePlanState = reactive({
    steps,
    specs: steps.map((s) => s.spec),
    planning: false,
    stale: false,
    planProblem: null,
    applying: false,
    problem: null,
    runWarnings: [],
    applied: false,
    declined: false,
  });
  return {
    state,
    plan: async () => true,
    apply: async () => true,
    retry: async () => undefined,
    planRemaining: async () => true,
    clear: () => undefined,
    markStale: () => undefined,
  };
}

function preview(plans: EditPlan[]) {
  return mount(PlanPreview, {
    props: { plan: planOf(plans), engine: 'mysql', readOnly: false },
    global: { stubs: { SqlCode: true } },
  });
}

describe('PlanPreview', () => {
  it('shows the readable statements of a plan, not the statements it runs as', () => {
    const runs = ['SET @saved = @@sql_mode', 'ALTER VIEW `v` AS SELECT 1', 'SET sql_mode = @saved'];
    const wrapper = preview([{ statements: runs, shown: ['ALTER VIEW `v` AS SELECT 1'], transactional: false, destructive: false, warnings: [] }]);
    expect(wrapper.findComponent({ name: 'SqlCode' }).props('modelValue')).toBe('ALTER VIEW `v` AS SELECT 1;');
  });

  it('shows the statements of a plan without readable ones', () => {
    const wrapper = preview([{ statements: ['DROP VIEW `v`', 'CREATE VIEW `v` AS SELECT 1'], transactional: false, destructive: true, warnings: [] }]);
    expect(wrapper.findComponent({ name: 'SqlCode' }).props('modelValue')).toBe('DROP VIEW `v`;\n\nCREATE VIEW `v` AS SELECT 1;');
  });
});
