<script setup lang="ts">
import { computed, defineAsyncComponent, nextTick } from 'vue';
import type { Component } from 'vue';
import type { ObjectRef } from '../api/catalog';
import { presentError } from '../api/errors';
import ExtensionSlot from '../components/ExtensionSlot.vue';
import Icon from '../components/common/Icon.vue';
import type { IconName } from '../components/common/Icon.vue';
import InitCommandNotice from '../components/notices/InitCommandNotice.vue';
import NoticeHost from '../components/notices/NoticeHost.vue';
import { useObjectBrowser } from '../components/tree/objectBrowser';
import { useQuerySave } from '../composables/useQuerySave';
import { useTabs } from '../composables/useTabs';
import type { TabKind, WorkspaceTab } from '../composables/useTabs';
import { useWorkspace } from '../composables/useWorkspace';
import { ENVIRONMENT_INFO } from '../lib/display';
import HomeView from './HomeView.vue';

// The editor (CodeMirror) and the result grid (AG Grid) load on first use.
const QueryTabView = defineAsyncComponent(() => import('../components/query/QueryTabView.vue'));
const KeyBrowser = defineAsyncComponent(() => import('../components/redis/KeyBrowser.vue'));
const OverviewTabView = defineAsyncComponent(() => import('../components/overview/OverviewTabView.vue'));

type ViewKind = Exclude<TabKind, 'query' | 'keys' | 'overview'>;

/**
 * The view of every other tab kind: a component at a fixed path in the
 * directory of the worker that builds it, with one prop, `tab` (the tab of
 * that kind from `useTabs`). Each loads on first use. While a file does not
 * exist yet, the tab shows a placeholder that says what is missing.
 */
const TAB_VIEWS: Record<ViewKind, { path: string; what: string }> = {
  object: { path: '../components/objects/ObjectTabView.vue', what: 'object details and editors' },
  ddl: { path: '../components/objects/DdlTabView.vue', what: 'DDL view' },
  tableData: { path: '../components/table/TableDataView.vue', what: 'table data grid' },
  tableDesigner: { path: '../components/table/TableDesignerView.vue', what: 'table designer' },
  explain: { path: '../components/explain/ExplainTabView.vue', what: 'EXPLAIN plan view' },
  live: { path: '../components/live/LiveTabView.vue', what: 'live server panels' },
  redisKey: { path: '../components/redis/KeyEditorTabView.vue', what: 'Redis key editor' },
  notify: { path: '../components/notify/NotifyTabView.vue', what: 'notifications panel' },
  pubsub: { path: '../components/redis/PubSubTabView.vue', what: 'Pub/Sub panel' },
};

const viewModules = import.meta.glob<{ default: Component }>([
  '../components/objects/ObjectTabView.vue',
  '../components/objects/DdlTabView.vue',
  '../components/table/TableDataView.vue',
  '../components/table/TableDesignerView.vue',
  '../components/explain/ExplainTabView.vue',
  '../components/live/LiveTabView.vue',
  '../components/redis/KeyEditorTabView.vue',
  '../components/notify/NotifyTabView.vue',
  '../components/redis/PubSubTabView.vue',
]);

const views = new Map<ViewKind, Component>();
for (const [kind, spec] of Object.entries(TAB_VIEWS) as [ViewKind, { path: string }][]) {
  const load = viewModules[spec.path];
  if (load) views.set(kind, defineAsyncComponent(() => load().then((m) => m.default)));
}

const TAB_ICONS: Partial<Record<TabKind, IconName>> = {
  overview: 'database',
  keys: 'key',
  redisKey: 'key',
  object: 'database',
  ddl: 'file',
  tableData: 'database',
  tableDesigner: 'settings',
  explain: 'activity',
  live: 'activity',
  notify: 'info',
  pubsub: 'activity',
};

/** Editor and key browser tabs, one session each. Without tabs it shows the welcome screen. */
const tabs = useTabs();
const workspace = useWorkspace();
const querySave = useQuerySave();

const active = computed(() => tabs.active.value);

function envColor(tab: WorkspaceTab): string {
  const profile = workspace.connection(tab.connectionId);
  return profile ? ENVIRONMENT_INFO[profile.environment].color : 'var(--border)';
}

