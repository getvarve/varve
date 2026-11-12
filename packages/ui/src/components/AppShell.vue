<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { RouterLink, RouterView } from 'vue-router';
import { setGlassShapes } from '../backend';
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

const shell = ref<HTMLElement | null>(null);
const card = ref<HTMLElement | null>(null);

/** The sidebar card's corner radius, the same number as in the styles below. */
const CARD_RADIUS = 18;

/** Whether Liquid Glass lies behind single shapes of this window (macOS 26, `lib/glass.ts`). */
function glassShapesOn(): boolean {
  return globalThis.document?.documentElement.dataset.glassShapes !== undefined;
}

let glassWasOn = false;

/**
 * Tells the backend where the sidebar card is, so the system draws glass exactly behind it: after the card
 * appears, moves, changes size or goes away. Without Liquid Glass it does nothing, apart from taking the glass
 * away once when it was just switched off (the user asked for less transparency).
 */
function reportGlass(): void {
  const on = glassShapesOn();
  if (!on && !glassWasOn) return;
  glassWasOn = on;
  const el = card.value;
  const shapes = [];
  if (on && el && !sidebar.state.collapsed) {
    const box = el.getBoundingClientRect();
    if (box.width > 0 && box.height > 0) {
      shapes.push({ id: 'sidebar', x: box.left, y: box.top, width: box.width, height: box.height, radius: CARD_RADIUS });
    }
  }
  setGlassShapes(shapes).catch((error: unknown) => {
    console.error('Could not place the glass behind the sidebar', error);
  });
}

// A hidden card has no size for the observer to see change, and a drag should not wait for it.
watch(
  () => [sidebar.state.collapsed, sidebar.state.width],
  () => void nextTick(reportGlass),
);

let cardSize: ResizeObserver | null = null;
let glassMark: MutationObserver | null = null;

/** The window lost or got the focus: the selected sidebar row is grey in a window that is not in front. */
function onWindowFocus(event: FocusEvent): void {
  const root = globalThis.document?.documentElement;
  if (!root) return;
  if (event.type === 'blur') root.dataset.windowInactive = '';
  else delete root.dataset.windowInactive;
}

/** How many pixels the texts of the sidebar's rows are cut off by, for the row that lacks the most. */
function cutOff(side: Element): number {
  let most = 0;
  for (const row of side.querySelectorAll('.row, .menu-item')) {
    let lacking = 0;
    for (const part of row.children) lacking += Math.max(0, part.scrollWidth - part.clientWidth);
    most = Math.max(most, lacking);
  }
  return most;
}

/**
 * Makes the sidebar as wide as its rows need: no text cut off, and no wider than that. It measures the rows on
 * screen at the smallest width and widens until nothing is cut off any more (a part of a row may take a share of
 * the row's width, so one step is not always enough).
 */
function fitWidth(): void {
  const root = shell.value;
  const side = root?.querySelector('.side');
  if (!root || !side) return;
  let width = SIDEBAR_MIN;
  for (let pass = 0; pass < 8; pass++) {
    root.style.setProperty('--sidebar-width', `${width}px`);
    const lacking = cutOff(side);
    if (lacking === 0 || width >= SIDEBAR_MAX) break;
    width = Math.min(SIDEBAR_MAX, width + lacking + 1);
  }
  sidebar.setWidth(width);
  // The measuring wrote the width straight to the element; leave it at the width that was chosen.
  root.style.setProperty('--sidebar-width', `${sidebar.state.width}px`);
}

function onDividerKey(event: KeyboardEvent): void {
  const width = sidebar.state.width;
  const next =
    event.key === 'ArrowLeft' ? width - SIDEBAR_STEP
    : event.key === 'ArrowRight' ? width + SIDEBAR_STEP
    : event.key === 'Home' ? SIDEBAR_MIN
    : event.key === 'End' ? SIDEBAR_MAX
    : null;
  if (event.key === 'Enter') {
    event.preventDefault();
    fitWidth();
    return;
  }
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
  globalThis.addEventListener('blur', onWindowFocus);
  globalThis.addEventListener('focus', onWindowFocus);
  globalThis.addEventListener('resize', reportGlass);
  // The card changes size with the window and with the sidebar's width; the mark for Liquid Glass arrives once the
  // backend has answered, and goes when the user asks for less transparency.
  if (typeof ResizeObserver === 'function' && card.value) {
    cardSize = new ResizeObserver(reportGlass);
    cardSize.observe(card.value);
  }
  if (typeof MutationObserver === 'function') {
    glassMark = new MutationObserver(reportGlass);
    glassMark.observe(document.documentElement, { attributes: true, attributeFilter: ['data-glass-shapes'] });
  }
  reportGlass();
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
  globalThis.removeEventListener('blur', onWindowFocus);
  globalThis.removeEventListener('focus', onWindowFocus);
  globalThis.removeEventListener('resize', reportGlass);
  cardSize?.disconnect();
  glassMark?.disconnect();
  removeQuitGuard?.();
  removeUpdates?.();
  removeExternalChanges?.();
});
</script>

