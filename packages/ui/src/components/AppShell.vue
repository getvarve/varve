<script setup lang="ts">
import { computed, inject, onBeforeUnmount, onMounted, ref } from 'vue';
import type { AppInfo } from '../backend';
import { RouterLink, RouterView } from 'vue-router';
import { installQuitGuard } from '../composables/useQuitGuard';
import { useWorkspace } from '../composables/useWorkspace';
import { useWorkspaceActions } from '../composables/useWorkspaceActions';
import { APP_INFO_KEY } from '../keys';
import Icon from './common/Icon.vue';
import ToastHost from './common/ToastHost.vue';
import DialogHost from './dialogs/DialogHost.vue';
import ExtensionSlot from './ExtensionSlot.vue';
import WorkspaceTree from './sidebar/WorkspaceTree.vue';
import StatusBar from './StatusBar.vue';
import VarveMark from './VarveMark.vue';

const info = inject(APP_INFO_KEY, ref<AppInfo | null>(null));
const productName = computed(() => info.value?.productName ?? 'Varve');
const isCommunity = computed(() => info.value?.edition !== 'pro');
const workspace = useWorkspace();
const actions = useWorkspaceActions();

let removeQuitGuard: (() => void) | null = null;
let unmounted = false;

onMounted(() => {
  void workspace.refresh();
  installQuitGuard()
    .then((remove) => {
      // Unmounted before the guard was ready: remove its listeners at once.
      if (unmounted) remove();
      else removeQuitGuard = remove;
    })
    .catch((error: unknown) => {
      console.error('Could not watch for the window closing', error);
    });
});

onBeforeUnmount(() => {
  unmounted = true;
  removeQuitGuard?.();
});
</script>

<template>
  <div class="shell">
    <header
      class="titlebar"
      data-tauri-drag-region
    >
      <div
        class="brand"
        data-tauri-drag-region
      >
        <VarveMark :size="18" />
        <span class="brand-name">{{ productName }}</span>
      </div>
      <div
        class="titlebar-slot"
        data-tauri-drag-region
      >
        <ExtensionSlot name="title-bar" />
      </div>
    </header>

    <div class="body">
      <aside
        class="sidebar"
        aria-label="Connections"
      >
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
          <RouterLink
            v-if="isCommunity"
            class="menu-item"
            to="/about-pro"
          >
            <Icon name="sparkle" />About Varve Pro
          </RouterLink>
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
      </aside>

      <main class="main">
        <RouterView />
      </main>
    </div>

    <StatusBar />
    <DialogHost />
    <ToastHost />
  </div>
</template>

<style scoped>
.shell {
  display: grid;
  grid-template-rows: 44px 1fr 24px;
  height: 100%;
}

.titlebar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 0 12px 0 84px;
  background: var(--surface);
  border-bottom: 1px solid var(--border);
  user-select: none;
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

.body {
  display: grid;
  grid-template-columns: 280px 1fr;
  min-height: 0;
}

.sidebar {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-height: 0;
  padding: 8px 0;
  background: var(--surface);
  border-right: 1px solid var(--border);
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
  background: var(--surface);
  border-top: 1px solid var(--border-inner);
}

.new-connection {
  width: 100%;
}

.main {
  min-width: 0;
  min-height: 0;
  overflow: auto;
  background: var(--ground);
}

</style>
