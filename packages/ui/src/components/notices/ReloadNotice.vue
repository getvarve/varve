<script setup lang="ts">
import { onMounted } from 'vue';
import Icon from '../common/Icon.vue';
import { useReloadNotice } from './reloadNotice';

/**
 * After a reload of the window: what the reload closed (the backend closes
 * every session of the previous page and rolls back its open transactions).
 * Read once per page load and kept until dismissed.
 */
const notice = useReloadNotice();
const text = notice.text;

onMounted(() => notice.load());
</script>

<template>
  <p
    v-if="text"
    class="notice tone-warning reload"
    role="status"
  >
    <span class="text">{{ text }}</span>
    <button
      type="button"
      class="icon-btn"
      title="Dismiss"
      aria-label="Dismiss"
      @click="notice.dismiss()"
    >
      <Icon name="close" />
    </button>
  </p>
</template>

<style scoped>
.reload {
  display: flex;
  gap: 8px;
  align-items: center;
  margin: 8px 12px;
}

.text {
  flex: 1;
}
</style>
