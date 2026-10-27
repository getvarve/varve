<script setup lang="ts">
import { computed, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { serverStatus } from '../../api/live';
import type { ReplicationRole, ServerStatus } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { formatSeconds } from '../../lib/live';
import { formatBytes, formatCount } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import GaleraSummary from './GaleraSummary.vue';
import LiveError from './LiveError.vue';
import RefreshControls from './RefreshControls.vue';
import { useAutoRefresh } from './useAutoRefresh';
import { useReporter } from './report';

/**
 * Server status of a PostgreSQL, MySQL or MariaDB server: version, uptime,
 * connections, cache hit ratio, database sizes, replication and (MariaDB)
 * Galera. Reads on the profile's admin session.
 */
const props = defineProps<{
  session: SessionInfo;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const status = shallowRef<ServerStatus | null>(null);
const error = shallowRef<CmdError | null>(null);

const refresh = useAutoRefresh(
  async () => {
    const result = await serverStatus(props.session.id);
    if (!refresh.alive()) return;
    status.value = result;
    error.value = null;
  },
  10_000,
  (e) => {
    error.value = report(e);
  },
);

const ROLE_LABELS: Record<ReplicationRole, string> = {
  primary: 'Primary',
  replica: 'Replica',
  galeraMember: 'Galera cluster member',
  standalone: 'Standalone',
  unknown: 'Unknown',
};

const connectionShare = computed(() => {
  const s = status.value;
  if (!s || s.connections === null || !s.maxConnections) return null;
  return Math.min(1, s.connections / s.maxConnections);
});

function seconds(value: number | null): string {
  if (value === null) return '';
  return value < 1 ? `${value.toFixed(2)} s` : formatSeconds(value);
}

function bytes(value: number | null): string {
  return value === null ? '' : formatBytes(value);
}
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Server status</h1>
      <span
        v-if="status"
        class="source"
      >{{ status.version }}</span>
      <EnvBadge :environment="session.environment" />
      <span class="grow" />
      <RefreshControls
        :state="refresh.state.value"
        @interval="refresh.setInterval"
        @pause="refresh.pause"
        @resume="refresh.resume"
        @refresh="refresh.refreshNow"
      />
    </div>

    <LiveError
      v-if="error"
      :error="error"
      class="panel-error"
      @reconnect="emit('reconnect')"
    />

    <div
      v-if="status"
      class="scroll"
    >
      <p
        v-if="status.versionDetail"
        class="note detail-line"
      >
        {{ status.versionDetail }}
      </p>
      <div class="cards">
        <div class="card">
          <span class="k">Uptime</span>
          <span class="v">{{ status.uptimeSeconds === null ? 'n/a' : formatSeconds(status.uptimeSeconds) }}</span>
          <span
            v-if="status.startedAt"
            class="s"
          >since {{ status.startedAt }}</span>
        </div>
        <div class="card">
          <span class="k">Connections</span>
          <span class="v">{{ status.connections === null ? 'n/a' : formatCount(status.connections) }}</span>
          <span
            v-if="status.maxConnections !== null"
            class="s"
          >of {{ formatCount(status.maxConnections) }} allowed{{ status.maxUsedConnections !== null ? ` · peak ${formatCount(status.maxUsedConnections)}` : '' }}</span>
          <span
            v-if="connectionShare !== null"
            class="meter"
            role="img"
            :aria-label="`${Math.round(connectionShare * 100)}% of the connection limit in use`"
          ><span
            :style="{ width: `${connectionShare * 100}%`, background: connectionShare >= 0.8 ? 'var(--danger)' : connectionShare >= 0.6 ? 'var(--warning)' : 'var(--accent)' }"
          /></span>
        </div>
        <div class="card">
          <span class="k">Active queries</span>
          <span class="v">{{ status.activeQueries === null ? 'n/a' : formatCount(status.activeQueries) }}</span>
          <span class="s">not counting this panel</span>
        </div>
        <div class="card">
          <span class="k">Cache hit ratio</span>
          <span class="v">{{ status.cacheHitRatio === null ? 'n/a' : `${(status.cacheHitRatio * 100).toFixed(2)}%` }}</span>
          <span class="s">{{ status.engine === 'postgres' ? 'shared buffers' : 'InnoDB buffer pool' }}, since the last reset</span>
        </div>
        <div class="card">
          <span class="k">Replication</span>
          <span class="v role">{{ ROLE_LABELS[status.replication.role] }}</span>
          <span
            v-if="!status.replication.visible && status.replication.message"
            class="s"
          >{{ status.replication.message }}</span>
        </div>
      </div>

      <section
        v-if="status.replication.upstream.length > 0"
        class="block"
        aria-label="Replication sources"
      >
        <h2>Replicates from</h2>
        <table class="grid">
          <thead>
            <tr>
              <th>Channel</th><th>Source</th><th>State</th><th>IO / SQL</th><th class="num">
                Lag
              </th><th class="num">
                Lag bytes
              </th><th>Position</th><th>Last error</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="(u, i) in status.replication.upstream"
              :key="i"
            >
              <td>{{ u.channel || 'default' }}</td>
              <td class="mono">
                {{ u.sourceHost ?? '' }}{{ u.sourcePort !== null ? `:${u.sourcePort}` : '' }}
              </td>
              <td>{{ u.state ?? '' }}</td>
              <td>{{ [u.ioRunning, u.sqlRunning].filter(Boolean).join(' / ') }}</td>
              <td
                class="mono num"
                :title="u.lagNote ?? ''"
              >
                {{ u.lagSeconds === null ? 'unknown' : seconds(u.lagSeconds) }}
              </td>
              <td class="mono num">
                {{ bytes(u.lagBytes ?? u.primaryLagBytes) }}
              </td>
              <td
                class="mono wrap"
                :title="u.gtidPosition ?? u.replayPosition ?? ''"
              >
                {{ u.gtidPosition ?? u.replayPosition ?? u.receivePosition ?? '' }}
              </td>
              <td class="wrap bad">
                {{ u.lastError ?? '' }}
              </td>
            </tr>
          </tbody>
        </table>
        <p
          v-for="(u, i) in status.replication.upstream.filter((x) => x.lagNote)"
          :key="`n${i}`"
          class="note"
        >
          {{ u.lagNote }}
        </p>
      </section>

      <section
        v-if="status.replication.replicas.length > 0"
        class="block"
        aria-label="Replicas"
      >
        <h2>Replicas</h2>
        <table class="grid">
          <thead>
            <tr>
              <th>Name</th><th>Address</th><th>State</th><th>Sync</th><th class="num">
                Lag
              </th><th class="num">
                Lag bytes
              </th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="(r, i) in status.replication.replicas"
              :key="i"
            >
              <td>{{ r.name ?? '' }}</td>
              <td class="mono">
                {{ r.address ?? '' }}
              </td>
              <td>{{ r.state ?? '' }}</td>
              <td>{{ r.syncState ?? '' }}</td>
              <td class="mono num">
                {{ seconds(r.lagSeconds) }}
              </td>
              <td class="mono num">
                {{ bytes(r.lagBytes) }}
              </td>
            </tr>
          </tbody>
        </table>
      </section>

      <section
        v-if="status.databases.length > 0"
        class="block"
        aria-label="Databases"
      >
        <h2>Databases</h2>
        <table class="grid">
          <thead>
            <tr>
              <th>Database</th><th class="num">
                Size
              </th><th class="num">
                Connections
              </th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="d in status.databases"
              :key="d.name"
            >
              <td>{{ d.name }}</td>
              <td class="mono num">
                {{ d.sizeBytes === null ? 'no access' : formatBytes(d.sizeBytes) }}
              </td>
              <td class="mono num">
                {{ d.connections === null ? '' : formatCount(d.connections) }}
              </td>
            </tr>
          </tbody>
        </table>
      </section>

      <section
        v-if="status.galera"
        class="block"
        aria-label="Galera cluster"
      >
        <h2>Galera cluster</h2>
        <GaleraSummary :galera="status.galera" />
      </section>
    </div>
    <div
      v-else-if="!error"
      class="empty"
    >
      Loading…
    </div>
  </div>
</template>

<style scoped src="./panel.css"></style>

<style scoped>
.panel-error {
  margin: 8px 16px 0;
}

.detail-line {
  margin: 12px 16px 0;
}

.role {
  font-family: var(--font-sans);
  font-size: 15px;
}

.block {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 8px 16px 16px;
}

.block h2 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
}

.bad {
  color: var(--danger-text);
}

.note {
  margin: 0;
}
</style>
