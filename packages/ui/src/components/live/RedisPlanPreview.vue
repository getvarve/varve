<script setup lang="ts">
import type { ShownPlan } from './useRedisPlan';
import Icon from '../common/Icon.vue';

/**
 * The preview of a stored Redis plan: the commands as they will run (secret
 * values masked), its warnings, and Apply / Cancel. Apply runs exactly what
 * is shown.
 */
defineProps<{ shown: ShownPlan; busy: boolean }>();
const emit = defineEmits<{ apply: []; discard: [] }>();
</script>

<template>
  <div
    class="preview"
    role="region"
    :aria-label="`Preview: ${shown.title}`"
  >
    <div class="head">
      <span class="label">Preview</span>
      <span class="title">{{ shown.title }}</span>
      <span
        v-if="shown.plan.destructive"
        class="pill tone-danger"
      ><span class="dot" />Destructive</span>
      <span
        v-if="shown.plan.sensitive"
        class="pill tone-neutral"
      ><Icon name="lock" />Secret masked</span>
    </div>
    <pre class="code">{{ shown.plan.display.join('\n') }}</pre>
    <ul
      v-if="shown.plan.warnings.length > 0"
      class="warnings"
    >
      <li
        v-for="(w, i) in shown.plan.warnings"
        :key="i"
        class="notice tone-warning"
      >
        <Icon name="warning" />{{ w }}
      </li>
    </ul>
    <div class="actions">
      <span class="grow" />
      <button
        type="button"
        class="btn compact"
        :disabled="busy"
        @click="emit('discard')"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn compact"
        :class="shown.plan.destructive ? 'danger' : 'primary'"
        :disabled="busy"
        @click="emit('apply')"
      >
        Apply
      </button>
    </div>
  </div>
</template>

<style scoped>
.preview {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--accent-tint);
}

.head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.title {
  font-weight: 500;
}

.code {
  margin: 0;
  padding: 8px 10px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
  background: var(--surface);
  font-family: var(--font-mono);
  font-size: 12px;
  white-space: pre-wrap;
  word-break: break-word;
}

.warnings {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.actions {
  display: flex;
  gap: 8px;
}

.grow {
  flex: 1;
}
</style>
