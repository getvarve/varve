<script setup lang="ts">
import { computed, ref } from 'vue';
import type { RedisKeyTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { entryForKey } from '../../lib/redisKeys';
import ValueViewer from './ValueViewer.vue';

/**
 * One Redis key in its own tab (`openRedisKeyTab`): the key viewer and its
 * editors on the tab's session, in database `tab.db`. A rename follows the key;
 * a delete leaves a note.
 */
const props = defineProps<{ tab: RedisKeyTab }>();

const workspace = useWorkspace();
const profile = computed(() => workspace.connection(props.tab.connectionId));
const deleted = ref(false);

/** The key as a browser entry; its type is read when the viewer opens it. */
const entry = computed(() => entryForKey(props.tab.key, 'unknown', null));

function onRenamed(newKey: string): void {
  // The tab object is shared state: the title and key follow the rename.
  const tab = props.tab;
  tab.key = newKey;
  tab.title = newKey === '' ? '(empty key name)' : newKey;
}
</script>

<template>
  <div class="key-tab">
    <p
      v-if="deleted"
      class="empty subtle"
    >
      The key {{ tab.key }} was deleted. Close this tab, or create the key again from the key browser.
    </p>
    <p
      v-else-if="!entry"
      class="notice tone-danger state"
      role="alert"
    >
      The key name {{ tab.key }} has a malformed escape (a backslash must start \\ or \xHH), so it cannot be opened.
    </p>
    <ValueViewer
      v-else-if="tab.session"
      :tab-id="tab.id"
      :session-id="tab.session.id"
      :entry="entry"
      :read-only="tab.session.readOnly"
      :connection-name="profile?.name ?? tab.title"
      @renamed="onRenamed"
      @deleted="deleted = true"
    />
  </div>
</template>

<style scoped>
.key-tab {
  height: 100%;
  min-height: 0;
}

.empty,
.state {
  margin: 24px;
}
</style>
