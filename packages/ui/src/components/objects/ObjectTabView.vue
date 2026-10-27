<script setup lang="ts">
import { computed, ref } from 'vue';
import type { Component } from 'vue';
import { displayName, kindLabel } from '../../api/catalog';
import type { ObjectKind, ObjectRef } from '../../api/catalog';
import type { ObjectTab } from '../../composables/useTabs';
import { buildDrop } from '../../lib/ddlPlan';
import { commentSupported } from '../../lib/objectComment';
import { hasObjectEditor } from '../../lib/editorKinds';
import type { EditorKind } from '../../lib/editorKinds';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { useObjectBrowser } from '../tree/objectBrowser';
import CommentEditor from './CommentEditor.vue';
import DatabaseEditor from './DatabaseEditor.vue';
import DdlView from './DdlView.vue';
import EventEditor from './EventEditor.vue';
import EventTriggerEditor from './EventTriggerEditor.vue';
import ExtensionsManager from './ExtensionsManager.vue';
import MatViewEditor from './MatViewEditor.vue';
import PartitionsOverview from './PartitionsOverview.vue';
import PolicyEditor from './PolicyEditor.vue';
import RoutineEditor from './RoutineEditor.vue';
import RuleEditor from './RuleEditor.vue';
import SchemaEditor from './SchemaEditor.vue';
import SequenceEditor from './SequenceEditor.vue';
import SystemVersionedTable from './SystemVersionedTable.vue';
import TableDropView from './TableDropView.vue';
import TriggerEditor from './TriggerEditor.vue';
import TypeEditor from './TypeEditor.vue';
import ViewEditor from './ViewEditor.vue';
import type { NewObjectContainer } from './types';

/**
 * The `object` tab (U1 contract in M2-UI-PLAN.md): picks the editor for the
 * tab's object kind and wires its events to the tabs, the object tree and
 * the query tabs. Table kinds show the partitions overview and, on
 * MariaDB, the system-versioning view; the table designer and data grid
 * are other tabs.
 */
const props = defineProps<{ tab: ObjectTab }>();

const tabs = useTabs();
const kind = computed<ObjectKind | null>(() => props.tab.object?.kind ?? props.tab.createKind);
const session = computed(() => props.tab.session);
const profile = computed(() => useWorkspace().connection(props.tab.connectionId));
const connectionName = computed(() => profile.value?.name ?? props.tab.title);
const tableView = ref<'partitions' | 'versioning'>(props.tab.object?.kind === 'partitionedTable' ? 'partitions' : 'versioning');

// Typed by OBJECT_EDITOR_KINDS, so the tree's New / Edit / Drop entries (lib/editorKinds.ts) match this map.
const EDITORS: Record<EditorKind, Component> = {
  view: ViewEditor,
  materializedView: MatViewEditor,
  function: RoutineEditor,
  procedure: RoutineEditor,
  trigger: TriggerEditor,
  eventTrigger: EventTriggerEditor,
  sequence: SequenceEditor,
  event: EventEditor,
  enumType: TypeEditor,
  compositeType: TypeEditor,
  rangeType: TypeEditor,
  domain: TypeEditor,
  extension: ExtensionsManager,
  database: DatabaseEditor,
  schema: SchemaEditor,
  rule: RuleEditor,
  policy: PolicyEditor,
};

const isTable = computed(() => kind.value === 'table' || kind.value === 'partitionedTable' || kind.value === 'partition');
const mariadb = computed(() => session.value?.engine === 'mariadb');
/** Tables are designed in their own tab; "Drop table" opens their drop view here. */
const tableDrop = computed(() => {
  const object = props.tab.object;
  if (!isTable.value || !object || props.tab.intent !== 'drop' || !session.value) return null;
  return buildDrop(session.value.engine, object, false) !== null ? object : null;
});
const editor = computed<Component | null>(() => {
  if (!kind.value) return null;
  if (isTable.value && props.tab.object) {
    if (session.value?.engine === 'postgres') return kind.value === 'partition' ? null : PartitionsOverview;
    return tableView.value === 'versioning' && mariadb.value ? SystemVersionedTable : PartitionsOverview;
  }
  return hasObjectEditor(kind.value) ? EDITORS[kind.value] : null;
});

/** Objects without an editor: their comment can still be changed (design doc 4.2, "Comments E on every object"). */
const commentable = computed(() => {
  const object = props.tab.object;
  return object !== null && !!session.value && commentSupported(session.value.engine, object.kind) ? object : null;
});
/** Bumped after a comment change so the DDL below is read again (unless the user is editing it). */
const ddlVersion = ref(0);
const commentDirty = ref(false);
const ddlDirty = ref(false);

function fallbackDirty(part: 'comment' | 'ddl', value: boolean): void {
  if (part === 'comment') commentDirty.value = value;
  else ddlDirty.value = value;
  tabs.setUnsaved(props.tab.id, commentDirty.value || ddlDirty.value);
}

/** Bumped after the DDL ran, so the comment is read again (unless the user is editing it). */
const commentVersion = ref(0);

function commentChanged(): void {
  if (!ddlDirty.value) ddlVersion.value += 1;
  refreshTree();
}

