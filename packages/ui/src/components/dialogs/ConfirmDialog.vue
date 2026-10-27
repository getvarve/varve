<script setup lang="ts">
import { useId } from 'vue';
import Modal from '../common/Modal.vue';

defineProps<{ title: string; message: string; confirmLabel: string; danger: boolean }>();
const emit = defineEmits<{ done: [answer: boolean] }>();
const messageId = useId();
</script>

<template>
  <Modal
    :title="title"
    :width="480"
    role="alertdialog"
    :described-by="messageId"
    @close="emit('done', false)"
  >
    <p
      :id="messageId"
      class="message"
    >
      {{ message }}
    </p>
    <template #footer>
      <button
        type="button"
        class="btn"
        data-autofocus
        @click="emit('done', false)"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn"
        :class="danger ? 'danger' : 'primary'"
        @click="emit('done', true)"
      >
        {{ confirmLabel }}
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.message {
  margin: 0;
  color: var(--text-muted);
}
</style>
