<script setup lang="ts">
import { computed, useId } from 'vue';
import type { RedisEditPlan } from '../../api/redis';
import { planHeadline } from '../../lib/redisEdit';
import type { RedisErrorView } from '../../lib/redisErrors';
import Modal from '../common/Modal.vue';

/** Preview of a Redis edit plan: the commands that will run and what is checked first. */
const props = defineProps<{ plan: RedisEditPlan; applying: boolean; error: RedisErrorView | null }>();
const emit = defineEmits<{ apply: []; cancel: [] }>();

const descId = useId();
const risky = computed(() => props.plan.destructive || props.plan.risk === 'destructive');
</script>

<template>
  <Modal
    :title="`Apply changes to ${plan.key}?`"
    :width="640"
    :described-by="descId"
    @close="emit('cancel')"
  >
    <div class="content">
      <p
        :id="descId"
        class="lede"
      >
        {{ planHeadline(plan) }}. They run together in one MULTI/EXEC on their own connection, only if the checks below
        still hold.
        <template v-if="risky">
          <strong>This removes or overwrites data.</strong>
        </template>
      </p>
      <div
        v-if="plan.unchecked"
        class="notice tone-danger unchecked"
        role="alert"
      >
        <strong>Changes by others cannot be checked for this key</strong>
        <span
          v-for="(w, i) in plan.warnings"
          :key="i"
        >{{ w }}</span>
        <span>You are asked to confirm before it runs.</span>
      </div>
      <section>
        <h3 class="label">
          Checked first
        </h3>
        <ul class="checks">
          <li
            v-for="(c, i) in plan.checks"
            :key="i"
          >
            {{ c }}
          </li>
        </ul>
      </section>
      <section>
        <h3 class="label">
          Commands
        </h3>
        <pre
          class="mono commands"
          aria-label="Commands that will run"
        >{{ plan.commands.join('\n') }}</pre>
      </section>
      <ul
        v-if="plan.warnings.length > 0 && !plan.unchecked"
        class="warnings"
      >
        <li
          v-for="(w, i) in plan.warnings"
          :key="i"
          class="notice tone-warning"
        >
          {{ w }}
        </li>
      </ul>
      <div
        v-if="error"
        class="notice tone-danger error"
        role="alert"
      >
        <strong>{{ error.title }}</strong>
        <span>{{ error.message }}</span>
        <span v-if="error.hint">{{ error.hint }}</span>
      </div>
    </div>
    <template #footer>
      <button
        type="button"
        class="btn"
        :disabled="applying"
        data-autofocus
        @click="emit('cancel')"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn"
        :class="risky || plan.unchecked ? 'danger' : 'primary'"
        :disabled="applying"
        @click="emit('apply')"
      >
        {{ applying ? 'Applying…' : 'Apply' }}
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.content {
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.lede {
  margin: 0;
  color: var(--text-muted);
}

h3 {
  margin: 0 0 6px;
}

.checks {
  margin: 0;
  padding-left: 18px;
  font-size: 12px;
}

.commands {
  max-height: 240px;
  margin: 0;
  padding: 10px 12px;
  overflow: auto;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-panel);
  background: var(--ground);
  font-size: 12px;
  white-space: pre;
}

.warnings {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.error,
.unchecked {
  flex-direction: column;
  gap: 2px;
}
</style>