function dirty(tab: WorkspaceTab): boolean {
  return tab.kind === 'query' ? querySave.hasUnsavedEdits(tab) : tab.unsaved;
}

const activeView = computed<Component | null>(() => {
  const tab = active.value;
  if (!tab || tab.kind === 'query' || tab.kind === 'keys' || tab.kind === 'overview') return null;
  return views.get(tab.kind) ?? null;
});

const missingView = computed(() => {
  const tab = active.value;
  if (!tab || tab.kind === 'query' || tab.kind === 'keys' || tab.kind === 'overview' || views.has(tab.kind)) return null;
  return TAB_VIEWS[tab.kind].what;
});

const connectError = computed(() => (active.value?.connectError ? presentError(active.value.connectError) : null));
const reconnectMessage = computed(() => (active.value?.reconnectNeeded ? presentError(active.value.reconnectNeeded).message : null));
/**
 * Tab views report changes with three events (see the U1 contract): `changed`
 * (something was applied), `created` (an object tab or the table designer
 * created `object` and now shows it) and `dropped`. The object tree reloads
 * the container the tab works in.
 */
function refreshTreeFor(tab: WorkspaceTab): void {
  const browser = useObjectBrowser();
  if ('object' in tab && tab.object) void browser.refreshObject(tab.connectionId, tab.object);
  else if ('container' in tab) void browser.refreshContainer(tab.connectionId, tab.container);
  else void browser.refresh(tab.connectionId);
}

function onViewChanged(): void {
  if (active.value) refreshTreeFor(active.value);
}

function onViewCreated(object: ObjectRef): void {
  const tab = active.value;
  if (!tab) return;
  tabs.setTabObject(tab.id, object);
  void useObjectBrowser().refreshObject(tab.connectionId, object);
}

const isRedisTab = computed(() => (active.value ? workspace.connection(active.value.connectionId)?.engine === 'redis' : false));
const activeProfileName = computed(() => (active.value ? (workspace.connection(active.value.connectionId)?.name ?? 'the server') : ''));

async function select(id: string, focus = false): Promise<void> {
  tabs.activate(id);
  if (focus) {
    await nextTick();
    document.getElementById(`wtab-${id}`)?.focus();
  }
}

function onKeydown(event: KeyboardEvent): void {
  const list = tabs.tabs;
  const at = list.findIndex((t) => t.id === tabs.activeId.value);
  let next: number;
  if (event.key === 'ArrowRight') next = (at + 1) % list.length;
  else if (event.key === 'ArrowLeft') next = (at - 1 + list.length) % list.length;
  else if (event.key === 'Home') next = 0;
  else if (event.key === 'End') next = list.length - 1;
  else if (event.key === 'Delete' && list[at]) {
    event.preventDefault();
    void querySave.closeWithCheck(list[at].id);
    return;
  } else return;
  event.preventDefault();
  const tab = list[next];
  if (tab) void select(tab.id, true);
}

function onAuxClick(event: MouseEvent, id: string): void {
  if (event.button === 1) void querySave.closeWithCheck(id);
}
</script>

