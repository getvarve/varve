<script setup lang="ts">
import { computed, inject, ref } from 'vue';
import type { AppInfo } from '../backend';
import { RouterLink, RouterView } from 'vue-router';
import { APP_INFO_KEY } from '../keys';
import ExtensionSlot from './ExtensionSlot.vue';
import VarveMark from './VarveMark.vue';

const info = inject(APP_INFO_KEY, ref<AppInfo | null>(null));
const productName = computed(() => info.value?.productName ?? 'Varve');
const isCommunity = computed(() => info.value?.edition !== 'pro');
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
      <div class="titlebar-slot">
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
            Home
          </RouterLink>
          <ExtensionSlot name="main-menu" />
          <RouterLink
            v-if="isCommunity"
            class="menu-item"
            to="/about-pro"
          >
            About Varve Pro
          </RouterLink>
        </nav>
        <div class="section-label">
          Connections
        </div>
        <p class="empty">
          No connections yet.
        </p>
        <ExtensionSlot name="sidebar-sections" />
      </aside>

      <main class="main">
        <RouterView />
      </main>
    </div>

    <footer class="statusbar">
      <span>{{ productName }}</span>
      <span v-if="info">v{{ info.version }}</span>
      <span class="spacer" />
      <span v-if="info">
        {{ info.extensions.length === 1 ? '1 extension' : `${info.extensions.length} extensions` }}
      </span>
    </footer>
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
  padding: 8px 0;
  background: var(--surface);
  border-right: 1px solid var(--border);
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

.section-label {
  height: var(--row-tree);
  display: flex;
  align-items: center;
  padding: 0 12px;
  font-size: 11px;
  font-weight: 500;
  letter-spacing: 0.06em;
  text-transform: uppercase;
  color: var(--text-muted);
}

.empty {
  margin: 0;
  padding: 0 12px;
  color: var(--text-subtle);
}

.main {
  min-width: 0;
  overflow: auto;
  background: var(--ground);
}

.statusbar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 0 12px;
  font-size: 12px;
  color: var(--text-muted);
  background: var(--surface);
  border-top: 1px solid var(--border);
}

.spacer {
  flex: 1;
}
</style>
