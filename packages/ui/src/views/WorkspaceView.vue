<script setup lang="ts">
import { computed, defineAsyncComponent, defineComponent, nextTick, onActivated, onDeactivated, onMounted, onUnmounted, provide, reactive, ref, watch } from 'vue';
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
import { showsView, TAB_VISIBLE_KEY, useTabs } from '../composables/useTabs';
import type { TabKind, WorkspaceTab } from '../composables/useTabs';
import { useWorkspace } from '../composables/useWorkspace';
import { ENVIRONMENT_INFO } from '../lib/display';
import { isMacPlatform, tabTip, withKeys } from '../lib/tips';
import HomeView from './HomeView.vue';

// AppShell keeps this view alive (`<KeepAlive include="WorkspaceView">`) while Activity or Settings is shown.
defineOptions({ name: 'WorkspaceView' });

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

/**
 * Editor and key browser tabs, one session each. Without tabs it shows the
 * welcome screen. Every open tab has its own panel: a tab's view is mounted
 * the first time the tab is shown and stays mounted (hidden with `v-show`)
 * until the tab closes, so typed text, staged edits, scroll positions and
 * running work survive switching tabs and pages.
 */
const tabs = useTabs();
const workspace = useWorkspace();
const querySave = useQuerySave();

const active = computed(() => tabs.active.value);

/** True while this page is on screen: mounted and not cached away by AppShell's KeepAlive. */
const shown = ref(false);
function setShown(value: boolean): void {
  shown.value = value;
  tabs.setWorkspaceShown(value);
}
onMounted(() => setShown(true));
onActivated(() => setShown(true));
onDeactivated(() => setShown(false));
onUnmounted(() => setShown(false));

/**
 * Tabs that were shown at least once: their panel content is mounted. A tab
 * opened while Activity or Settings is shown mounts once the workspace is
 * back, so its view never mounts detached from the document.
 */
const opened = reactive(new Set<string>());
/** Tabs whose own view was mounted: it stays mounted while a reconnect hides it. */
const viewMounted = reactive(new Set<string>());

watch(
  [() => tabs.activeId.value, shown],
  ([id, isShown]) => {
    if (id !== null && isShown) opened.add(id);
  },
  { immediate: true },
);
watch(
  () => tabs.tabs.map((t) => t.id),
  (ids) => {
    const open = new Set(ids);
    for (const id of [...opened]) if (!open.has(id)) opened.delete(id);
    for (const id of [...viewMounted]) if (!open.has(id)) viewMounted.delete(id);
  },
);

watch(
  () => tabs.tabs.filter((t) => opened.has(t.id) && showsView(t)).map((t) => t.id),
  (ids) => {
    for (const id of ids) viewMounted.add(id);
  },
  { immediate: true },
);

/**
 * The keyboard focus must not stay on something that is hidden: in the panel
 * of a tab that is not the active one any more, or in a view that is hidden
 * behind the connection state while its tab reconnects. Not every webview
 * drops the focus of a `display: none` element by itself, and typing would
 * then go on in the hidden editor. The element is blurred; when nothing in
 * the tab now shown takes the focus (a view may focus itself when shown), it
 * goes to the label of the active tab, from where the arrow keys switch tabs
 * and Tab enters the panel.
 */
watch(
  [() => tabs.activeId.value, () => (tabs.activeId.value !== null ? tabs.isTabVisible(tabs.activeId.value) : false), shown],
  async ([id, viewShown]) => {
    const focused = document.activeElement;
    if (!(focused instanceof HTMLElement)) return;
    const panel = focused.closest('[id^="wpanel-"]');
    if (!panel) return;
    const inActivePanel = id !== null && panel.id === `wpanel-${id}`;
    const inHiddenView = inActivePanel && !viewShown && (document.getElementById(`wview-${id}`)?.contains(focused) ?? false);
    if (inActivePanel && !inHiddenView) return;
    focused.blur();
    await nextTick();
    const nowhere = document.activeElement === null || document.activeElement === document.body;
    if (nowhere && shown.value && tabs.activeId.value !== null) document.getElementById(`wtab-${tabs.activeId.value}`)?.focus();
  },
  { flush: 'post' },
);

/**
 * Provides `useTabVisible()` to the views of one tab: whether that tab is on
 * screen (`useTabs().isTabVisible`).
 */
const TabScope = defineComponent({
  name: 'TabScope',
  props: { tabId: { type: String, required: true } },
  setup(props, { slots }) {
    provide(
      TAB_VISIBLE_KEY,
      computed(() => tabs.isTabVisible(props.tabId)),
    );
    return () => slots.default?.();
  },
});

function envColor(tab: WorkspaceTab): string {
  const profile = workspace.connection(tab.connectionId);
  return profile ? ENVIRONMENT_INFO[profile.environment].color : 'var(--border)';
}

/** The tab's full title and its connection's environment (the colour of its top edge). */
function tipOf(tab: WorkspaceTab): string {
  const profile = workspace.connection(tab.connectionId);
  return tabTip(tab.title, profile ? { name: profile.name, environment: ENVIRONMENT_INFO[profile.environment].label } : null);
}

/** Close Tab has its shortcut in the macOS menu bar; the other systems have no menu bar. */
const closeTip = withKeys('Close tab', isMacPlatform() ? 'CmdOrCtrl+W' : null);

function dirty(tab: WorkspaceTab): boolean {
  return tab.kind === 'query' ? querySave.hasUnsavedEdits(tab) : tab.unsaved;
}

