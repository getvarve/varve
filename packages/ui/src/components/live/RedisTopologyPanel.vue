<script setup lang="ts">
import { computed, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { redisInfo, redisTopology } from '../../api/live';
import type { ClusterNode, RedisInfo, RedisTopology, SentinelPeer } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { CLUSTER_SLOTS, slotCount, slotSegments, slotText } from '../../lib/live';
import { formatCount } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import LiveError from './LiveError.vue';
import RefreshControls from './RefreshControls.vue';
import { useAutoRefresh } from './useAutoRefresh';
import { useReporter } from './report';

/**
 * Redis topology, read only: cluster state, slot map and nodes (cluster
 * mode), the monitored primaries with their replicas and Sentinels (a
 * Sentinel), or the replication of a standalone server (`INFO
 * replication`).
 */
const props = defineProps<{
  /** The tab's Redis session. */
  session: SessionInfo;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const topology = shallowRef<RedisTopology | null>(null);
const info = shallowRef<RedisInfo | null>(null);
const error = shallowRef<CmdError | null>(null);

const refresh = useAutoRefresh(
  async () => {
    const [t, i] = await Promise.all([redisTopology(props.session.id), redisInfo(props.session.id)]);
    if (!refresh.alive()) return;
    topology.value = t;
    info.value = i;
    error.value = null;
  },
  10_000,
  (e) => {
    error.value = report(e);
  },
);

const cluster = computed(() => topology.value?.cluster ?? null);
const sentinel = computed(() => topology.value?.sentinel ?? null);
const replication = computed(() => info.value?.replication ?? null);

/** A stable tag color per primary, from the shared tag palette. */
const TAGS = ['violet', 'blue', 'green', 'amber', 'red', 'gray'] as const;
const primaryColor = computed(() => {
  const map = new Map<string, string>();
  const primaries = (cluster.value?.nodes ?? []).filter((n) => n.role === 'primary');
  primaries.forEach((n, i) => map.set(n.id, `var(--tag-${TAGS[i % TAGS.length] ?? 'gray'})`));
  return map;
});

const segments = computed(() => (cluster.value ? slotSegments(cluster.value.slotMap, cluster.value.unassigned) : []));

function address(n: ClusterNode): string {
  if (n.endpoint && n.endpoint !== n.ip) return `${n.endpoint}:${n.port}`;
  return n.ip ? `${n.ip}:${n.port}` : 'no address';
}

function shortId(id: string | null): string {
  return id ? id.slice(0, 8) : '';
}

function nodeTone(n: ClusterNode): string {
  if (n.failing || n.linkState === 'disconnected' || n.health === 'failed') return 'tone-danger';
  if (n.health === 'loading' || n.flags.includes('handshake')) return 'tone-warning';
  return 'tone-success';
}

function nodeState(n: ClusterNode): string {
  if (n.failing) return n.flags.includes('fail') ? 'failed' : 'possibly failing';
  if (n.linkState === 'disconnected') return 'disconnected';
  return n.health ?? n.linkState ?? 'unknown';
}

function peerAddress(p: SentinelPeer): string {
  return p.ip ? `${p.ip}${p.port !== null ? `:${p.port}` : ''}` : p.name;
}

function ms(value: number | null): string {
  if (value === null) return '';
  return value < 1000 ? `${value} ms` : `${(value / 1000).toFixed(1)} s`;
}
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Topology</h1>
      <span
        v-if="cluster"
        class="source"
      >{{ cluster.source === 'nodes' ? 'CLUSTER NODES' : cluster.source === 'shards' ? 'CLUSTER SHARDS' : 'CLUSTER SLOTS' }}</span>
      <span
        v-else-if="sentinel"
        class="source"
      >SENTINEL MASTERS</span>
      <span
        v-else
        class="source"
      >INFO replication</span>
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

    <div class="scroll body">
      <template v-if="cluster">
        <div class="cards">
          <div class="card">
            <span class="k">Cluster state</span>
            <span
              class="v"
              :class="cluster.state.state === 'ok' ? 'ok' : 'bad'"
            >{{ cluster.state.state ?? 'unknown' }}</span>
          </div>
          <div class="card">
            <span class="k">Slots</span>
            <span class="v">{{ formatCount(cluster.state.slotsAssigned ?? slotCount(cluster.slotMap.map((s) => s.range))) }}</span>
            <span class="s">of {{ formatCount(CLUSTER_SLOTS) }} assigned<template v-if="cluster.state.slotsPfail || cluster.state.slotsFail"> · {{ cluster.state.slotsPfail ?? 0 }} pfail, {{ cluster.state.slotsFail ?? 0 }} fail</template></span>
          </div>
          <div class="card">
            <span class="k">Primaries</span>
            <span class="v">{{ cluster.state.size ?? cluster.nodes.filter((n) => n.role === 'primary').length }}</span>
            <span class="s">{{ cluster.state.knownNodes ?? cluster.nodes.length }} nodes known</span>
          </div>
          <div class="card">
            <span class="k">Epoch</span>
            <span class="v">{{ cluster.state.currentEpoch ?? 'n/a' }}</span>
            <span
              v-if="cluster.state.myEpoch !== null"
              class="s"
            >this node {{ cluster.state.myEpoch }}</span>
          </div>
        </div>

        <section
          class="box"
          aria-label="Slot map"
        >
          <div class="section-head">
            <h2>Slot map</h2>
            <span class="note">0 to {{ CLUSTER_SLOTS - 1 }}</span>
          </div>
          <div
            class="slotbar"
            role="img"
            :aria-label="`Slot map: ${cluster.slotMap.length} assigned ranges, ${slotCount(cluster.unassigned)} unassigned slots`"
          >
            <span
              v-for="s in segments"
              :key="`${s.range.start}`"
              class="seg"
              :class="{ gap: s.nodeId === null }"
              :style="{ left: `${s.start * 100}%`, width: `${s.width * 100}%`, background: s.nodeId ? primaryColor.get(s.nodeId) : undefined }"
              :title="`${slotText([s.range])}: ${s.nodeId ? shortId(s.nodeId) : 'unassigned'}`"
            />
          </div>
          <p
            v-if="cluster.unassigned.length > 0"
            class="notice tone-danger"
          >
            <Icon name="warning" />{{ formatCount(slotCount(cluster.unassigned)) }} slots have no primary: {{ slotText(cluster.unassigned) }}. Keys in them cannot be read or written.
          </p>
        </section>

        <section
          class="box"
          aria-label="Cluster nodes"
        >
          <div class="section-head">
            <h2>Nodes</h2>
          </div>
          <div class="table-wrap">
            <table class="grid">
              <thead>
                <tr>
                  <th>Node</th><th>Role</th><th>Address</th><th>State</th><th class="num">
                    Slots
                  </th><th>Ranges</th><th>Flags</th><th class="num">
                    Offset
                  </th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="n in cluster.nodes"
                  :key="n.id"
                  :class="{ own: n.myself }"
                >
                  <td
                    class="mono"
                    :title="n.id"
                  >
                    <span
                      v-if="n.role === 'primary'"
                      class="swatch"
                      :style="{ background: primaryColor.get(n.id) }"
                    />{{ shortId(n.id) }}<span
                      v-if="n.myself"
                      class="note"
                    > (this connection)</span>
                  </td>
                  <td>
                    {{ n.role }}<span
                      v-if="n.primaryId"
                      class="note mono"
                    > of {{ shortId(n.primaryId) }}</span>
                  </td>
                  <td class="mono">
                    {{ address(n) }}<span
                      v-if="n.hostname"
                      class="note"
                    > {{ n.hostname }}</span>
                  </td>
                  <td>
                    <span
                      class="pill"
                      :class="nodeTone(n)"
                    ><span class="dot" />{{ nodeState(n) }}</span>
                  </td>
                  <td class="mono num">
                    {{ n.slots.length > 0 ? formatCount(slotCount(n.slots)) : '' }}
                  </td>
                  <td
                    class="mono wrap"
                    :title="slotText(n.slots)"
                  >
                    {{ slotText(n.slots) }}<template v-if="n.migrating.length > 0">
                      · migrating {{ n.migrating.length }}
                    </template><template v-if="n.importing.length > 0">
                      · importing {{ n.importing.length }}
                    </template>
                  </td>
                  <td class="mono wrap">
                    {{ n.flags.join(',') }}
                  </td>
                  <td class="mono num">
                    {{ n.replicationOffset === null ? '' : formatCount(n.replicationOffset) }}
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </section>
      </template>

      <template v-else-if="sentinel">
        <p
          v-if="sentinel.masters.length === 0"
          class="empty"
        >
          This Sentinel monitors no primaries.
        </p>
        <section
          v-for="m in sentinel.masters"
          :key="m.master.name"
          class="box"
          :aria-label="`Primary ${m.master.name}`"
        >
          <div class="section-head">
            <h2>{{ m.master.name }}</h2>
            <span class="mono note">{{ peerAddress(m.master) }}</span>
            <span
              class="pill"
              :class="m.master.down ? 'tone-danger' : 'tone-success'"
            ><span class="dot" />{{ m.master.down ? 'down' : 'up' }}</span>
            <span class="grow" />
            <span class="note">quorum {{ m.quorum ?? '?' }} · {{ m.numReplicas ?? m.replicas.length }} replicas · {{ (m.numOtherSentinels ?? m.sentinels.length) + 1 }} sentinels · down after {{ ms(m.downAfterMs) }} · failover timeout {{ ms(m.failoverTimeoutMs) }}</span>
          </div>
          <table class="grid">
            <thead>
              <tr>
                <th>Kind</th><th>Address</th><th>State</th><th>Flags</th><th class="num">
                  Last ping reply
                </th><th class="num">
                  Offset
                </th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="p in [...m.replicas.map((r) => ({ kind: 'replica', p: r })), ...m.sentinels.map((s) => ({ kind: 'sentinel', p: s }))]"
                :key="`${p.kind}-${p.p.name}`"
              >
                <td>{{ p.kind }}</td>
                <td class="mono">
                  {{ peerAddress(p.p) }}
                </td>
                <td>
                  <span
                    class="pill"
                    :class="p.p.down ? 'tone-danger' : 'tone-success'"
                  ><span class="dot" />{{ p.p.down ? 'down' : (p.p.masterLinkStatus ?? 'up') }}</span>
                </td>
                <td class="mono">
                  {{ p.p.flags.join(',') }}
                </td>
                <td class="mono num">
                  {{ ms(p.p.lastOkPingReplyMs) }}
                </td>
                <td class="mono num">
                  {{ p.p.replOffset === null ? '' : formatCount(p.p.replOffset) }}
                </td>
              </tr>
            </tbody>
          </table>
        </section>
      </template>

      <template v-else-if="replication">
        <div class="cards">
          <div class="card">
            <span class="k">Role</span>
            <span class="v">{{ replication.role === 'slave' ? 'replica' : replication.role === 'master' ? 'primary' : (replication.role ?? 'unknown') }}</span>
            <span class="s">standalone server (no cluster, no Sentinel)</span>
          </div>
          <div
            v-if="replication.masterHost"
            class="card"
          >
            <span class="k">Replicates from</span>
            <span class="v small mono">{{ replication.masterHost }}:{{ replication.masterPort ?? '' }}</span>
            <span
              class="s"
              :class="replication.masterLinkStatus === 'down' ? 'bad' : ''"
            >link {{ replication.masterLinkStatus ?? 'unknown' }}<template v-if="replication.masterLastIoSecondsAgo !== null"> · last IO {{ replication.masterLastIoSecondsAgo }} s ago</template></span>
          </div>
          <div class="card">
            <span class="k">Replication offset</span>
            <span class="v small">{{ replication.masterReplOffset === null ? 'n/a' : formatCount(replication.masterReplOffset) }}</span>
            <span
              v-if="replication.replBacklogSize !== null"
              class="s"
            >backlog {{ formatCount(replication.replBacklogSize) }} bytes</span>
          </div>
        </div>
        <section
          v-if="replication.replicas.length > 0"
          class="box"
          aria-label="Replicas"
        >
          <div class="section-head">
            <h2>Replicas</h2>
            <span class="note">{{ replication.connectedReplicas ?? replication.replicas.length }} connected</span>
          </div>
          <table class="grid">
            <thead>
              <tr>
                <th>Address</th><th>State</th><th class="num">
                  Offset
                </th><th class="num">
                  Behind
                </th><th class="num">
                  Lag
                </th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(r, i) in replication.replicas"
                :key="i"
              >
                <td class="mono">
                  {{ r.ip ?? '' }}{{ r.port !== null ? `:${r.port}` : '' }}
                </td>
                <td>{{ r.state ?? '' }}</td>
                <td class="mono num">
                  {{ r.offset === null ? '' : formatCount(r.offset) }}
                </td>
                <td class="mono num">
                  {{ r.offset !== null && replication.masterReplOffset !== null ? formatCount(Math.max(0, replication.masterReplOffset - r.offset)) : '' }}
                </td>
                <td class="mono num">
                  {{ r.lag === null ? '' : `${r.lag} s` }}
                </td>
              </tr>
            </tbody>
          </table>
        </section>
        <p
          v-else
          class="note"
        >
          No replicas are connected.
        </p>
      </template>
      <div
        v-else-if="!error"
        class="empty"
      >
        Loading…
      </div>
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

.box {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

.box .section-head {
  flex-wrap: wrap;
  padding: 0;
}

.slotbar {
  position: relative;
  height: 18px;
  border-radius: var(--radius-control);
  background: var(--surface-raised);
  overflow: hidden;
}

.seg {
  position: absolute;
  top: 0;
  bottom: 0;
  min-width: 1px;
}

.seg.gap {
  background: repeating-linear-gradient(45deg, var(--danger-tint), var(--danger-tint) 4px, var(--danger) 4px, var(--danger) 5px);
}

.swatch {
  display: inline-block;
  width: 8px;
  height: 8px;
  margin-right: 6px;
  border-radius: 2px;
}

.table-wrap {
  max-height: 480px;
  overflow: auto;
}

.ok {
  color: var(--success-text);
}

.bad {
  color: var(--danger-text);
}

.v.small {
  font-size: 15px;
}

.note {
  margin: 0;
}
</style>
