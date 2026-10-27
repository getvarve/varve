<script setup lang="ts">
import { computed, useId } from 'vue';
import type { RowEditPlan } from '../../api/grid';
import type { Environment } from '../../api/records';
import Modal from '../common/Modal.vue';

/**
 * The review step of grid editing: every generated statement of the
 * planned row changes, the plan's warnings, and the option to check that
 * rows did not change since they were read. Apply runs them in one
 * transaction (the parent calls `row_edit_apply`).
 */
const props = defineProps<{
  /** The plan from `row_edit_plan`; `null` while it is being made. */
  plan: RowEditPlan | null;
  /** One label per planned change, in plan order ("Update row id = 7"). */
  labels: string[];
  /** Optimistic concurrency: compare original values (re-plans when changed). */
  checkOriginal: boolean;
  busy: boolean;
  /** Refusal or failure to show (open transaction, planning error). */
  message: { tone: 'danger' | 'warning'; text: string } | null;
  connectionName: string;
  environment: Environment;
}>();
const emit = defineEmits<{ apply: []; close: []; 'update:checkOriginal': [value: boolean] }>();
const uid = useId();

const counts = computed(() => {
  const c = { insert: 0, update: 0, delete: 0 };
  for (const change of props.plan?.changes ?? []) c[change.op] += 1;
  return c;
});
const begin = computed(() => (props.plan?.engine === 'postgres' ? 'BEGIN;' : 'START TRANSACTION;'));
</script>

<template>
  <Modal
    title="Review changes"
    :width="780"
    @close="!busy && emit('close')"
  >
    <div class="review">
      <p class="muted lede">
        Applied in one transaction on <strong>{{ connectionName }}</strong> ({{ environment }}):
        {{ counts.update }} updated, {{ counts.insert }} inserted, {{ counts.delete }} deleted. If any row does not match, nothing is applied.
      </p>
      <label class="check">
        <input
          :id="`${uid}-orig`"
          type="checkbox"
          :checked="checkOriginal"
          :disabled="busy"
          @change="emit('update:checkOriginal', ($event.target as HTMLInputElement).checked)"
        >
        <span>Only change rows nobody else changed since they were read
          <span class="subtle">(compares the original values; off: the key alone finds the row)</span></span>
      </label>
      <p
        v-if="message"
        class="notice"
        :class="`tone-${message.tone}`"
        role="alert"
      >
        {{ message.text }}
      </p>
      <p
        v-if="plan?.hasExpressions"
        class="notice tone-warning"
      >
        Some cells hold SQL expressions. They run on the server as written.
      </p>
      <ul
        v-if="plan && plan.warnings.length > 0"
        class="warnings"
      >
        <li
          v-for="w in plan.warnings"
          :key="w"
        >
          {{ w }}
        </li>
      </ul>
      <div
        v-if="!plan"
        class="muted"
      >
        Planning…
      </div>
      <div
        v-else
        class="script"
        aria-label="Statements"
      >
        <pre class="sql mono">{{ begin }}</pre>
        <div
          v-for="(c, i) in plan.changes"
          :key="i"
          class="change"
        >
          <div
            class="change-head"
            :class="`op-${c.op}`"
          >
            <span class="op">{{ c.op.toUpperCase() }}</span>
            <span>{{ labels[c.change] ?? '' }}</span>
            <span
              v-if="c.checksOriginal"
              class="pill tone-neutral"
            >checks original values</span>
          </div>
          <pre class="sql mono">{{ [c.lock, c.statement, c.refresh].filter((s) => s).map((s) => `${s};`).join('\n') }}</pre>
        </div>
        <pre class="sql mono">COMMIT;</pre>
      </div>
    </div>
    <template #footer>
      <button
        type="button"
        class="btn"
        :disabled="busy"
        @click="emit('close')"
      >
        Back to editing
      </button>
      <button
        type="button"
        class="btn primary"
        :disabled="busy || !plan"
        data-autofocus
        @click="emit('apply')"
      >
        {{ busy ? 'Applying…' : `Apply ${plan?.changes.length ?? 0} ${plan?.changes.length === 1 ? 'change' : 'changes'}` }}
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.review {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.lede {
  margin: 0;
}

.check {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  font-size: 13px;
}

.notice {
  margin: 0;
}

.warnings {
  margin: 0;
  padding: 8px 12px 8px 28px;
  border-radius: var(--radius-control);
  background: var(--warning-tint);
  color: var(--warning-text);
  font-size: 12px;
}

.script {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.change {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.change-head {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12px;
}

.op {
  font-size: 11px;
  font-weight: 600;
}

.op-insert .op {
  color: var(--success-text);
}

.op-update .op {
  color: var(--warning-text);
}

.op-delete .op {
  color: var(--danger-text);
}

.sql {
  margin: 0;
  padding: 8px 12px;
  border-radius: var(--radius-control);
  background: var(--ground);
  font-size: 12px;
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-word;
}
</style>
