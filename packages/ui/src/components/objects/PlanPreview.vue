<script setup lang="ts">
import { computed, useId } from 'vue';
import type { Engine } from '../../api/records';
import { useToasts } from '../../composables/useToasts';
import { stepsNotes, stepsSql, stepsWarnings } from '../../lib/ddlApply';
import Icon from '../common/Icon.vue';
import SqlCode from './SqlCode.vue';
import type { ChangePlan } from './useChangePlan';

/**
 * "Will run": the SQL the server planned for an editor's change, its
 * warnings and how it runs, the apply button and what went wrong. The SQL
 * shown is exactly what `edit_apply` runs.
 */
const props = withDefaults(
  defineProps<{
    plan: ChangePlan;
    engine: Engine;
    readOnly: boolean;
    /** Label of the apply button. */
    applyLabel?: string;
    /** Text when there is nothing to plan. */
    emptyText?: string;
    /** Form problems that block planning (shown instead of the SQL). */
    blocked?: string | null;
  }>(),
  { applyLabel: 'Apply', emptyText: 'No changes yet.', blocked: null },
);
const emit = defineEmits<{ applied: []; reconnect: []; refresh: [] }>();

const toasts = useToasts();
const headingId = useId();
const state = computed(() => props.plan.state);
const sql = computed(() => stepsSql(state.value.steps));
const warnings = computed(() => stepsWarnings(state.value.steps));
const notes = computed(() => stepsNotes(state.value.steps, props.engine));
const destructive = computed(() => state.value.steps.some((s) => s.plan.destructive));
const problem = computed(() => state.value.problem ?? state.value.planProblem);
const pending = computed(() => state.value.steps.some((s) => s.status !== 'done'));
const canApply = computed(
  () => !props.readOnly && !props.blocked && !state.value.planning && !state.value.applying && pending.value && state.value.planProblem === null,
);
const statusText = computed(() => {
  if (state.value.applying) return 'Running';
  if (state.value.planning) return 'Planning';
  if (state.value.applied) return 'Applied';
  return '';
});

async function copy(): Promise<void> {
  try {
    await navigator.clipboard.writeText(sql.value);
    toasts.push('success', 'Copied the SQL');
  } catch {
    toasts.push('warning', 'Could not copy to the clipboard');
  }
}

async function apply(): Promise<void> {
  if (await props.plan.apply()) emit('applied');
}

async function act(): Promise<void> {
  const action = problem.value?.action;
  if (action === 'reconnect') emit('reconnect');
  else if (action === 'refresh') emit('refresh');
  else await props.plan.retry();
}
</script>

<template>
  <section
    class="preview"
    :aria-labelledby="headingId"
    aria-live="polite"
  >
    <header class="head">
      <h2 :id="headingId">
        Will run
      </h2>
      <span
        v-if="destructive"
        class="pill tone-danger"
      ><span class="dot" />Destructive</span>
      <span
        v-if="statusText"
        class="subtle status"
      >{{ statusText }}</span>
      <span class="grow" />
      <button
        type="button"
        class="btn compact"
        :disabled="sql === ''"
        @click="copy"
      >
        <Icon name="copy" />Copy
      </button>
    </header>

    <p
      v-if="blocked"
      class="notice tone-warning"
      role="note"
    >
      <Icon name="warning" />{{ blocked }}
    </p>
    <p
      v-else-if="state.steps.length === 0 && !state.planning && !problem"
      class="muted empty"
    >
      {{ emptyText }}
    </p>

    <SqlCode
      v-if="sql !== '' && !blocked"
      :model-value="sql"
      :engine="engine"
      label="SQL that will run"
      readonly
      :min-height="80"
      class="sql"
    />

    <ul
      v-if="warnings.length > 0 && !blocked"
      class="warnings"
      aria-label="Warnings"
    >
      <li
        v-for="w in warnings"
        :key="w"
        class="notice tone-warning"
      >
        <Icon name="warning" />{{ w }}
      </li>
    </ul>
    <ul
      v-if="notes.length > 0 && !blocked"
      class="notes"
    >
      <li
        v-for="n in notes"
        :key="n"
        class="muted"
      >
        {{ n }}
      </li>
    </ul>

    <div
      v-if="state.runWarnings.length > 0"
      class="notice tone-warning run-warnings"
      role="alert"
    >
      <Icon name="warning" />
      <div>
        <strong>The run finished with warnings</strong>
        <p
          v-for="w in state.runWarnings"
          :key="w"
        >
          {{ w }}
        </p>
      </div>
    </div>

    <div
      v-if="problem"
      class="notice tone-danger problem"
      role="alert"
    >
      <Icon name="warning" />
      <div class="problem-body">
        <strong>{{ problem.title }}</strong>
        <p>{{ problem.message }}</p>
        <p
          v-if="problem.detail"
          class="detail"
        >
          {{ problem.detail }}
        </p>
        <p
          v-if="problem.hint"
          class="detail"
        >
          {{ problem.hint }}
        </p>
        <p
          v-if="problem.code"
          class="detail mono"
        >
          {{ problem.code }}
        </p>
      </div>
      <button
        v-if="problem.action !== 'none'"
        type="button"
        class="btn compact"
        :disabled="state.applying || state.planning"
        @click="act"
      >
        {{ problem.actionLabel }}
      </button>
    </div>
    <p
      v-if="state.declined"
      class="notice tone-neutral"
      role="status"
    >
      Not confirmed. Steps before it ran; nothing after it did.
    </p>
    <p
      v-if="readOnly"
      class="notice tone-neutral"
    >
      <Icon name="lock" />This connection is read only. You can review the SQL but not run it.
    </p>

    <footer class="foot">
      <slot name="extra" />
      <span class="grow" />
      <button
        type="button"
        :class="['btn', destructive ? 'danger' : 'primary']"
        :disabled="!canApply"
        @click="apply"
      >
        {{ applyLabel }}
      </button>
    </footer>
  </section>
</template>

<style scoped>
.preview {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 16px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

.head,
.foot {
  display: flex;
  align-items: center;
  gap: 8px;
}

h2 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
}

.grow {
  flex-grow: 1;
}

.status {
  font-size: 12px;
}

.empty {
  margin: 0;
  font-size: 12px;
}

.sql {
  max-height: 320px;
}

.warnings,
.notes {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.notes li {
  font-size: 12px;
  line-height: 1.5;
}

.problem,
.run-warnings {
  align-items: flex-start;
}

.problem-body {
  display: flex;
  flex-direction: column;
  gap: 4px;
  flex-grow: 1;
  min-width: 0;
}

.problem p,
.run-warnings p {
  margin: 0;
  overflow-wrap: anywhere;
}

.detail {
  color: var(--text-muted);
}
</style>
