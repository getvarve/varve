<script setup lang="ts">
import { computed, useId } from 'vue';
import { useWorkspace } from '../../composables/useWorkspace';
import Icon from '../common/Icon.vue';
import { keychainLines } from './notices';
import { useNotices } from './useNotices';

/**
 * A quiet notice while keychain entries of edited or deleted connections are
 * still to be removed. Varve retries on its own; Retry tries now and shows
 * what still fails.
 */
const notices = useNotices();
const workspace = useWorkspace();
const uid = useId();

const lines = computed(() => keychainLines(notices.state.keychain, (id) => workspace.connection(id)));
</script>

<template>
  <div
    v-if="lines.length > 0 || notices.state.keychainError"
    class="banner tone-neutral"
    role="region"
    :aria-labelledby="`${uid}-head`"
  >
    <Icon
      name="key"
      class="icon"
    />
    <div class="body">
      <p
        :id="`${uid}-head`"
        class="head"
      >
        <template v-if="lines.length > 0">
          {{ lines.length === 1 ? 'One keychain entry' : `${lines.length} keychain entries` }} of an edited or deleted connection could not be removed yet.
          Varve no longer uses {{ lines.length === 1 ? 'it' : 'them' }} and retries in the background.
        </template>
        <template v-else>
          Keychain cleanup
        </template>
      </p>
      <ul
        v-if="lines.length > 0"
        class="list"
      >
        <li
          v-for="(line, i) in lines"
          :key="i"
        >
          {{ line }}
        </li>
      </ul>
      <div
        v-if="notices.state.keychainMessages.length > 0"
        class="problem"
        role="alert"
      >
        <p>Still failing after the retry:</p>
        <ul class="list">
          <li
            v-for="(m, i) in notices.state.keychainMessages"
            :key="i"
          >
            {{ m }}
          </li>
        </ul>
      </div>
      <p
        v-if="notices.state.keychainError"
        class="problem"
        role="alert"
      >
        {{ notices.state.keychainError }}
      </p>
    </div>
    <button
      type="button"
      class="btn compact"
      :disabled="notices.state.retrying"
      @click="notices.retryKeychain()"
    >
      {{ notices.state.retrying ? 'Retrying…' : 'Retry' }}
    </button>
  </div>
</template>

<style scoped>
.banner {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 8px 12px;
  border-bottom: 1px solid var(--border-inner);
  font-size: 12px;
}

.icon {
  flex-shrink: 0;
  margin-top: 2px;
}

.body {
  flex: 1;
  min-width: 0;
}

.body p {
  margin: 0;
}

.list {
  margin: 4px 0 0;
  padding-left: 18px;
}

.problem {
  margin-top: 4px;
  color: var(--danger-text);
}

.compact {
  height: 28px;
  font-size: 12px;
}
</style>
