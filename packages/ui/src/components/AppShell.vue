<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue';
import { RouterLink, RouterView } from 'vue-router';
import { installExternalChanges } from '../composables/useExternalChanges';
import { installQuitGuard } from '../composables/useQuitGuard';
import { SIDEBAR_MAX, SIDEBAR_MIN, SIDEBAR_STEP, useSidebar } from '../composables/useSidebar';
import { installUpdates, useUpdates } from '../composables/useUpdates';
import { useWorkspace } from '../composables/useWorkspace';
import { useWorkspaceActions } from '../composables/useWorkspaceActions';
import Icon from './common/Icon.vue';
import ToastHost from './common/ToastHost.vue';
import DialogHost from './dialogs/DialogHost.vue';
import UpdateDialog from './dialogs/UpdateDialog.vue';
import ExtensionSlot from './ExtensionSlot.vue';
import WorkspaceTree from './sidebar/WorkspaceTree.vue';
import StatusBar from './StatusBar.vue';
import VarveMark from './VarveMark.vue';

const workspace = useWorkspace();
const actions = useWorkspaceActions();
const updates = useUpdates();
const sidebar = useSidebar();

const isMac = globalThis.document?.documentElement.dataset.platform === 'mac';
/** The shortcut that shows or hides the sidebar: the system one on macOS. */
const toggleKeys = isMac ? '⌃⌘S' : 'Ctrl+B';
const toggleLabel = computed(() => `${sidebar.state.collapsed ? 'Show' : 'Hide'} sidebar (${toggleKeys})`);
/** The button sits in the sidebar's head while the sidebar shows, and in the title bar while it is hidden. */
const sideToggle = ref<HTMLButtonElement | null>(null);
const barToggle = ref<HTMLButtonElement | null>(null);

/** Shows or hides the sidebar. Focus inside the part that hides moves to the button that is left. */
function toggleSidebar(): void {
  const active = globalThis.document?.activeElement ?? null;
  const inSidebar = active instanceof HTMLElement && active.closest('.side') !== null;
  const onBarToggle = active !== null && active === barToggle.value;
  sidebar.toggle();
  if (!inSidebar && !onBarToggle) return;
  void nextTick(() => (sidebar.state.collapsed ? barToggle.value : sideToggle.value)?.focus());
}

function onShortcut(event: KeyboardEvent): void {
  if (event.key.toLowerCase() !== (isMac ? 's' : 'b')) return;
  const held = isMac
    ? event.metaKey && event.ctrlKey && !event.altKey && !event.shiftKey
    : event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey;
  if (!held) return;
  event.preventDefault();
  toggleSidebar();
}

/** The drag in progress on the divider: where it started and how wide the sidebar was. */
let drag: { x: number; width: number } | null = null;

