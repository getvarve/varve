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
 * lists and keeps them current.
 */
const notices = useNotices();
onMounted(() => notices.start());
</script>

<template>
  <ReloadNotice />
  <div
    v-if="notices.state.restores.length > 0 || notices.state.keychain.length > 0 || notices.state.keychainError || notices.state.restoreError"
    class="notices"
  >
    <p
      v-if="notices.state.restoreError"
      class="notice tone-danger error"
      role="alert"
    >
      {{ notices.state.restoreError }}
    </p>
    <PendingRestoreBanner
      v-for="entry in notices.state.restores"
      :key="restoreKey(entry)"
      :entry="entry"
    />
    <KeychainNotice />
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
</style>
