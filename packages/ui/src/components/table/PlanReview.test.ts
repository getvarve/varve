import { mount } from '@vue/test-utils';
import { defineComponent, h } from 'vue';
import type { EditPlan } from '../../api/grid';
import PlanReview from './PlanReview.vue';
import type { PlanStep } from './planFlow';

/** The dialog frame without its teleport: the review's content and footer in place. */
const ModalStub = defineComponent({
  name: 'Modal',
  setup(_props, { slots }) {
    return () => h('div', [slots.default?.(), slots.footer?.()]);
  },
});

const change = { engine: 'mysql', change: { change: 'alterTable', name: 'orders', operations: [] } } as unknown as PlanStep['change'];

function step(planId: string, plan: EditPlan): PlanStep {
  return { label: `Step ${planId}`, change, planId, plan, notes: [], status: 'pending', error: null, result: null };
}

function review(steps: PlanStep[]) {
  return mount(PlanReview, {
    props: { title: 'Save changes to orders', steps, busy: false, message: null, applyLabel: 'Apply' },
    global: { stubs: { Modal: ModalStub } },
  });
}

describe('PlanReview', () => {
  it('shows the readable statements of a plan, not the statements it runs as', () => {
    const runs = ['SET @old = @@foreign_key_checks', 'ALTER TABLE `orders` ADD COLUMN `note` text', 'SET foreign_key_checks = @old'];
    const wrapper = review([step('p1', { statements: runs, shown: ['ALTER TABLE `orders` ADD COLUMN `note` text'], transactional: false, destructive: false, warnings: [] })]);
    const sql = wrapper.get('pre.sql').text();
    expect(sql).toBe('ALTER TABLE `orders` ADD COLUMN `note` text;');
    expect(sql).not.toContain('foreign_key_checks');
    // How the step runs is still judged by what runs.
    expect(wrapper.text()).toContain('Statement by statement');
  });

  it('shows the statements of a plan without readable ones', () => {
    const wrapper = review([step('p1', { statements: ['CREATE TABLE "t" ("id" integer)', 'COMMENT ON TABLE "t" IS \'x\''], transactional: true, destructive: false, warnings: [] })]);
    expect(wrapper.get('pre.sql').text()).toBe('CREATE TABLE "t" ("id" integer);\nCOMMENT ON TABLE "t" IS \'x\';');
  });
});
