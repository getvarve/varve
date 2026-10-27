<script setup lang="ts">
import { computed, ref, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { redisInfo } from '../../api/live';
import type { CommandStat, InfoField, RedisInfo } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { formatTime } from '../../lib/display';
import { formatSeconds } from '../../lib/live';
import { formatBytes, formatCount } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import LiveError from './LiveError.vue';
import RefreshControls from './RefreshControls.vue';
import { useAutoRefresh } from './useAutoRefresh';
import { useReporter } from './report';

/**
 * Redis server info (`INFO everything`): the main figures, persistence,
 * keyspace, command statistics, modules, and every other section as the
 * server reports it.
 */
const props = defineProps<{
  /** The tab's Redis session. */
  session: SessionInfo;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const info = shallowRef<RedisInfo | null>(null);
const error = shallowRef<CmdError | null>(null);

const refresh = useAutoRefresh(
  async () => {
    const result = await redisInfo(props.session.id);
    if (!refresh.alive()) return;
    info.value = result;
    error.value = null;
  },
  5000,
  (e) => {
    error.value = report(e);
  },
);

const version = computed(() => {
  const s = info.value?.server;
  if (!s) return '';
  if (info.value?.flavor === 'valkey') return `Valkey ${s.valkeyVersion ?? s.redisVersion ?? ''}`;
  return `Redis ${s.redisVersion ?? ''}`;
});

function n(value: number | null): string {
  return value === null ? 'n/a' : formatCount(value);
}

function b(value: number | null): string {
  return value === null ? 'n/a' : formatBytes(value);
}

type StatSort = 'calls' | 'usec' | 'usecPerCall' | 'failedCalls';
const statSort = ref<StatSort>('usec');
const stats = computed(() => {
  const key = statSort.value;
  const v = (c: CommandStat): number => c[key] ?? 0;
  return [...(info.value?.commandstats ?? [])].sort((a, z) => v(z) - v(a));
});

/** The typed sections' leftover fields plus every untyped section, for the "all fields" list. */
const otherSections = computed(() => {
  const i = info.value;
  if (!i) return [];
  const typed: { name: string; fields: InfoField[] }[] = [
    { name: 'Server', fields: i.server.other },
    { name: 'Clients', fields: i.clients.other },
    { name: 'Memory', fields: i.memory.other },
    { name: 'Persistence', fields: i.persistence.other },
    { name: 'Stats', fields: i.stats.other },
    { name: 'Replication', fields: i.replication.other },
    { name: 'CPU', fields: i.cpu.other },
  ];
  return [...typed, ...i.sections.map((s) => ({ name: s.name || 'Other', fields: s.fields }))].filter((s) => s.fields.length > 0);
});
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Server info</h1>
      <span class="source">{{ version }}</span>
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
      v-if="info"
      class="scroll body"
    >
      <div class="cards">
        <div class="card">
          <span class="k">Uptime</span>
          <span class="v">{{ info.server.uptimeInSeconds === null ? 'n/a' : formatSeconds(info.server.uptimeInSeconds) }}</span>
          <span class="s">{{ info.mode }}{{ info.server.os ? ` · ${info.server.os}` : '' }}</span>
        </div>
        <div class="card">
          <span class="k">Clients</span>
          <span class="v">{{ n(info.clients.connectedClients) }}</span>
          <span class="s">{{ n(info.clients.blockedClients) }} blocked · max {{ n(info.clients.maxclients) }}</span>
        </div>
        <div class="card">
          <span class="k">Memory</span>
          <span class="v">{{ b(info.memory.usedMemory) }}</span>
          <span class="s">peak {{ b(info.memory.usedMemoryPeak) }}{{ info.memory.maxmemory ? ` · max ${formatBytes(info.memory.maxmemory)}` : '' }}</span>
        </div>
        <div class="card">
          <span class="k">Ops per second</span>
          <span class="v">{{ n(info.stats.instantaneousOpsPerSec) }}</span>
          <span class="s">{{ n(info.stats.totalCommandsProcessed) }} commands since start</span>
        </div>
        <div class="card">
          <span class="k">Hit ratio</span>
          <span class="v">{{ info.stats.hitRatio === null ? 'n/a' : `${(info.stats.hitRatio * 100).toFixed(1)}%` }}</span>
          <span class="s">{{ n(info.stats.evictedKeys) }} evicted · {{ n(info.stats.expiredKeys) }} expired</span>
        </div>
      </div>

      <div class="two">
        <section
          class="box"
          aria-label="Persistence"
        >
          <h2>Persistence</h2>
          <dl class="facts">
            <dt>Loading</dt><dd>{{ info.persistence.loading ? 'yes' : 'no' }}</dd>
            <dt>Changes since save</dt><dd>{{ n(info.persistence.rdbChangesSinceLastSave) }}</dd>
            <dt>Last save</dt><dd>{{ info.persistence.rdbLastSaveTime === null ? 'n/a' : formatTime(info.persistence.rdbLastSaveTime * 1000) }} ({{ info.persistence.rdbLastBgsaveStatus ?? 'n/a' }})</dd>
            <dt>Saving now</dt><dd>{{ info.persistence.rdbBgsaveInProgress ? 'yes' : 'no' }}</dd>
            <dt>AOF</dt><dd>{{ info.persistence.aofEnabled ? 'on' : 'off' }}{{ info.persistence.aofEnabled ? ` · last write ${info.persistence.aofLastWriteStatus ?? 'n/a'}` : '' }}</dd>
          </dl>
        </section>
        <section
          class="box"
          aria-label="Keyspace"
        >
          <h2>Keyspace</h2>
          <table class="grid">
            <thead>
              <tr>
                <th>DB</th><th class="num">
                  Keys
                </th><th class="num">
                  With TTL
                </th><th class="num">
                  Avg TTL
                </th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="d in info.keyspace"
                :key="d.db"
              >
                <td class="mono">
                  db{{ d.db }}
                </td>
                <td class="mono num">
                  {{ formatCount(d.keys) }}
                </td>
                <td class="mono num">
                  {{ formatCount(d.expires) }}
                </td>
                <td class="mono num">
                  {{ d.avgTtlMs > 0 ? formatSeconds(d.avgTtlMs / 1000) : '' }}
                </td>
              </tr>
            </tbody>
          </table>
          <p
            v-if="info.keyspace.length === 0"
            class="note"
          >
            No database has keys.
          </p>
        </section>
      </div>

      <section
        v-if="stats.length > 0"
        class="box"
        aria-label="Command statistics"
      >
        <div class="section-head">
          <h2>Command statistics</h2>
          <span class="grow" />
          <label class="check">Sort by
            <select
              v-model="statSort"
              class="select small-select"
            >
              <option value="usec">Total time</option>
              <option value="calls">Calls</option>
              <option value="usecPerCall">Time per call</option>
              <option value="failedCalls">Failed calls</option>
            </select>
          </label>
        </div>
        <div class="table-wrap">
          <table class="grid">
            <thead>
              <tr>
                <th>Command</th><th class="num">
                  Calls
                </th><th class="num">
                  Total time
                </th><th class="num">
                  Per call
                </th><th class="num">
                  Rejected
                </th><th class="num">
                  Failed
                </th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="c in stats"
                :key="c.command"
              >
                <td class="mono">
                  {{ c.command }}
                </td>
                <td class="mono num">
                  {{ formatCount(c.calls) }}
                </td>
                <td class="mono num">
                  {{ (c.usec / 1000).toFixed(1) }} ms
                </td>
                <td class="mono num">
                  {{ c.usecPerCall === null ? '' : `${c.usecPerCall.toFixed(2)} µs` }}
                </td>
                <td class="mono num">
                  {{ c.rejectedCalls ?? '' }}
                </td>
                <td class="mono num">
                  {{ c.failedCalls ?? '' }}
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <section
        v-if="info.modules.length > 0"
        class="box"
        aria-label="Modules"
      >
        <h2>Modules</h2>
        <dl class="facts">
          <template
            v-for="m in info.modules"
            :key="m.name"
          >
            <dt class="mono">
              {{ m.name }}
            </dt><dd>version {{ m.version ?? '?' }}</dd>
          </template>
        </dl>
      </section>

      <details
        v-for="s in otherSections"
        :key="s.name"
        class="box more"
      >
        <summary>{{ s.name }} ({{ s.fields.length }})</summary>
        <dl class="facts">
          <template
            v-for="f in s.fields"
            :key="f.key"
          >
            <dt class="mono">
              {{ f.key }}
            </dt><dd>{{ f.value }}</dd>
          </template>
        </dl>
      </details>
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

.body {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px;
  background: var(--ground);
}

.cards {
  padding: 0;
}

.two {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 12px;
}

.box {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

.box h2 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
}

.box .section-head {
  padding: 0;
}

.table-wrap {
  max-height: 360px;
  overflow: auto;
}

.more summary {
  color: var(--text-muted);
  cursor: pointer;
}

.more .facts {
  margin-top: 8px;
}

.note {
  margin: 0;
}

@media (max-width: 900px) {
  .two {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
