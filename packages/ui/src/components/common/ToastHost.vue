<script setup lang="ts">
import { useToasts } from '../../composables/useToasts';
import Icon from './Icon.vue';

const { toasts, dismiss } = useToasts();
</script>

<template>
  <div
    class="toasts"
    role="region"
    aria-label="Notifications"
    aria-live="polite"
  >
    <div
      v-for="t in toasts"
      :key="t.id"
      class="toast"
      :class="`edge-${t.tone}`"
      :role="t.tone === 'danger' ? 'alert' : 'status'"
    >
      <div class="text">
        <strong>{{ t.title }}</strong>
        <span v-if="t.message">{{ t.message }}</span>
      </div>
      <button
        type="button"
        class="icon-btn"
        aria-label="Dismiss notification"
        @click="dismiss(t.id)"
      >
        <Icon name="close" />
      </button>
    </div>
  </div>
</template>

<style scoped>
.toasts {
  position: fixed;
  right: 16px;
  bottom: 36px;
  z-index: 300;
  display: flex;
  flex-direction: column;
  gap: 8px;
  width: 380px;
  pointer-events: none;
}

.toast {
  display: flex;
  gap: 8px;
  align-items: flex-start;
  padding: 10px 8px 10px 14px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
  box-shadow: var(--shadow-dialog);
  pointer-events: auto;
}

.edge-success {
  box-shadow: inset 3px 0 0 var(--success), var(--shadow-dialog);
}

.edge-info {
  box-shadow: inset 3px 0 0 var(--info), var(--shadow-dialog);
}

.edge-warning {
  box-shadow: inset 3px 0 0 var(--warning), var(--shadow-dialog);
}

.edge-danger {
  box-shadow: inset 3px 0 0 var(--danger), var(--shadow-dialog);
}

.text {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  overflow-wrap: anywhere;
}

.text span {
  font-size: 12px;
  color: var(--text-muted);
}
</style>