<template>
  <div class="workspace">
    <div class="notice-host">
      <NoticeHost />
    </div>
    <div
      v-if="tabs.tabs.length > 0"
      class="strip"
    >
      <div
        class="tablist"
        role="tablist"
        aria-label="Open tabs"
        @keydown="onKeydown"
      >
        <div
          v-for="tab in tabs.tabs"
          :key="tab.id"
          class="tab"
          :class="{ active: tab.id === tabs.activeId.value }"
          :style="{ boxShadow: `inset 0 2px 0 ${envColor(tab)}` }"
          @auxclick="onAuxClick($event, tab.id)"
        >
          <button
            :id="`wtab-${tab.id}`"
            type="button"
            role="tab"
            class="tab-label"
            :aria-selected="tab.id === tabs.activeId.value"
            :aria-controls="`wpanel-${tab.id}`"
            :tabindex="tab.id === tabs.activeId.value ? 0 : -1"
            @click="select(tab.id)"
          >
            <Icon
              v-if="TAB_ICONS[tab.kind]"
              :name="TAB_ICONS[tab.kind] ?? 'file'"
              :size="12"
            />
            <span class="tab-title">{{ tab.title }}</span>
            <span
              v-if="dirty(tab)"
              class="dirty"
              role="img"
              aria-label="Unsaved changes"
            />
            <span
              v-if="tab.status === 'connecting'"
              class="visually-hidden"
            >, connecting</span>
          </button>
          <button
            type="button"
            class="close"
            tabindex="-1"
            :aria-label="`Close ${tab.title}`"
            @click="querySave.closeWithCheck(tab.id)"
          >
            <Icon
              name="close"
              :size="12"
            />
          </button>
        </div>
      </div>
      <div class="strip-slot">
        <ExtensionSlot name="connection-tabs" />
      </div>
    </div>

    <div
      v-if="active"
      :id="`wpanel-${active.id}`"
      :key="active.id"
      class="panel"
      role="tabpanel"
      :aria-labelledby="`wtab-${active.id}`"
    >
      <div
        v-if="reconnectMessage"
        class="notice tone-warning reconnect"
        role="alert"
      >
        <Icon name="warning" />
        <span class="reconnect-text"><strong>Reconnect needed.</strong> {{ reconnectMessage }} Reconnecting opens a new session; an open
          transaction in this tab is rolled back.</span>
        <button
          type="button"
          class="btn compact"
          @click="tabs.reconnect(active.id)"
        >
          <Icon name="refresh" />Reconnect
        </button>
      </div>
      <div class="view">
        <QueryTabView
          v-if="active.kind === 'query'"
          :tab="active"
        />
        <KeyBrowser
          v-else-if="active.kind === 'keys'"
          :tab="active"
        />
        <div
          v-else-if="active.status === 'connecting'"
          class="state"
          role="status"
        >
          Connecting to {{ activeProfileName }}…
        </div>
        <div
          v-else-if="active.status === 'error' || active.status === 'cancelled'"
          class="state"
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
            v-if="active.connectError && isRedisTab"
            :error="active.connectError"
            :connection-id="active.connectionId"
            @edited="tabs.reconnect(active.id)"
          />
          <button
            type="button"
            class="btn"
            @click="tabs.reconnect(active.id)"
          >
            <Icon name="refresh" />Connect again
          </button>
        </div>
        <OverviewTabView
          v-else-if="active.kind === 'overview'"
          :key="active.id"
          :tab="active"
        />
        <component
          :is="activeView"
          v-else-if="activeView"
          :tab="active"
          @changed="onViewChanged"
          @dropped="onViewChanged"
          @created="onViewCreated"
        />
        <div
          v-else
          class="state"
          role="status"
        >
          <p>
            <strong>{{ active.title }}</strong>
          </p>
          <p class="subtle">
            The {{ missingView }} is not part of this build yet. The tab is connected; close it or open another one.
          </p>
        </div>
      </div>
    </div>
    <HomeView v-else />
  </div>
</template>

<style scoped>
.workspace {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}

.strip {
  display: flex;
  flex-shrink: 0;
  height: 32px;
  border-bottom: 1px solid var(--border);
  background: var(--ground);
}

.tablist {
  display: flex;
  min-width: 0;
  overflow-x: auto;
}

.tab {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  max-width: 240px;
  border-right: 1px solid var(--border);
  color: var(--text-muted);
}

.tab.active {
  background: var(--surface);
  color: var(--text);
}

.tab-label {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
  height: 100%;
  padding: 0 4px 0 14px;
  border: 0;
  background: transparent;
  color: inherit;
  font-size: 13px;
  cursor: pointer;
}

.tab-title {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.dirty {
  flex-shrink: 0;
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--text);
}

.close {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  margin-right: 6px;
  padding: 0;
  border: 0;
  border-radius: 3px;
  background: transparent;
  color: var(--text-subtle);
  cursor: pointer;
}

.close:hover {
  background: var(--surface-raised);
  color: var(--text);
}

.strip-slot {
  display: flex;
  align-items: center;
  gap: 4px;
  margin-left: auto;
  padding: 0 8px;
}

.panel {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
}

.notice-host {
  flex-shrink: 0;
}

.view {
  flex: 1;
  min-height: 0;
}

.reconnect {
  flex-shrink: 0;
  align-items: center;
  margin: 8px 12px 0;
}

.reconnect-text {
  flex: 1;
}

.state {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 12px;
  max-width: 560px;
  padding: 24px;
}

.state p {
  margin: 0;
}
</style>
