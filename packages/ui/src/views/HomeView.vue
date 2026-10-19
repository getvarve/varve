<script setup lang="ts">
import { computed, inject, ref } from 'vue';
import type { AppInfo } from '../backend';
import ExtensionSlot from '../components/ExtensionSlot.vue';
import VarveMark from '../components/VarveMark.vue';
import { APP_INFO_KEY } from '../keys';

const info = inject(APP_INFO_KEY, ref<AppInfo | null>(null));
const engines = [
  { code: 'PG', name: 'PostgreSQL', color: 'var(--engine-pg)' },
  { code: 'MY', name: 'MySQL', color: 'var(--engine-my)' },
  { code: 'MA', name: 'MariaDB', color: 'var(--engine-ma)' },
  { code: 'RD', name: 'Redis', color: 'var(--engine-rd)' },
];
const extensions = computed(() => info.value?.extensions ?? []);
</script>

<template>
  <section class="home">
    <div class="hero">
      <VarveMark :size="72" />
      <div>
        <h1>{{ info?.productName ?? 'Varve' }}</h1>
        <p class="lede">
          A calm desktop manager for PostgreSQL, MySQL, MariaDB and Redis.
        </p>
      </div>
    </div>

    <ul
      class="engines"
      aria-label="Supported engines"
    >
      <li
        v-for="engine in engines"
        :key="engine.code"
      >
        <span
          class="tile"
          :style="{ background: engine.color }"
        >{{ engine.code }}</span>
        {{ engine.name }}
      </li>
    </ul>

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
  gap: 24px;
  max-width: 880px;
  padding: 48px;
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

.engines {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.engines li {
  display: flex;
  align-items: center;
  gap: 8px;
  height: var(--control);
  padding: 0 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
}

.tile {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 18px;
  border-radius: 3px;
  font-family: var(--font-mono);
  font-size: 10px;
  font-weight: 500;
  color: #14171C;
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
  margin: 0;
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
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
