<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { reloadReport } from '../../api/sessions';
import { useWorkspace } from '../../composables/useWorkspace';
import Icon from '../common/Icon.vue';
import { reloadNoticeText } from './notices';

/**
 * After a reload of the window: what the reload closed (the backend closes
 * every session of the previous page and rolls back its open transactions).
 * Read once at mount; dismissible.
 */
const text = ref<string | null>(null);

onMounted(async () => {
  try {
    const report = await reloadReport();
    text.value = reloadNoticeText(report, (id) => useWorkspace().connection(id)?.name ?? 'a removed connection');
  } catch {
    // Nothing to tell when the report cannot be read.
  }
});
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
      aria-label="Dismiss"
      @click="text = null"
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
