<script setup lang="ts">
import { computed, shallowRef, watch } from 'vue';
import type { DdlTab } from '../../composables/useTabs';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { useObjectBrowser } from '../tree/objectBrowser';
import DdlView from './DdlView.vue';

/**
 * The `ddl` tab (U1 contract in M2-UI-PLAN.md, "Show DDL"): the object's
 * DDL, read only with copy, save and "Open in editor", or edited and run
 * with `ddl_apply`.
 */
const props = defineProps<{ tab: DdlTab }>();

const tabs = useTabs();
// Kept while a reconnect clears the tab's session, so edited DDL is not unmounted (the workspace hides the tab meanwhile).
const session = shallowRef(props.tab.session);
watch(
  () => props.tab.session,
  (next) => {
    if (next) session.value = next;
  },
);
const profile = computed(() => useWorkspace().connection(props.tab.connectionId));

function openSql(sql: string): void {
  if (profile.value) tabs.openQueryTab(profile.value, null, sql, props.tab.database);
}
</script>

<template>
  <DdlView
    v-if="session"
    :session="session"
    :connection-name="profile?.name ?? tab.title"
    :object="tab.object"
    :report-error="(error: unknown, sessionId: string) => tabs.reportTabError(tab.id, error, sessionId)"
    :report-dirty="(dirty: boolean) => tabs.setUnsaved(tab.id, dirty)"
    @open-sql="openSql"
    @changed="useObjectBrowser().refresh(tab.connectionId)"
    @reconnect="tabs.reconnect(tab.id)"
  />
</template>
