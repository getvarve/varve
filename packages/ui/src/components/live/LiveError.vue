<script setup lang="ts">
import { computed } from 'vue';
import { presentError } from '../../api/errors';
import type { CmdError } from '../../api/errors';
import Icon from '../common/Icon.vue';

/**
 * A panel's error with the server's code, detail and hint. For
 * `reconnectRequired` it offers "Reconnect" (`reconnect` event; the tab host
 * closes the tab's session and opens a new one).
 */
const props = defineProps<{ error: CmdError }>();
const emit = defineEmits<{ reconnect: [] }>();

const presented = computed(() => presentError(props.error));
const reconnect = computed(() => props.error.kind === 'reconnectRequired');
</script>

<template>
  <div
    class="notice tone-danger live-error"
    role="alert"
  >
    <Icon name="warning" />
    <div class="body">
      <strong>{{ presented.title }}</strong>
      <span>{{ presented.message }}</span>
      <span
        v-if="presented.code"
        class="mono subtle-line"
      >Code {{ presented.code }}</span>
      <span
        v-if="presented.detail"
        class="subtle-line"
      >{{ presented.detail }}</span>
      <span
        v-if="presented.hint"
        class="subtle-line"
      >Hint: {{ presented.hint }}</span>
    </div>
    <button
      v-if="reconnect"
      type="button"
      class="btn compact"
      @click="emit('reconnect')"
    >
      Reconnect
    </button>
  </div>
</template>

<style scoped>
.live-error {
  align-items: flex-start;
}

.body {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  word-break: break-word;
}

.subtle-line {
  color: var(--text);
  opacity: 0.85;
}
</style>
