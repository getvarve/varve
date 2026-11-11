<script setup lang="ts">
import { computed, onBeforeUnmount } from 'vue';
import type { PubSubTab } from '../../composables/useTabs';
import { useLastSession, useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import RedisPubSub from './RedisPubSub.vue';
import { usePubSub } from './usePubSub';

/**
 * The Redis Pub/Sub tab (`openPubSubTab`). Subscriptions belong to the tab and
 * keep running while it is hidden; they end when the tab closes.
 */
const props = defineProps<{ tab: PubSubTab }>();

const workspace = useWorkspace();
const tabs = useTabs();
// Kept while the session is lost, so the received messages stay until the tab connects again.
const session = useLastSession(() => props.tab);
const name = computed(() => workspace.connection(props.tab.connectionId)?.name ?? props.tab.title);

onBeforeUnmount(() => {
  // Unmounted because the tab closed (not just hidden): end its subscription.
  if (!tabs.tabs.some((t) => t.id === props.tab.id)) void usePubSub().close(props.tab.id);
});
</script>

<template>
  <RedisPubSub
    v-if="session"
    :tab-id="tab.id"
    :session-id="session.id"
    :connection-name="name"
    :read-only="session.readOnly"
  />
</template>
