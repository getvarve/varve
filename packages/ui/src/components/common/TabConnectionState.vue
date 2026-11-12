<script setup lang="ts">
import { computed } from 'vue';
import { presentError } from '../../api/errors';
import { useTabs } from '../../composables/useTabs';
import type { WorkspaceTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import InitCommandNotice from '../notices/InitCommandNotice.vue';
import Icon from './Icon.vue';

/**
 * What a tab without a session shows: "Connecting to …" while it connects,
 * and once a connect failed, was cancelled (a dismissed password prompt) or
 * its connection was closed, the error (or "Not connected.") with Connect
 * again. Every tab kind shows this same block; the workspace shows it in
 * place of the view, the query and key browser tabs inside their own layout.
 * Renders nothing while the tab is connected. `compact` is for a pane or a
 * column of a tab rather than the whole tab.
 */
const props = defineProps<{ tab: WorkspaceTab; compact?: boolean }>();

const tabs = useTabs();
const workspace = useWorkspace();

const profile = computed(() => workspace.connection(props.tab.connectionId));
const connectError = computed(() => (props.tab.connectError ? presentError(props.tab.connectError) : null));
</script>

<template>
  <div
    v-if="tab.status === 'connecting'"
    class="connection-state"
    :class="{ compact }"
    role="status"
  >
    Connecting to {{ profile?.name ?? 'the server' }}…
  </div>
  <div
    v-else-if="tab.status === 'error' || tab.status === 'cancelled'"
    class="connection-state"
    :class="{ compact }"
  >
    <div
      v-if="connectError"
      class="notice tone-danger"
      role="alert"
    >
      <Icon name="warning" />
      <span><strong>{{ connectError.title }}.</strong> {{ connectError.message }}<template v-if="connectError.code"> ({{ connectError.code }})</template></span>
    </div>
    <p
      v-else
      class="subtle"
    >
      Not connected.
    </p>
    <!-- A refused Redis init command explains itself and offers to edit the connection. -->
    <InitCommandNotice
      v-if="tab.connectError && profile?.engine === 'redis'"
      :error="tab.connectError"
      :connection-id="tab.connectionId"
      @edited="tabs.reconnect(tab.id)"
    />
    <button
      type="button"
      class="btn"
      @click="tabs.reconnect(tab.id)"
    >
      <Icon name="refresh" />Connect again
    </button>
  </div>
</template>

<style scoped>
.connection-state {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 12px;
  max-width: 560px;
  padding: 24px;
}

.connection-state.compact {
  padding: 12px;
}

.connection-state p {
  margin: 0;
}
</style>