function viewOf(tab: WorkspaceTab): Component | null {
  if (tab.kind === 'query' || tab.kind === 'keys' || tab.kind === 'overview') return null;
  return views.get(tab.kind) ?? null;
}

function missingView(tab: WorkspaceTab): string | null {
  if (tab.kind === 'query' || tab.kind === 'keys' || tab.kind === 'overview' || views.has(tab.kind)) return null;
  return TAB_VIEWS[tab.kind].what;
}

function connectError(tab: WorkspaceTab): ReturnType<typeof presentError> | null {
  return tab.connectError ? presentError(tab.connectError) : null;
}

function reconnectMessage(tab: WorkspaceTab): string | null {
  return tab.reconnectNeeded ? presentError(tab.reconnectNeeded).message : null;
}

/**
 * Tab views report changes with three events (see the U1 contract): `changed`
 * (something was applied), `created` (an object tab or the table designer
 * created `object` and now shows it) and `dropped`. The object tree reloads
 * the container the tab works in. A view emits for its own tab, which need
 * not be the active one (an apply that finishes after a tab switch).
 */
function refreshTreeFor(tab: WorkspaceTab): void {
  const browser = useObjectBrowser();
  if ('object' in tab && tab.object) void browser.refreshObject(tab.connectionId, tab.object);
  else if ('container' in tab) void browser.refreshContainer(tab.connectionId, tab.container);
  else void browser.refresh(tab.connectionId);
}

function onViewChanged(id: string): void {
  const tab = tabs.tabs.find((t) => t.id === id);
  if (tab) refreshTreeFor(tab);
}

function onViewCreated(id: string, object: ObjectRef): void {
  const tab = tabs.tabs.find((t) => t.id === id);
  if (!tab) return;
  tabs.setTabObject(tab.id, object);
  void useObjectBrowser().refreshObject(tab.connectionId, object);
}

function isRedisTab(tab: WorkspaceTab): boolean {
  return workspace.connection(tab.connectionId)?.engine === 'redis';
}

function profileName(tab: WorkspaceTab): string {
  return workspace.connection(tab.connectionId)?.name ?? 'the server';
}

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
          :title="tipOf(tab)"
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
              title="Unsaved changes"
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
            :title="closeTip"
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

    <!-- One panel per open tab; inactive ones are hidden (display: none, so out of the accessibility tree). -->
    <div
      v-for="tab in tabs.tabs"
      v-show="tab.id === tabs.activeId.value"
      :id="`wpanel-${tab.id}`"
      :key="tab.id"
      class="panel"
      role="tabpanel"
      :aria-labelledby="`wtab-${tab.id}`"
    >
      <TabScope
        v-if="opened.has(tab.id)"
        :tab-id="tab.id"
      >
        <div
          v-if="reconnectMessage(tab)"
          class="notice tone-warning reconnect"
          role="alert"
        >
          <Icon name="warning" />
          <span class="reconnect-text"><strong>Reconnect needed.</strong> {{ reconnectMessage(tab) }} Reconnecting opens a new session; an open
            transaction in this tab is rolled back.</span>
          <button
            type="button"
            class="btn compact"
            @click="tabs.reconnect(tab.id)"
          >
            <Icon name="refresh" />Reconnect
          </button>
        </div>
        <div class="view">
          <QueryTabView
            v-if="tab.kind === 'query'"
            :tab="tab"
          />
          <KeyBrowser
            v-else-if="tab.kind === 'keys'"
            :tab="tab"
          />
          <template v-else>
            <div
              v-if="tab.status === 'connecting'"
              class="state"
              role="status"
            >
              Connecting to {{ profileName(tab) }}…
            </div>
            <div
              v-else-if="tab.status === 'error' || tab.status === 'cancelled'"
              class="state"
            >
              <div
                v-if="connectError(tab)"
                class="notice tone-danger"
                role="alert"
              >
                <Icon name="warning" />
                <span><strong>{{ connectError(tab)?.title }}.</strong> {{ connectError(tab)?.message }}<template v-if="connectError(tab)?.code"> ({{ connectError(tab)?.code }})</template></span>
              </div>
              <p
                v-else
                class="subtle"
              >
                Not connected.
              </p>
              <!-- A refused Redis init command explains itself and offers to edit the connection. -->
              <InitCommandNotice
                v-if="tab.connectError && isRedisTab(tab)"
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
            <!-- Once mounted, the view stays mounted while a reconnect hides it, so its state survives. -->
            <div
              v-if="viewMounted.has(tab.id)"
              v-show="showsView(tab)"
              :id="`wview-${tab.id}`"
              class="view-body"
            >
              <OverviewTabView
                v-if="tab.kind === 'overview'"
                :tab="tab"
              />
              <component
                :is="viewOf(tab)"
                v-else-if="viewOf(tab)"
                :tab="tab"
                @changed="onViewChanged(tab.id)"
                @dropped="onViewChanged(tab.id)"
                @created="(object: ObjectRef) => onViewCreated(tab.id, object)"
              />
              <div
                v-else
                class="state"
                role="status"
              >
                <p>
                  <strong>{{ tab.title }}</strong>
                </p>
                <p class="subtle">
                  The {{ missingView(tab) }} is not part of this build yet. The tab is connected; close it or open another one.
                </p>
              </div>
            </div>
          </template>
        </div>
      </TabScope>
    </div>
    <HomeView v-if="!active" />
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

/* The tab strip clips overflow: draw the focus ring inside. */
.tab-label:focus-visible {
  outline-offset: -2px;
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

.view-body {
  height: 100%;
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
