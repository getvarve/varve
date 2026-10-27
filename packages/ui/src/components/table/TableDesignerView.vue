<script setup lang="ts">
import { computed, onMounted } from 'vue';
import type { ObjectKind, ObjectRef } from '../../api/catalog';
import type { TableTarget } from '../../api/grid';
import type { TableDesignerTab } from '../../composables/useTabs';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { designerIntent, focusShown, pendingFocus, pruneIntents } from './designerIntent';
import type { DesignerFocus } from './designerIntent';
import TableDesigner from './TableDesigner.vue';

/**
 * Tab view of kind `tableDesigner` (U1 contract): designs `tab.object`, or
 * a new table in `tab.container` when `object` is `null`. Rendered by
 * WorkspaceView only while `tab.status === 'ready'`.
 */
const props = defineProps<{ tab: TableDesignerTab }>();
const emit = defineEmits<{
  /** Something was applied: WorkspaceView reloads the table's schema or database in the tree. */
  changed: [];
  /** A new table was created: WorkspaceView points the tab at it and reloads its container. */
  created: [object: ObjectRef];
}>();
const tabs = useTabs();
const workspace = useWorkspace();

const connectionName = computed(() => workspace.connection(props.tab.connectionId)?.name ?? 'this connection');

const target = computed<TableTarget | null>(() => {
  const o = props.tab.object;
  if (!o) return null;
  const pg = props.tab.session?.engine === 'postgres';
  return { database: o.database ?? props.tab.session?.database ?? '', schema: pg ? o.schema : null, table: o.name };
});

const newIn = computed(() => {
  const c = props.tab.container;
  const pg = props.tab.session?.engine === 'postgres';
  return { database: c.database ?? props.tab.session?.database ?? '', schema: pg ? (c.schema ?? 'public') : null };
});

/** What the tab was opened for: a new partitioned table, or an item to show (designerIntent.ts). */
const intent = computed(() => designerIntent(props.tab.id));
/** A focus the designer has not shown yet (the view is mounted anew each time the tab is shown). */
const focus = computed<DesignerFocus | null>(() => (intent.value.focus ? pendingFocus(props.tab.id) : null));

onMounted(() => pruneIntents(tabs.tabs.map((t) => t.id)));

function onFocused(shown: DesignerFocus): void {
  focusShown(props.tab.id, shown);
}

function onError(error: unknown): void {
  tabs.reportTabError(props.tab.id, error);
}

function onCreated(created: TableTarget, kind: ObjectKind): void {
  emit('created', { kind, database: created.database, schema: created.schema, parent: null, name: created.table, signature: null });
}

</script>

<template>
  <TableDesigner
    v-if="tab.session"
    :key="tab.id"
    :session="tab.session"
    :connection-name="connectionName"
    :target="target"
    :object-kind="tab.object?.kind ?? 'table'"
    :new-in="newIn"
    :start-partitioned="intent.partitioned"
    :focus="focus"
    @dirty="tabs.setUnsaved(tab.id, $event)"
    @error="onError"
    @changed="emit('changed')"
    @created="onCreated"
    @focused="onFocused"
  />
</template>
