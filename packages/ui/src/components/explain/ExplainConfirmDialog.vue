<script setup lang="ts">
import { computed, ref, useId } from 'vue';
import type { Environment } from '../../api/records';
import { canConfirm, needsTypedName } from '../../lib/destructive';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import Modal from '../common/Modal.vue';

/**
 * Asks before `EXPLAIN ANALYZE` runs a statement the server could not prove
 * free of side effects. Lists every reason the backend gave, in its order
 * (the first says the statement runs for real inside a transaction that is
 * rolled back). Production needs the connection name typed.
 */
const props = defineProps<{ connectionName: string; environment: Environment; reasons: readonly string[] }>();
const emit = defineEmits<{ done: [answer: boolean] }>();

const typed = ref('');
const inputId = useId();
const descId = useId();
const typing = computed(() => needsTypedName(props.environment));
const enabled = computed(() => canConfirm(props.environment, props.connectionName, typed.value));
const lead = computed(() => props.reasons[0] ?? 'EXPLAIN ANALYZE runs the statement for real, inside a transaction that is rolled back afterwards.');
const rest = computed(() => props.reasons.slice(1));

function run(): void {
  if (enabled.value) emit('done', true);
}
</script>

<template>
  <Modal
    :title="`Run EXPLAIN ANALYZE on ${connectionName}?`"
    :width="560"
    role="alertdialog"
    :described-by="descId"
    @close="emit('done', false)"
  >
    <template #badges>
      <EnvBadge :environment="environment" />
    </template>
    <form
      class="content"
      @submit.prevent="run"
    >
      <p
        :id="descId"
        class="lede"
      >
        {{ lead }}
      </p>
      <ul
        v-if="rest.length > 0"
        class="reasons"
        aria-label="Why it needs confirmation"
      >
        <li
          v-for="(r, i) in rest"
          :key="i"
        >
          <Icon name="warning" />
          <span>{{ r }}</span>
        </li>
      </ul>
      <p class="hint">
        A rollback does not undo everything: effects outside the transaction (sequences, advisory locks, notifications, files, other connections) stay.
      </p>
      <div
        v-if="typing"
        class="typed"
      >
        <label :for="inputId">Type the connection name to confirm</label>
        <input
          :id="inputId"
          v-model="typed"
          class="input mono"
          autocomplete="off"
          spellcheck="false"
          :placeholder="connectionName"
          data-autofocus
        >
      </div>
    </form>
    <template #footer>
      <button
        type="button"
        class="btn"
        :data-autofocus="typing ? undefined : true"
        @click="emit('done', false)"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn danger"
        :disabled="!enabled"
        @click="run"
      >
        Run EXPLAIN ANALYZE
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.content {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.lede {
  margin: 0;
  color: var(--text);
}

.reasons {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.reasons li {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 8px 10px;
  border-radius: var(--radius-control);
  background: var(--warning-tint);
  color: var(--warning-text);
  font-size: 12px;
}

.reasons li span {
  color: var(--text);
}

.hint {
  margin: 0;
  color: var(--text-muted);
  font-size: 12px;
}

.typed {
  display: flex;
  flex-direction: column;
  gap: 6px;
  color: var(--text-muted);
  font-size: 12px;
}
</style>
