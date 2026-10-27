<script setup lang="ts">
import { useId } from 'vue';
import Modal from '../common/Modal.vue';

/** Asks what to do with unsaved edits when a tab closes. */
defineProps<{ title: string; saved: boolean }>();
const emit = defineEmits<{ done: [answer: 'save' | 'discard' | null] }>();
const messageId = useId();
</script>

<template>
  <Modal
    :title="`Save changes to “${title}”?`"
    :width="480"
    role="alertdialog"
    :described-by="messageId"
    @close="emit('done', null)"
  >
    <p
      :id="messageId"
      class="message"
    >
      {{
        saved
          ? 'The editor text differs from the saved query. Your changes are lost if you close without saving.'
          : 'This tab has text that is not saved as a query. It is lost if you close without saving.'
      }}
    </p>
    <template #footer>
      <button
        type="button"
        class="btn ghost"
        @click="emit('done', 'discard')"
      >
        Discard
      </button>
      <span class="spacer" />
      <button
        type="button"
        class="btn"
        data-autofocus
        @click="emit('done', null)"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn primary"
        @click="emit('done', 'save')"
      >
        {{ saved ? 'Save' : 'Save as…' }}
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.message {
  margin: 0;
  color: var(--text-muted);
}

.spacer {
  flex: 1;
}
</style>
