<script setup lang="ts">
import { onMounted } from 'vue';
import KeychainNotice from './KeychainNotice.vue';
import PendingRestoreBanner from './PendingRestoreBanner.vue';
import ReloadNotice from './ReloadNotice.vue';
import { restoreKey, useNotices } from './useNotices';

/**
 * App-wide notices above the workspace: what a reload of the window closed,
 * pending collation restores (one persistent banner each) and the keychain
 * cleanup. Mount once; it loads both
 * lists and keeps them current. A banner settles in from just above as it
 * appears (opacity and transform only, the space it takes opens at once).
 */
const notices = useNotices();
onMounted(() => notices.start());
</script>

<template>
  <ReloadNotice class="notice-in" />
  <div
    v-if="notices.state.restores.length > 0 || notices.state.keychain.length > 0 || notices.state.keychainError || notices.state.restoreError"
    class="notices"
  >
    <p
      v-if="notices.state.restoreError"
      class="notice tone-danger error notice-in"
      role="alert"
    >
      {{ notices.state.restoreError }}
    </p>
    <PendingRestoreBanner
      v-for="entry in notices.state.restores"
      :key="restoreKey(entry)"
      class="notice-in"
      :entry="entry"
    />
    <KeychainNotice class="notice-in" />
  </div>
</template>

<style scoped>
.notices {
  display: flex;
  flex-shrink: 0;
  flex-direction: column;
  max-height: 40vh;
  overflow: auto;
  border-bottom: 1px solid var(--border);
}

.error {
  margin: 8px 12px;
}

.notice-in {
  animation: notice-in 180ms var(--ease-out);
}

@keyframes notice-in {
  from {
    opacity: 0;
    transform: translateY(-4px);
  }
}

@keyframes notice-fade-in {
  from {
    opacity: 0;
  }
}

/* Reduced motion: a short fade only, no movement. */
@media (prefers-reduced-motion: reduce) {
  .notice-in {
    animation-name: notice-fade-in;
  }
}
</style>