function onDividerDown(event: PointerEvent): void {
  if (event.button !== 0) return;
  drag = { x: event.clientX, width: sidebar.state.width };
  (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  event.preventDefault();
}

function onDividerMove(event: PointerEvent): void {
  if (drag) sidebar.setWidth(drag.width + event.clientX - drag.x, false);
}

function onDividerUp(event: PointerEvent): void {
  if (!drag) return;
  sidebar.setWidth(drag.width + event.clientX - drag.x);
  drag = null;
}

function onDividerKey(event: KeyboardEvent): void {
  const width = sidebar.state.width;
  const next =
    event.key === 'ArrowLeft' ? width - SIDEBAR_STEP
    : event.key === 'ArrowRight' ? width + SIDEBAR_STEP
    : event.key === 'Home' ? SIDEBAR_MIN
    : event.key === 'End' ? SIDEBAR_MAX
    : null;
  if (next === null) return;
  event.preventDefault();
  sidebar.setWidth(next);
}

let removeQuitGuard: (() => void) | null = null;
let removeUpdates: (() => void) | null = null;
let removeExternalChanges: (() => void) | null = null;
let unmounted = false;

onMounted(() => {
  void workspace.refresh();
  globalThis.addEventListener('keydown', onShortcut);
  installQuitGuard()
    .then((remove) => {
      // Unmounted before the guard was ready: remove its listeners at once.
      if (unmounted) remove();
      else removeQuitGuard = remove;
    })
    .catch((error: unknown) => {
      console.error('Could not watch for the window closing', error);
    });
  installUpdates()
    .then((remove) => {
      if (unmounted) remove();
      else removeUpdates = remove;
    })
    .catch((error: unknown) => {
      console.error('Could not set up the update checks', error);
    });
  installExternalChanges()
    .then((remove) => {
      if (unmounted) remove();
      else removeExternalChanges = remove;
    })
    .catch((error: unknown) => {
      console.error('Could not follow changes from MCP clients', error);
    });
});

onBeforeUnmount(() => {
  unmounted = true;
  globalThis.removeEventListener('keydown', onShortcut);
  removeQuitGuard?.();
  removeUpdates?.();
  removeExternalChanges?.();
});
</script>

<template>
  <div
    class="shell"
    :class="{ collapsed: sidebar.state.collapsed }"
    :style="{ '--sidebar-width': `${sidebar.state.width}px` }"
  >
    <!-- Hidden, not removed: the tree keeps what is expanded and where it was scrolled. -->
    <aside
      v-show="!sidebar.state.collapsed"
      class="side"
      aria-label="Sidebar"
    >
      <div
        class="side-head"
        data-tauri-drag-region
      >
        <button
          ref="sideToggle"
          type="button"
          class="icon-btn"
          :title="toggleLabel"
          :aria-label="toggleLabel"
          aria-expanded="true"
          @click="toggleSidebar"
        >
          <Icon
            name="sidebar"
            :size="16"
          />
        </button>
      </div>
      <div class="sidebar">
        <nav
          class="menu"
          aria-label="Main"
        >
          <RouterLink
            class="menu-item"
            to="/"
          >
            <Icon name="home" />Workspace
          </RouterLink>
          <RouterLink
            class="menu-item"
            to="/activity"
          >
            <Icon name="activity" />Activity
          </RouterLink>
          <RouterLink
            class="menu-item"
            to="/settings"
          >
            <Icon name="settings" />Settings
          </RouterLink>
          <ExtensionSlot name="main-menu" />
        </nav>
        <WorkspaceTree class="tree" />
        <ExtensionSlot name="sidebar-sections" />
        <div class="sidebar-foot">
          <button
            type="button"
            class="btn new-connection"
            @click="actions.newConnection()"
          >
            <Icon name="plus" />New connection
          </button>
        </div>
      </div>
      <div
        class="divider"
        role="separator"
        tabindex="0"
        aria-orientation="vertical"
        aria-label="Sidebar width"
        :aria-valuenow="sidebar.state.width"
        :aria-valuemin="SIDEBAR_MIN"
        :aria-valuemax="SIDEBAR_MAX"
        title="Drag to resize, double click to reset"
        @pointerdown="onDividerDown"
        @pointermove="onDividerMove"
        @pointerup="onDividerUp"
        @pointercancel="onDividerUp"
        @dblclick="sidebar.resetWidth()"
        @keydown="onDividerKey"
      />
    </aside>

    <header
      class="titlebar"
      data-tauri-drag-region
    >
      <button
        v-if="sidebar.state.collapsed"
        ref="barToggle"
        type="button"
        class="icon-btn"
        :title="toggleLabel"
        :aria-label="toggleLabel"
        aria-expanded="false"
        @click="toggleSidebar"
      >
        <Icon
          name="sidebar"
          :size="16"
        />
      </button>
      <div
        class="brand"
        data-tauri-drag-region
      >
        <VarveMark :size="18" />
        <span class="brand-name">Varve</span>
      </div>
      <div
        class="titlebar-slot"
        data-tauri-drag-region
      >
        <ExtensionSlot name="title-bar" />
      </div>
    </header>

    <main class="main">
      <!-- The workspace stays alive on the other pages, so its open tabs keep their views and unsaved work. -->
      <RouterView v-slot="{ Component }">
        <KeepAlive include="WorkspaceView">
          <component :is="Component" />
        </KeepAlive>
      </RouterView>
    </main>

    <StatusBar class="status" />
    <!-- Before DialogHost: the unsaved-work questions of an install open on top of it. -->
    <UpdateDialog v-if="updates.state.dialogOpen" />
    <DialogHost />
    <ToastHost />
  </div>
</template>

<style scoped>
/* The sidebar runs the full height beside the title bar, as a system sidebar does: its head
   holds the button that hides it. Hidden, the title bar and the page take the whole width and
   the button moves into the title bar. */
.shell {
  display: grid;
  grid-template-columns: var(--sidebar-width) minmax(0, 1fr);
  grid-template-rows: 44px minmax(0, 1fr) 24px;
  height: 100%;
}

.shell.collapsed {
  grid-template-columns: minmax(0, 1fr);
}

.side {
  position: relative;
  grid-column: 1;
  grid-row: 1 / 3;
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  background: var(--chrome);
  border-right: 1px solid var(--border);
}

.side-head {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: flex-end;
  height: 44px;
  /* 84px clears the macOS window buttons drawn over the bar. */
  padding: 0 8px 0 84px;
  user-select: none;
}

.titlebar {
  grid-row: 1;
  display: flex;
  align-items: center;
  gap: 12px;
  min-width: 0;
  padding: 0 12px;
  background: var(--chrome);
  border-bottom: 1px solid var(--border);
  user-select: none;
}

/* Without the sidebar the title bar starts under the macOS window buttons. */
.shell.collapsed .titlebar {
  padding-left: 84px;
}

/* Windows and Linux draw their window buttons in a frame of their own. */
:root[data-platform='windows'] .side-head,
:root[data-platform='linux'] .side-head {
  padding-left: 8px;
}

:root[data-platform='windows'] .shell.collapsed .titlebar,
:root[data-platform='linux'] .shell.collapsed .titlebar {
  padding-left: 8px;
}

.status {
  grid-column: 1 / -1;
  grid-row: 3;
}

/* The edge of the sidebar: drag it, or use the arrow keys, to change the width. */
.divider {
  position: absolute;
  top: 0;
  right: -3px;
  bottom: 0;
  z-index: 1;
  width: 6px;
  cursor: col-resize;
  touch-action: none;
}

.divider:hover,
.divider:focus-visible {
  outline: none;
  background: linear-gradient(to right, transparent 2px, var(--accent) 2px, var(--accent) 4px, transparent 4px);
}

/* On glass the sidebar is a panel of its own that floats over the window, as on macOS 26. */
:root[data-glass] .side {
  margin: 6px 0 6px 6px;
  border: 1px solid var(--border);
  border-radius: 12px;
  box-shadow: var(--glass-edge);
}

:root[data-glass] .side-head {
  height: 38px;
  padding-left: 78px;
}

:root[data-glass][data-platform='windows'] .side-head,
:root[data-glass][data-platform='linux'] .side-head {
  padding-left: 8px;
}

:root[data-glass] .sidebar {
  border-radius: 0 0 11px 11px;
}

.brand {
  display: flex;
  align-items: center;
  gap: 8px;
}

/* Tauri only drags when the pressed element itself has data-tauri-drag-region,
   so the logo and name let the press fall through to the brand. */
.brand > * {
  pointer-events: none;
}

.brand-name {
  font-weight: 600;
  letter-spacing: -0.01em;
}

.titlebar-slot {
  margin-left: auto;
  display: flex;
  align-items: center;
  gap: 8px;
}

.sidebar {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-height: 0;
  padding: 0 0 8px;
  /* The whole sidebar scrolls, so extension sections below the tree are never clipped. */
  overflow-y: auto;
}

.menu {
  display: flex;
  flex-direction: column;
  padding-bottom: 8px;
  margin-bottom: 4px;
  border-bottom: 1px solid var(--border-inner);
}

.menu-item,
.menu :deep(a) {
  display: flex;
  align-items: center;
  gap: 8px;
  height: var(--row-tree);
  padding: 0 12px;
  color: var(--text);
  text-decoration: none;
}

.menu-item:hover,
.menu :deep(a:hover) {
  background: var(--surface-raised);
  color: var(--text);
}

/* Drawn inside the row: the sidebar clips an outline outside it. */
.menu-item:focus-visible {
  outline-offset: -2px;
}

.menu-item.router-link-exact-active {
  background: var(--accent-tint);
  box-shadow: inset 2px 0 0 var(--accent);
}

.tree {
  flex: 1 0 auto;
}

.sidebar-foot {
  position: sticky;
  bottom: 0;
  padding: 8px 12px;
  background: var(--frost);
  /* Over glass the tree scrolls under this bar: frost it instead of letting rows show through. */
  -webkit-backdrop-filter: var(--overlay-blur);
  backdrop-filter: var(--overlay-blur);
  border-top: 1px solid var(--border-inner);
}

.new-connection {
  width: 100%;
}

.main {
  grid-row: 2;
  min-width: 0;
  min-height: 0;
  overflow: auto;
  background: var(--ground);
}

</style>
