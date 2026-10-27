<script setup lang="ts">
import { computed } from 'vue';
import type { ExplainTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import ExplainView from './ExplainView.vue';

/**
 * The EXPLAIN view of a workspace tab (`kind: 'explain'`): explains
 * `tab.statement` on the tab's own session. WorkspaceView renders it once
 * the tab is connected. It shows the plain EXPLAIN right away (the
 * statement does not run); EXPLAIN ANALYZE runs only when the user asks.
 */
const props = defineProps<{ tab: ExplainTab }>();

const workspace = useWorkspace();
const connectionName = computed(() => workspace.connection(props.tab.connectionId)?.name ?? props.tab.title);
</script>

<template>
  <ExplainView
    v-if="tab.session"
    :session="tab.session"
    :connection-name="connectionName"
    :statement="tab.statement"
    :tab-id="tab.id"
    auto-run="explain"
  />
</template>
