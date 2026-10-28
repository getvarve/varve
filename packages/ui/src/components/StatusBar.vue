<script setup lang="ts">
import { computed, inject, ref } from 'vue';
import { useRoute } from 'vue-router';
import type { AppInfo } from '../backend';
import { useTabs } from '../composables/useTabs';
import { useUpdates } from '../composables/useUpdates';
import { useWorkspace } from '../composables/useWorkspace';
import { APP_INFO_KEY } from '../keys';
import { ENGINE_INFO, ENVIRONMENT_INFO, encryptionStatus } from '../lib/display';
import { formatCount, formatDuration } from '../lib/values';

const info = inject(APP_INFO_KEY, ref<AppInfo | null>(null));
const tabs = useTabs();
const workspace = useWorkspace();
const route = useRoute();
const updates = useUpdates();

/** The active tab's connection, only while the workspace (not Activity or Settings) is shown. */
const tab = computed(() => (route.name === 'home' ? tabs.active.value : null));
const profile = computed(() => (tab.value ? workspace.connection(tab.value.connectionId) : null));
const env = computed(() => (profile.value ? ENVIRONMENT_INFO[profile.value.environment] : null));
const session = computed(() => tab.value?.session ?? null);
const encryption = computed(() =>
  session.value && profile.value ? encryptionStatus(session.value.encryption, profile.value.ssh.enabled) : null,
);

const lastRun = computed(() => {
  const t = tab.value;
  // A failed run shows its error in the results, not as the last run's counts.
  if (!t || t.kind !== 'query' || !t.run?.output || t.run.error) return null;
  const output = t.run.output;
  let rows: number | null = null;
  for (const r of output.results) {
    if (r.kind === 'rows') rows = r.rows.length;
    else if (r.rowsAffected !== null) rows = r.rowsAffected;
  }
  return { rows, elapsed: output.elapsedMs };
});
</script>

<template>
  <footer
    class="statusbar"
    aria-label="Status"
  >
    <template v-if="profile && env">
      <span
        class="band"
        :style="{ background: env.color, color: env.onColor }"
      >{{ env.short }}</span>
      <span class="seg">
        {{ profile.name }} · {{ ENGINE_INFO[profile.engine].name }}<template v-if="session"> {{ session.server.version }}</template><template v-if="profile.ssh.enabled"> · via SSH</template>
      </span>
      <span
        v-if="tab?.status === 'connecting'"
        class="seg"
      >Connecting…</span>
      <span
        v-else-if="tab && tab.status !== 'ready'"
        class="seg"
      >Not connected</span>
      <span
        v-if="encryption && tab?.status === 'ready'"
        class="seg"
        :class="{ badge: encryption.warn }"
        :title="encryption.hint"
      >{{ encryption.label }}<template v-if="encryption.note"> · {{ encryption.note }}</template></span>
      <span
        v-if="session?.readOnly"
        class="seg badge"
      >Read only</span>
    </template>
    <template v-else>
      <span class="seg">No active connection</span>
      <span class="seg">{{ info?.productName ?? 'Varve' }}<template v-if="info"> v{{ info.version }}</template></span>
    </template>
    <span class="spacer" />
    <span
      v-if="lastRun"
      class="seg end mono"
    >
      <template v-if="lastRun.rows !== null">{{ formatCount(lastRun.rows) }} {{ lastRun.rows === 1 ? 'row' : 'rows' }} · </template>{{ formatDuration(lastRun.elapsed) }}
    </span>
    <button
      v-if="updates.notice.value"
      type="button"
      class="seg end update"
      @click="updates.openNotice()"
    >
      {{ updates.notice.value }}
    </button>
    <span
      v-if="info"
      class="seg end"
    >{{ info.extensions.length === 1 ? '1 extension' : `${info.extensions.length} extensions` }}</span>
  </footer>
</template>

<style scoped>
.statusbar {
  display: flex;
  align-items: center;
  height: 24px;
  font-size: 12px;
  color: var(--text-muted);
  background: var(--chrome);
  border-top: 1px solid var(--border);
  white-space: nowrap;
  overflow: hidden;
}

.band {
  display: flex;
  align-items: center;
  height: 100%;
  padding: 0 10px;
  font-weight: 500;
}

.seg {
  padding: 0 12px;
  border-right: 1px solid var(--border);
}

.seg.end {
  border-right: 0;
  border-left: 1px solid var(--border);
}

.badge {
  color: var(--warning-text);
}

/* A segment that opens the update dialog: plain text, only the color says it can be clicked. */
.update {
  height: 100%;
  margin: 0;
  border-top: 0;
  border-bottom: 0;
  background: none;
  font: inherit;
  color: var(--accent);
  cursor: pointer;
}

.update:hover {
  background: var(--surface-raised);
}

/* The bar clips overflow: draw the focus ring inside. */
.update:focus-visible {
  outline-offset: -2px;
}

.spacer {
  flex: 1;
}
</style>
