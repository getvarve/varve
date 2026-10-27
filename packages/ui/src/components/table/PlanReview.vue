<script setup lang="ts">
import { computed } from 'vue';
import Modal from '../common/Modal.vue';
import type { PlanStep } from './planFlow';

/**
 * Shows the SQL the server planned for designer changes, step by step,
 * with warnings and what each step does to locks and transactions, and
 * applies them on request. The parent runs the apply and updates the
 * steps' statuses; this dialog only shows them.
 */
const props = defineProps<{
  /** Dialog title, for example "Save changes to public.orders". */
  title: string;
  steps: PlanStep[];
  /** True while the steps are being applied. */
  busy: boolean;
  /** Outcome or refusal to show above the steps (open transaction, failure). */
  message: { tone: 'danger' | 'warning' | 'success'; text: string } | null;
  /** Label of the apply button. */
  applyLabel: string;
}>();
const emit = defineEmits<{ apply: []; close: [] }>();

const pending = computed(() => props.steps.filter((s) => s.status === 'pending').length);
const done = computed(() => props.steps.length > 0 && props.steps.every((s) => s.status === 'applied'));
const destructive = computed(() => props.steps.some((s) => s.status === 'pending' && s.plan.destructive));

const STATUS: Record<PlanStep['status'], { text: string; tone: string }> = {
  pending: { text: 'Not run yet', tone: 'tone-neutral' },
  running: { text: 'Running', tone: 'tone-info' },
  applied: { text: 'Applied', tone: 'tone-success' },
  failed: { text: 'Failed', tone: 'tone-danger' },
  skipped: { text: 'Not run', tone: 'tone-warning' },
};
</script>

<template>
  <Modal
    :title="title"
    :width="760"
    @close="!busy && emit('close')"
  >
    <div class="review">
      <p
        v-if="message"
        class="notice"
        :class="`tone-${message.tone}`"
        role="status"
      >
        {{ message.text }}
      </p>
      <p
        v-if="steps.length > 1"
        class="muted lede"
      >
        {{ steps.length }} steps run one after another. A step that fails stops the rest; steps before it stay applied.
      </p>
      <section
        v-for="(step, i) in steps"
        :key="step.planId"
        class="step"
        :aria-label="`Step ${i + 1}: ${step.label}`"
      >
        <header class="step-head">
          <span class="step-n mono">{{ i + 1 }}</span>
          <span class="step-label">{{ step.label }}</span>
          <span
            v-if="step.plan.transactional"
            class="pill tone-neutral"
            title="Runs as one transaction: all statements or none"
          >One transaction</span>
          <span
            v-else-if="step.plan.statements.length > 1"
            class="pill tone-warning"
            title="Statements run one by one; a failure leaves the ones before it applied"
          >Statement by statement</span>
          <span
            v-if="step.plan.destructive"
            class="pill tone-danger"
          ><span class="dot" />Destructive</span>
          <span class="spacer" />
          <span
            class="pill"
            :class="STATUS[step.status].tone"
          >{{ STATUS[step.status].text }}</span>
        </header>
        <pre
          class="sql mono"
          :aria-label="`SQL of step ${i + 1}`"
        >{{ step.plan.statements.map((s) => `${s};`).join('\n') }}</pre>
        <ul
          v-if="step.notes.length > 0"
          class="notice tone-danger notes"
          role="note"
        >
          <li
            v-for="n in step.notes"
            :key="n"
          >
            {{ n }}
          </li>
        </ul>
        <ul
          v-if="step.plan.warnings.length > 0"
          class="warnings"
        >
          <li
            v-for="w in step.plan.warnings"
            :key="w"
          >
            {{ w }}
          </li>
        </ul>
        <p
          v-if="step.error"
          class="notice tone-danger"
        >
          {{ step.error }}
        </p>
      </section>
    </div>

    <template #footer>
      <span
        v-if="destructive && !done"
        class="subtle foot-note"
      >Destructive steps ask for confirmation before they run.</span>
      <button
        type="button"
        class="btn"
        :disabled="busy"
        @click="emit('close')"
      >
        {{ done ? 'Close' : 'Cancel' }}
      </button>
      <button
        v-if="!done"
        type="button"
        class="btn primary"
        :disabled="busy || pending === 0"
        data-autofocus
        @click="emit('apply')"
      >
        {{ busy ? 'Applying…' : applyLabel }}
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.notes {
  flex-direction: column;
  margin: 0;
  padding-left: 28px;
}

.review {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.lede {
  margin: 0;
  font-size: 12px;
}

.step {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.step-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.step-n {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  border-radius: 50%;
  background: var(--surface-raised);
  color: var(--text-muted);
  font-size: 11px;
}

.step-label {
  font-weight: 500;
}

.spacer {
  flex: 1;
}

.sql {
  margin: 0;
  max-height: 240px;
  padding: 10px 12px;
  overflow: auto;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
  background: var(--ground);
  font-size: 12px;
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-word;
}

.warnings {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0;
  padding: 8px 12px 8px 28px;
  border-radius: var(--radius-control);
  background: var(--warning-tint);
  color: var(--warning-text);
  font-size: 12px;
}

.notice {
  margin: 0;
}

.foot-note {
  margin-right: auto;
  font-size: 12px;
}
</style>