function ddlRan(): void {
  if (!commentDirty.value) commentVersion.value += 1;
  refreshTree();
}

const container = computed<NewObjectContainer>(() => ({
  database: props.tab.container.database,
  schema: props.tab.container.schema,
  parent: props.tab.container.parent?.name ?? props.tab.object?.parent ?? null,
}));

function report(error: unknown): void {
  tabs.reportTabError(props.tab.id, error);
}

function dirty(value: boolean): void {
  tabs.setUnsaved(props.tab.id, value);
}

function refreshTree(): void {
  void useObjectBrowser().refresh(props.tab.connectionId);
}

function opened(object: ObjectRef): void {
  // The tab now shows this object (after create or rename).
  const tab = tabs.tabs.find((t) => t.id === props.tab.id);
  if (!tab || tab.kind !== 'object') return;
  tab.object = object;
  tab.intent = 'edit';
  tab.title = displayName(object);
}

function dropped(): void {
  tabs.setUnsaved(props.tab.id, false);
  void tabs.closeTab(props.tab.id);
}

function openSql(sql: string): void {
  if (profile.value) tabs.openQueryTab(profile.value, null, sql, props.tab.database);
}

function openObject(object: ObjectRef): void {
  if (profile.value) tabs.openObjectTab(profile.value, { object });
}

function createObject(createKind: ObjectKind, where: NewObjectContainer): void {
  if (!profile.value) return;
  const parent: ObjectRef | null = where.parent
    ? { kind: 'table', database: where.database, schema: where.schema, parent: null, name: where.parent, signature: null }
    : null;
  tabs.openObjectTab(profile.value, { createKind, container: { database: where.database, schema: where.schema, parent } });
}

function reconnect(): void {
  void tabs.reconnect(props.tab.id);
}

function switchDatabase(database: string): void {
  void tabs.switchTabDatabase(props.tab.id, database);
}

function cancelTableDrop(): void {
  const tab = tabs.tabs.find((t) => t.id === props.tab.id);
  if (tab && tab.kind === 'object') tab.intent = 'view';
}
</script>

<template>
  <div
    v-if="session"
    class="object-tab"
  >
    <div
      v-if="isTable && tab.object && mariadb && !tableDrop"
      class="switch-bar"
    >
      <div
        role="radiogroup"
        aria-label="Table view"
        class="segmented"
      >
        <button
          type="button"
          role="radio"
          :aria-checked="tableView === 'versioning'"
          @click="tableView = 'versioning'"
        >
          System versioning
        </button>
        <button
          type="button"
          role="radio"
          :aria-checked="tableView === 'partitions'"
          @click="tableView = 'partitions'"
        >
          Partitions
        </button>
      </div>
    </div>
    <TableDropView
      v-if="tableDrop"
      :session="session"
      :connection-name="connectionName"
      :object="tableDrop"
      :report-error="report"
      @dropped="dropped"
      @changed="refreshTree"
      @cancel="cancelTableDrop"
      @reconnect="reconnect"
    />
    <component
      :is="editor"
      v-else-if="editor"
      :key="`${tab.object?.kind ?? tab.createKind}:${tableView}`"
      :session="session"
      :connection-name="connectionName"
      :object="tab.object"
      :container="container"
      :create-kind="tab.createKind"
      :intent="tab.intent"
      :report-error="report"
      :report-dirty="dirty"
      @changed="refreshTree"
      @opened="opened"
      @dropped="dropped"
      @open-sql="openSql"
      @open-object="openObject"
      @create-object="createObject"
      @reconnect="reconnect"
      @switch-database="switchDatabase"
    />
    <div
      v-else
      class="none"
    >
      <p class="notice tone-neutral">
        There is no editor for {{ kind ? kindLabel(kind).toLocaleLowerCase() : 'this object' }} objects here.
        <template v-if="commentable">
          Change its comment below; for anything else edit and run its DDL, or use the console.
        </template>
        <template v-else-if="tab.object">
          Its DDL is below; edit and run it there, or use the console.
        </template>
      </p>
      <CommentEditor
        v-if="commentable"
        :key="commentVersion"
        class="comment"
        :session="session"
        :connection-name="connectionName"
        :object="commentable"
        :report-error="report"
        :report-dirty="(value: boolean) => fallbackDirty('comment', value)"
        @changed="commentChanged"
        @reconnect="reconnect"
      />
      <DdlView
        v-if="tab.object"
        :key="ddlVersion"
        :session="session"
        :connection-name="connectionName"
        :object="tab.object"
        :report-error="report"
        :report-dirty="(value: boolean) => fallbackDirty('ddl', value)"
        @open-sql="openSql"
        @changed="ddlRan"
        @reconnect="reconnect"
      />
    </div>
  </div>
</template>

<style scoped>
.object-tab {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}

.object-tab > :last-child {
  flex-grow: 1;
  min-height: 0;
}

.switch-bar {
  display: flex;
  padding: 8px 16px;
  border-bottom: 1px solid var(--border-inner);
  background: var(--surface);
}

.none {
  display: flex;
  flex-direction: column;
  gap: 8px;
  height: 100%;
}

.none > .notice {
  margin: 16px 16px 0;
}

.none > .comment {
  flex-shrink: 0;
  margin: 0 16px;
}
</style>
