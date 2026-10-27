<script setup lang="ts">
import { computed, onBeforeUnmount } from 'vue';
import type { NotifyTab } from '../../composables/useTabs';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import NotifyPanel from './NotifyPanel.vue';
import { useNotify } from './useNotify';

/**
 * The PostgreSQL notifications tab (`openNotifyTab`, `kind: 'notify'`). Its
 * listener belongs to the tab and keeps running while the tab is hidden; it
 * ends when the tab closes (or its session does).
 */
const props = defineProps<{ tab: NotifyTab }>();

const workspace = useWorkspace();
const tabs = useTabs();
const name = computed(() => workspace.connection(props.tab.connectionId)?.name ?? props.tab.title);

onBeforeUnmount(() => {
  // Unmounted because the tab closed (not just hidden): end its listener.
  if (!tabs.tabs.some((t) => t.id === props.tab.id)) void useNotify().close(props.tab.id);
});
</script>

<template>
  <NotifyPanel
    v-if="tab.session"
    :tab-id="tab.id"
    :session-id="tab.session.id"
    :connection-name="name"
    :read-only="tab.session.readOnly"
  />
</template>