<template>
  <div
    ref="shell"
    class="shell"
    :class="{ collapsed: sidebar.state.collapsed }"
    :style="{ '--sidebar-width': `${sidebar.state.width}px` }"
  >
    <!-- Hidden, not removed: the tree keeps what is expanded and where it was scrolled. -->
    <aside
      v-show="!sidebar.state.collapsed"
      ref="card"
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
            <Icon
              name="home"
              :size="16"
            />Workspace
          </RouterLink>
          <RouterLink
            class="menu-item"
            to="/activity"
          >
            <Icon
              name="activity"
              :size="16"
            />Activity
          </RouterLink>
          <RouterLink
            class="menu-item"
            to="/settings"
          >
            <Icon
              name="settings"
              :size="16"
            />Settings
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
        title="Drag to resize, double click to fit the text"
        @pointerdown="onDividerDown"
        @pointermove="onDividerMove"
        @pointerup="onDividerUp"
        @pointercancel="onDividerUp"
        @dblclick="fitWidth"
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
/* The window frame follows the shared sizes of the app family, measured from a macOS 26 window: the sidebar is a
   card that floats 8px inside the window with 10px to the content, its head holds the button that hides it on the
   line of the window buttons, and the title strip is 52px with no line under it. Hidden, the title strip and the
   page take the whole width and the button moves into the title strip. */
.shell {
  display: grid;
  grid-template-columns: var(--sidebar-width) minmax(0, 1fr);
  grid-template-rows: 52px minmax(0, 1fr) 24px;
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
  margin: 8px 10px 8px 8px;
  background: var(--chrome);
  border: 1px solid var(--border);
  /* Concentric with the window's 26px corners, 8px further in. */
  border-radius: 18px;
  box-shadow: var(--shadow-card), var(--glass-edge);
}

/* 44px, so the first row starts 52px from the top of the window. The button's centre sits 26px from the top of the
   window, on the line of the macOS window buttons, and its glyph ends 13px from the card's edge. */
.side-head {
  flex: none;
  display: flex;
  align-items: flex-start;
  justify-content: flex-end;
  height: 44px;
  padding: 3px 8px 0 0;
  user-select: none;
}

.titlebar {
  grid-row: 1;
  display: flex;
  align-items: center;
  gap: 12px;
  min-width: 0;
  padding: 0 8px 0 10px;
  user-select: none;
}

/* With Liquid Glass the system draws the card: glass lies exactly behind it, under the clear page, so the card
   paints no fill, edge or shadow of its own. The window's own background shows in the gaps around it. */
:root[data-glass-shapes] .side {
  background: transparent;
  border-color: transparent;
  box-shadow: none;
}

/* The page is clear there, so the rest of the shell paints itself. */
:root[data-glass-shapes] .titlebar {
  background: var(--ground);
}

:root[data-glass-shapes] .sidebar-foot {
  background: transparent;
  -webkit-backdrop-filter: none;
  backdrop-filter: none;
  border-top-color: transparent;
}

/* Without the sidebar the title strip starts under the macOS window buttons. */
.shell.collapsed .titlebar {
  padding-left: 84px;
}

/* Windows and Linux draw their window buttons in a frame of their own. */
:root[data-platform='windows'] .shell.collapsed .titlebar,
:root[data-platform='linux'] .shell.collapsed .titlebar {
  padding-left: 8px;
}

.status {
  grid-column: 1 / -1;
  grid-row: 3;
}

/* The edge of the sidebar: drag it, double click it, or use the arrow keys, to change the width. The grip lies over
   the card's edge itself and reaches into the gap beside it, so it is found where the edge is seen. */
.divider {
  position: absolute;
  top: 12px;
  right: -8px;
  bottom: 12px;
  z-index: 5;
  width: 14px;
  cursor: col-resize;
  touch-action: none;
}

/* A line on the card's edge while the grip is under the pointer or has focus. */
.divider::after {
  content: '';
  position: absolute;
  top: 0;
  bottom: 0;
  left: 5px;
  width: 2px;
  border-radius: 1px;
  background: transparent;
}

.divider:hover::after,
.divider:focus-visible::after {
  background: var(--accent);
}

.divider:focus-visible {
  outline: none;
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
  font-size: 15px;
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
  min-height: 0;
  padding: 0 0 8px;
  border-radius: 0 0 17px 17px;
  /* The whole sidebar scrolls, so extension sections below the tree are never clipped. */
  overflow-y: auto;
}

/* The navigation rows: 32px each with no gap, the fill 10px inside the card on both sides. A group without a label
   is set apart by 13px of space. */
.menu {
  display: flex;
  flex-direction: column;
  padding: 0 10px;
  margin-bottom: 13px;
}

.menu-item,
.menu :deep(a) {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 32px;
  padding: 0 12px;
  border-radius: 8px;
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

/* The selected row is the system's rounded fill: grey while the content has focus or the window is not in front, the system accent colour with
   its own text colour while the sidebar has focus. The brand colour is only the fallback where the webview does not
   know the system accent. */
.menu-item.router-link-exact-active {
  background: var(--row-selected);
}

:root:not([data-window-inactive]) .side:focus-within .menu-item.router-link-exact-active {
  background: var(--accent);
  background: AccentColor;
  color: #FFFFFF;
  color: AccentColorText;
}

.tree {
  flex: 1 0 auto;
}

.sidebar-foot {
  position: sticky;
  bottom: 0;
  padding: 8px 10px;
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
