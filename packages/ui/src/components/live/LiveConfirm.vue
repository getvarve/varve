<script setup lang="ts">
import { computed, ref, useId } from 'vue';
import type { Environment } from '../../api/records';
import EnvBadge from '../common/EnvBadge.vue';
import Modal from '../common/Modal.vue';

/**
 * Confirms an action of a live panel (cancel or terminate a backend, kill a
 * client, reset a log, apply a variable). `statement` is shown as the exact
 * command that runs. With `typeToConfirm`, the confirm button stays disabled
 * until that text is typed (production, design: PgActivity).
 */
const props = withDefaults(
  defineProps<{
    title: string;
    message: string;
    confirmLabel: string;
    environment: Environment;
    statement?: string;
    details?: readonly string[];
    typeToConfirm?: string;
    danger?: boolean;
  }>(),
  { statement: '', details: () => [], typeToConfirm: '', danger: true },
);
const emit = defineEmits<{ done: [answer: boolean] }>();

const typed = ref('');
const inputId = useId();
const descId = useId();
const enabled = computed(() => props.typeToConfirm === '' || typed.value.trim() === props.typeToConfirm);

function confirm(): void {
  if (enabled.value) emit('done', true);
}
</script>

<template>
  <Modal
    :title="title"
    :width="520"
    role="alertdialog"
    :described-by="descId"
    @close="emit('done', false)"
  >
    <template #badges>
      <EnvBadge :environment="environment" />
    </template>
    <form
      class="content"
      @submit.prevent="confirm"
    >
      <p
        :id="descId"
        class="message"
      >
        {{ message }}
      </p>
      <pre
        v-if="statement"
        class="statement mono"
        aria-label="Statement"
      >{{ statement }}</pre>
      <ul
        v-if="details.length > 0"
        class="details"
      >
        <li
          v-for="(d, i) in details"
          :key="i"
        >
          {{ d }}
        </li>
      </ul>
      <div
        v-if="typeToConfirm"
        class="typed"
      >
        <label :for="inputId">Type <span class="mono">{{ typeToConfirm }}</span> to confirm</label>
        <input
          :id="inputId"
          v-model="typed"
          class="input mono"
          autocomplete="off"
          spellcheck="false"
          data-autofocus
        >
      </div>
    </form>
    <template #footer>
      <button
        type="button"
        class="btn"
        :data-autofocus="typeToConfirm ? undefined : true"
        @click="emit('done', false)"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn"
        :class="danger ? 'danger' : 'primary'"
        :disabled="!enabled"
        @click="confirm"
      >
        {{ confirmLabel }}
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

.message {
  margin: 0;
  color: var(--text-muted);
}

.statement {
  margin: 0;
  padding: 8px 10px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
  background: var(--ground);
  font-size: 12px;
  white-space: pre-wrap;
  word-break: break-word;
}

.details {
  margin: 0;
  padding-left: 18px;
  color: var(--text);
  font-size: 12px;
}

.typed {
  display: flex;
  flex-direction: column;
  gap: 6px;
  font-size: 12px;
  color: var(--text-muted);
}
</style>
