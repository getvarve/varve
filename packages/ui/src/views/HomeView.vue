<script setup lang="ts">
import { computed, inject, ref } from 'vue';
import type { AppInfo } from '../backend';
import { ENGINES } from '../api/records';
import EngineTile from '../components/common/EngineTile.vue';
import EnvBadge from '../components/common/EnvBadge.vue';
import Icon from '../components/common/Icon.vue';
import ExtensionSlot from '../components/ExtensionSlot.vue';
import VarveMark from '../components/VarveMark.vue';
import { useWorkspace } from '../composables/useWorkspace';
import { useWorkspaceActions } from '../composables/useWorkspaceActions';
import { APP_INFO_KEY } from '../keys';
import { ENGINE_INFO } from '../lib/display';

const info = inject(APP_INFO_KEY, ref<AppInfo | null>(null));
const workspace = useWorkspace();
const actions = useWorkspaceActions();
const extensions = computed(() => info.value?.extensions ?? []);
const recent = computed(() => [...workspace.records.value.connections].sort((a, b) => b.updatedAt - a.updatedAt).slice(0, 6));

const ENGINE_VERSIONS: Record<string, string> = {
  postgres: 'Versions 13 to 18',
  mysql: '8.0, 8.4 LTS and 9.x',
  mariadb: '10.6 and later',
  redis: '6.2 and later, Valkey 8',
};
</script>

<template>
  <section class="home">
    <div class="hero">
      <VarveMark :size="56" />
      <div>
        <h1>{{ workspace.isEmpty.value ? 'Welcome to Varve' : 'Varve' }}</h1>
        <p class="lede">
          One app for PostgreSQL, MySQL, MariaDB and Redis.
        </p>
      </div>
    </div>

    <div class="card">
      <div class="card-head">
        <h2>{{ workspace.isEmpty.value ? 'Add your first connection' : 'New connection' }}</h2>
        <span class="subtle">TLS and SSH tunnels are available for every engine</span>
      </div>
      <div class="engines">
        <button
          v-for="engine in ENGINES"
          :key="engine"
          type="button"
          class="engine"
          @click="actions.newConnection(null, engine)"
        >
          <EngineTile :engine="engine" />
          <span class="engine-text">
            <span>{{ ENGINE_INFO[engine].name }}</span>
            <span class="subtle">{{ ENGINE_VERSIONS[engine] }}</span>
          </span>
        </button>
      </div>
    </div>

    <div
      v-if="recent.length > 0"
      class="card"
    >
      <div class="card-head">
        <h2>Connections</h2>
        <span class="subtle">Open a query tab, or right-click a connection in the sidebar for more</span>
      </div>
      <ul class="recent">
        <li
          v-for="c in recent"
          :key="c.id"
        >
          <button
            type="button"
            class="recent-item"
            @click="actions.openConnection(c)"
          >
            <EngineTile :engine="c.engine" />
            <span class="name">{{ c.name }}</span>
            <span class="subtle mono">{{ c.host }}:{{ c.port }}</span>
            <EnvBadge :environment="c.environment" />
          </button>
        </li>
      </ul>
    </div>

    <div
      class="notice tone-neutral"
      role="note"
    >
      <Icon name="lock" />
      <span>Passwords, passphrases and SSH keys stay in your system keychain on this device. They are never written to
        Varve's data, exports or the activity log.</span>
    </div>

    <dl
      v-if="info"
      class="facts"
    >
      <div>
        <dt>Version</dt>
        <dd>{{ info.version }}</dd>
      </div>
      <div>
        <dt>Local data</dt>
        <dd class="mono">
          {{ info.dataDir }}
        </dd>
      </div>
      <div>
        <dt>Extensions</dt>
        <dd>
          <span v-if="extensions.length === 0">None</span>
          <span
            v-for="ext in extensions"
            :key="ext.id"
            class="chip"
          >{{ ext.name }} <span class="mono">{{ ext.id }}</span></span>
        </dd>
      </div>
    </dl>

    <div class="tools">
      <ExtensionSlot name="tools" />
    </div>
  </section>
</template>

<style scoped>
.home {
  display: flex;
  flex-direction: column;
  gap: 20px;
  max-width: 880px;
  padding: 40px 48px;
  overflow: auto;
}

.hero {
  display: flex;
  align-items: center;
  gap: 20px;
}

h1 {
  margin: 0;
  font-size: 24px;
  font-weight: 600;
  letter-spacing: -0.01em;
}

.lede {
  margin: 4px 0 0;
  font-size: 15px;
  color: var(--text-muted);
}

.card {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

.card-head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 12px;
}

.card-head span {
  font-size: 12px;
}

h2 {
  margin: 0;
  font-size: 15px;
  font-weight: 500;
}

.engines {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 8px;
}

.engine {
  display: flex;
  align-items: center;
  gap: 10px;
  height: 52px;
  padding: 0 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
  text-align: left;
  cursor: pointer;
}

.engine:hover {
  border-color: var(--accent);
  background: var(--accent-tint);
}

.engine-text {
  display: flex;
  flex-direction: column;
}

.engine-text .subtle {
  font-size: 11px;
}

.recent {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.recent-item {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  height: 34px;
  padding: 0 8px;
  border: 0;
  border-radius: var(--radius-control);
  background: transparent;
  text-align: left;
  cursor: pointer;
}

.recent-item:hover {
  background: var(--accent-tint);
}

.recent-item .name {
  flex: 1;
}

.recent-item .mono {
  font-size: 12px;
}

.notice {
  align-items: center;
}

.facts {
  display: grid;
  gap: 8px;
  margin: 0;
  padding: 16px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

.facts div {
  display: grid;
  grid-template-columns: 120px 1fr;
  gap: 12px;
}

dt {
  color: var(--text-muted);
}

dd {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin: 0;
}

.mono {
  font-family: var(--font-mono);
  font-size: 12px;
}

.chip {
  display: inline-flex;
  gap: 6px;
  padding: 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
}

.tools:empty {
  display: none;
}

.tools {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
}
</style>
