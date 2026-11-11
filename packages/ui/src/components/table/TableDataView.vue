<script setup lang="ts">
import { computed } from 'vue';
import { displayName } from '../../api/catalog';
import type { TableDataTab } from '../../composables/useTabs';
import { useLastSession, useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import EnvBadge from '../common/EnvBadge.vue';
import TableData from './TableData.vue';

/**
 * Tab view of kind `tableData` (U1 contract): the rows of `tab.object`
 * with grid editing for tables that have a usable key. WorkspaceView shows
 * it only while `tab.status === 'ready'`; it stays mounted on the last
 * session while the session is lost, so staged edits survive a reconnect.
 */
const props = defineProps<{ tab: TableDataTab }>();
const emit = defineEmits<{
  /** Rows were written: WorkspaceView reloads the table's container in the tree (row estimates). */
  changed: [];
}>();
const tabs = useTabs();
const workspace = useWorkspace();
const session = useLastSession(() => props.tab);

const profile = computed(() => workspace.connection(props.tab.connectionId));
const connectionName = computed(() => profile.value?.name ?? 'this connection');
const KIND_LABELS: Record<string, string> = {
  table: 'Table',
  partitionedTable: 'Partitioned table',
  partition: 'Partition',
  view: 'View',
  materializedView: 'Materialized view',
  foreignTable: 'Foreign table',
};
const designable = computed(() => ['table', 'partitionedTable', 'partition'].includes(props.tab.object.kind));

function design(): void {
  const p = profile.value;
  if (!p) return;
  const o = props.tab.object;
  tabs.openTableDesigner(p, o, { database: o.database, schema: o.schema, parent: null });
}

function onError(error: unknown, sessionId: string): void {
  tabs.reportTabError(props.tab.id, error, sessionId);
}
</script>

<template>
  <div
    v-if="session"
    class="view"
  >
    <header class="head">
      <span class="name mono">{{ displayName(tab.object) }}</span>
      <span class="muted small">{{ tab.object.database ?? '' }} · {{ KIND_LABELS[tab.object.kind] ?? tab.object.kind }}</span>
      <span class="spacer" />
      <button
        v-if="designable"
        type="button"
        class="btn compact"
        @click="design"
      >
        Design
      </button>
      <EnvBadge :environment="session.environment" />
    </header>
    <TableData
      class="body"
      :session="session"
      :connection-name="connectionName"
      :object="tab.object"
      @dirty="tabs.setUnsaved(tab.id, $event)"
      @error="onError"
      @changed="emit('changed')"
    />
  </div>
</template>

<style scoped>
.view {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}

.head {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 12px;
  height: 48px;
  padding: 0 16px;
  border-bottom: 1px solid var(--border-inner);
  background: var(--surface);
}

.name {
  font-size: 15px;
  font-weight: 500;
}

.small {
  font-size: 12px;
}

.spacer {
  flex: 1;
}

.body {
  flex: 1;
  min-height: 0;
}
</style>
