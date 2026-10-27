<script setup lang="ts">
import { computed, nextTick, onMounted, ref, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import {
  redisClientKill,
  redisClients,
  redisInfo,
  redisLatency,
  redisLatencyHistory,
  redisLatencyReset,
  redisSlowlog,
  redisSlowlogReset,
} from '../../api/live';
import type { ClientInfo, LatencyReport, LatencySample, RedisInfo, SlowLog } from '../../api/live';
import type { SessionInfo, Value } from '../../api/sessions';
import { useToasts } from '../../composables/useToasts';
import { formatTime } from '../../lib/display';
import { clientType, filterClients, formatSeconds } from '../../lib/live';
import type { ClientType } from '../../lib/live';
import { cellText, formatBytes, formatCount } from '../../lib/values';
import Icon from '../common/Icon.vue';
import LiveConfirm from './LiveConfirm.vue';
import LiveError from './LiveError.vue';
import RefreshControls from './RefreshControls.vue';
import { useAutoRefresh } from './useAutoRefresh';
import { useConfirm } from './useConfirm';
import { useReporter } from './report';

/**
 * Redis diagnostics (design: RedisDiagnostics): slow log, latency monitor
 * with the history of one event, and the client list with kill. Reads on
 * the profile's admin session; resets and kills ask first and are refused on
 * read-only connections.
 */
const props = defineProps<{
  /** The tab's Redis session. */
  session: SessionInfo;
  /** Connection name, for the confirmation dialogs. */
  connectionName: string;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
  /** The section to scroll to when the panel opens. */
  focus?: 'slowlog' | 'latency' | 'clients' | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const toasts = useToasts();
const confirm = useConfirm();
const readOnly = computed(() => props.session.readOnly);

const slowlog = shallowRef<SlowLog | null>(null);
const latency = shallowRef<LatencyReport | null>(null);
const clients = shallowRef<ClientInfo[] | null>(null);
const info = shallowRef<RedisInfo | null>(null);
const errors = shallowRef<{ slowlog: CmdError | null; latency: CmdError | null; clients: CmdError | null }>({
  slowlog: null,
  latency: null,
  clients: null,
});
const loadError = shallowRef<CmdError | null>(null);
const historyEvent = ref<string | null>(null);
const history = shallowRef<LatencySample[]>([]);
const historyError = shallowRef<CmdError | null>(null);

function errorOf(result: PromiseSettledResult<unknown>): CmdError | null {
  return result.status === 'rejected' ? report(result.reason) : null;
}

async function fetchAll(): Promise<void> {
  const id = props.session.id;
  const [s, l, c, i] = await Promise.allSettled([redisSlowlog(id), redisLatency(id), redisClients(id), redisInfo(id)]);
  if (!refresh.alive()) return;
  if (s.status === 'fulfilled') slowlog.value = s.value;
  if (l.status === 'fulfilled') latency.value = l.value;
  if (c.status === 'fulfilled') clients.value = c.value;
  if (i.status === 'fulfilled') info.value = i.value;
  errors.value = { slowlog: errorOf(s), latency: errorOf(l), clients: errorOf(c) };
  // A refused session (closed, reconnect needed) fails every call alike: show it once at the top.
  const all = [s, l, c].every((r) => r.status === 'rejected');
  loadError.value = all ? errorOf(s) : null;
  const event = historyEvent.value ?? latency.value?.events[0]?.event ?? null;
  if (event) await loadHistory(event);
  else history.value = [];
}

async function loadHistory(event: string): Promise<void> {
  historyEvent.value = event;
  try {
    const samples = await redisLatencyHistory(props.session.id, event);
    if (historyEvent.value !== event) return;
    history.value = samples;
    historyError.value = null;
  } catch (e: unknown) {
    if (historyEvent.value === event) historyError.value = report(e);
  }
}

const sections = { slowlog: ref<HTMLElement | null>(null), latency: ref<HTMLElement | null>(null), clients: ref<HTMLElement | null>(null) };
const slowlogSection = sections.slowlog;
const latencySection = sections.latency;
const clientsSection = sections.clients;
onMounted(async () => {
  await nextTick();
  if (props.focus) sections[props.focus].value?.scrollIntoView({ block: 'start' });
});

const refresh = useAutoRefresh(fetchAll, 5000, (e) => {
  loadError.value = report(e);
});

const CHOICES = [
  { label: 'Off', ms: null },
  { label: '5 s', ms: 5000 },
  { label: '15 s', ms: 15_000 },
  { label: '60 s', ms: 60_000 },
] as const;

// Slow log

function slowCommand(args: readonly Value[]): string {
  return args.map((a) => cellText(a)).join(' ');
}

function micros(us: number): string {
  if (us < 1000) return `${us} µs`;
  return `${(us / 1000).toFixed(us < 10_000 ? 2 : 1)} ms`;
}

async function resetSlowlog(): Promise<void> {
  const yes = await confirm.ask({
    title: `Reset the slow log of ${props.connectionName}?`,
    message: 'Every entry is removed from the server for every user. This cannot be undone.',
    confirmLabel: 'Reset log',
    statement: 'SLOWLOG RESET',
  });
  if (!yes) return;
  try {
    await redisSlowlogReset(props.session.id);
    toasts.push('success', 'Slow log reset');
  } catch (e: unknown) {
    toasts.pushError(report(e), 'Could not reset the slow log');
  }
  await refresh.refreshNow();
}

// Latency

async function resetLatency(): Promise<void> {
  const yes = await confirm.ask({
    title: `Reset the latency monitor of ${props.connectionName}?`,
    message: 'The recorded spikes of every event are removed from the server. This cannot be undone.',
    confirmLabel: 'Reset',
    statement: 'LATENCY RESET',
  });
  if (!yes) return;
  try {
    const n = await redisLatencyReset(props.session.id);
    toasts.push('success', 'Latency monitor reset', `${n} ${n === 1 ? 'event' : 'events'} cleared.`);
  } catch (e: unknown) {
    toasts.pushError(report(e), 'Could not reset the latency monitor');
  }
  historyEvent.value = null;
  await refresh.refreshNow();
}

const chart = computed(() => {
  const samples = [...history.value].sort((a, b) => a.time - b.time);
  if (samples.length === 0) return null;
  const max = Math.max(...samples.map((s) => s.ms), 1);
  const first = samples[0]?.time ?? 0;
  const last = samples[samples.length - 1]?.time ?? first;
  const span = Math.max(last - first, 1);
  const peak = samples.reduce((a, b) => (b.ms > a.ms ? b : a));
  const latest = samples[samples.length - 1] ?? peak;
  return {
    max,
    first,
    last,
    bars: samples.map((s) => ({ key: `${s.time}-${s.ms}`, x: samples.length === 1 ? 50 : ((s.time - first) / span) * 100, h: (s.ms / max) * 100, s })),
    aria: `Latency spikes for ${historyEvent.value ?? ''}: highest ${peak.ms} ms at ${formatTime(peak.time * 1000)}, latest ${latest.ms} ms at ${formatTime(latest.time * 1000)}`,
  };
});

// Clients

const clientText = ref('');
const clientKind = ref<ClientType | ''>('');
const idleOnly = ref(false);
const visibleClients = computed(() =>
  filterClients(clients.value ?? [], { text: clientText.value, type: clientKind.value, idleOverSeconds: idleOnly.value ? 600 : 0 }),
);

async function killClient(c: ClientInfo): Promise<void> {
  const yes = await confirm.ask({
    title: `Kill client ${c.id} on ${props.connectionName}?`,
    message: 'The client connection is closed. A client that reconnects on its own comes back with a new id.',
    confirmLabel: `Kill ${c.id}`,
    statement: `CLIENT KILL ID ${c.id}`,
    details: [[c.addr, c.name, c.user ? `user ${c.user}` : null, c.cmd ? `last command ${c.cmd}` : null].filter(Boolean).join(' · ')],
    typeToConfirm: props.session.environment === 'production' ? String(c.id) : '',
  });
  if (!yes) return;
  try {
    await redisClientKill(props.session.id, c.id);
    toasts.push('success', `Client ${c.id} killed`);
  } catch (e: unknown) {
    toasts.pushError(report(e), `Could not kill client ${c.id}`);
  }
  await refresh.refreshNow();
}

function seconds(n: number | null): string {
  return n === null ? '' : formatSeconds(n);
}
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Diagnostics</h1>
      <span class="grow" />
      <RefreshControls
        :state="refresh.state.value"
        :choices="CHOICES"
        @interval="refresh.setInterval"
        @pause="refresh.pause"
        @resume="refresh.resume"
        @refresh="refresh.refreshNow"
      />
    </div>
    <LiveError
      v-if="loadError"
      :error="loadError"
      class="panel-error"
      @reconnect="emit('reconnect')"
    />

    <div class="scroll body">
      <div class="top">
        <section
          ref="slowlogSection"
          class="box"
          aria-label="Slow log"
        >
          <div class="section-head">
            <h2>Slow log</h2>
            <span
              v-if="slowlog"
              class="note"
            >{{ formatCount(slowlog.length) }} entries<template v-if="slowlog.thresholdUs !== null"> · logged when slower than <span class="mono">{{ micros(slowlog.thresholdUs) }}</span></template><template v-if="slowlog.maxLen !== null"> · max {{ formatCount(slowlog.maxLen) }}</template></span>
            <span class="grow" />
            <button
              type="button"
              class="btn compact"
              :disabled="readOnly || !slowlog || slowlog.length === 0"
              :title="readOnly ? 'Read-only connection' : ''"
              @click="resetSlowlog"
            >
              Reset log
            </button>
          </div>
          <LiveError
            v-if="errors.slowlog && !loadError"
            :error="errors.slowlog"
            class="inner-error"
            @reconnect="emit('reconnect')"
          />
          <div class="table-wrap">
            <table
              class="grid"
              aria-label="Slow log entries"
            >
              <thead>
                <tr>
                  <th>ID</th><th>Time</th><th class="num">
                    Duration
                  </th><th>Command</th><th>Client</th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="e in slowlog?.entries ?? []"
                  :key="e.id"
                >
                  <td class="mono">
                    {{ e.id }}
                  </td>
                  <td class="mono">
                    {{ formatTime(e.time * 1000) }}
                  </td>
                  <td class="mono num">
                    {{ micros(e.durationUs) }}
                  </td>
                  <td
                    class="mono wrap"
                    :title="slowCommand(e.args)"
                  >
                    {{ slowCommand(e.args) }}
                  </td>
                  <td class="mono">
                    {{ [e.clientAddr, e.clientName].filter(Boolean).join(' ') }}
                  </td>
                </tr>
              </tbody>
            </table>
            <div
              v-if="slowlog && slowlog.entries.length === 0"
              class="empty"
            >
              The slow log is empty.
            </div>
          </div>
        </section>

        <section
          ref="latencySection"
          class="box"
          aria-label="Latency monitor"
        >
          <div class="section-head">
            <h2>Latency monitor</h2>
            <span
              v-if="latency"
              class="note"
            >
              <template v-if="latency.thresholdMs === null">threshold unknown (no CONFIG permission)</template>
              <template v-else-if="latency.thresholdMs === 0">off: set latency-monitor-threshold to record spikes</template>
              <template v-else>threshold <span class="mono">{{ latency.thresholdMs }} ms</span></template>
            </span>
            <span class="grow" />
            <button
              type="button"
              class="btn compact"
              :disabled="readOnly || !latency || latency.events.length === 0"
              @click="resetLatency"
            >
              Reset
            </button>
          </div>
          <LiveError
            v-if="errors.latency && !loadError"
            :error="errors.latency"
            class="inner-error"
            @reconnect="emit('reconnect')"
          />
          <table
            class="grid"
            aria-label="Latency events"
          >
            <thead>
              <tr>
                <th>Event</th><th>Latest at</th><th class="num">
                  Latest
                </th><th class="num">
                  Max
                </th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="l in latency?.events ?? []"
                :key="l.event"
                class="selectable"
                :class="{ selected: historyEvent === l.event }"
                tabindex="0"
                :aria-selected="historyEvent === l.event"
                @click="loadHistory(l.event)"
                @keydown.enter.prevent="loadHistory(l.event)"
              >
                <td class="mono">
                  {{ l.event }}
                </td>
                <td class="mono">
                  {{ formatTime(l.time * 1000) }}
                </td>
                <td class="mono num">
                  {{ l.latestMs }} ms
                </td>
                <td class="mono num">
                  {{ l.maxMs }} ms
                </td>
              </tr>
            </tbody>
          </table>
          <div
            v-if="latency && latency.events.length === 0"
            class="empty"
          >
            No latency spikes recorded.
          </div>
          <div
            v-if="historyEvent"
            class="history"
          >
            <div class="history-head">
              <span>History for <span class="mono">{{ historyEvent }}</span></span>
              <span class="note mono">LATENCY HISTORY</span>
            </div>
            <LiveError
              v-if="historyError"
              :error="historyError"
              @reconnect="emit('reconnect')"
            />
            <div
              v-else-if="chart"
              class="chart"
            >
              <div class="y mono">
                <span>{{ chart.max }}</span><span>{{ Math.round(chart.max / 2) }}</span><span>0 ms</span>
              </div>
              <div
                class="plot"
                role="img"
                :aria-label="chart.aria"
              >
                <span
                  v-for="b in chart.bars"
                  :key="b.key"
                  class="spike"
                  :style="{ left: `${b.x}%`, height: `${Math.max(b.h, 2)}%` }"
                  :title="`${b.s.ms} ms at ${formatTime(b.s.time * 1000)}`"
                />
              </div>
              <div class="x mono">
                <span>{{ formatTime(chart.first * 1000) }}</span><span>{{ formatTime(chart.last * 1000) }}</span>
              </div>
            </div>
            <p
              v-else
              class="note"
            >
              No samples for this event.
            </p>
          </div>
        </section>
      </div>

      <section
        ref="clientsSection"
        class="box"
        aria-label="Client list"
      >
        <div class="section-head">
          <h2>Clients</h2>
          <span
            v-if="info"
            class="note"
          >{{ formatCount(info.clients.connectedClients ?? clients?.length ?? 0) }} connected<template v-if="info.clients.blockedClients"> · {{ info.clients.blockedClients }} blocked</template><template v-if="info.clients.maxclients !== null"> · maxclients {{ formatCount(info.clients.maxclients) }}</template></span>
          <span class="grow" />
          <label class="search">
            <Icon name="search" />
            <input
              v-model="clientText"
              aria-label="Filter clients"
              placeholder="Address, name or user"
            >
          </label>
          <label class="check">Type
            <select
              v-model="clientKind"
              class="select small-select"
            >
              <option value="">All</option>
              <option value="normal">normal</option>
              <option value="pubsub">pubsub</option>
              <option value="replica">replica</option>
              <option value="master">master</option>
              <option value="monitor">monitor</option>
            </select>
          </label>
          <label class="check"><input
            v-model="idleOnly"
            type="checkbox"
          >Idle over 10 min only</label>
        </div>
        <LiveError
          v-if="errors.clients && !loadError"
          :error="errors.clients"
          class="inner-error"
          @reconnect="emit('reconnect')"
        />
        <div class="table-wrap clients">
          <table
            class="grid"
            aria-label="Connected clients"
          >
            <thead>
              <tr>
                <th>ID</th><th>Address</th><th>Name</th><th>User</th><th class="num">
                  DB
                </th><th class="num">
                  Age
                </th><th class="num">
                  Idle
                </th><th>Flags</th><th>Last command</th><th class="num">
                  Memory
                </th><th><span class="visually-hidden">Actions</span></th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="c in visibleClients"
                :key="c.id"
                :class="{ own: c.isSelf }"
              >
                <td class="mono">
                  {{ c.id }}
                </td>
                <td class="mono">
                  {{ c.addr ?? '' }}
                </td>
                <td>{{ c.name ?? '' }}</td>
                <td>{{ c.user ?? '' }}</td>
                <td class="mono num">
                  {{ c.db ?? '' }}
                </td>
                <td class="mono num">
                  {{ seconds(c.age) }}
                </td>
                <td class="mono num">
                  {{ seconds(c.idle) }}
                </td>
                <td
                  class="mono"
                  :title="c.flagNames.join(', ')"
                >
                  {{ c.flags }} <span class="note">{{ clientType(c) }}</span>
                </td>
                <td class="mono">
                  {{ c.cmd ?? '' }}
                </td>
                <td class="mono num">
                  {{ c.totMem === null ? '' : formatBytes(c.totMem) }}
                </td>
                <td>
                  <span
                    v-if="c.isSelf"
                    class="note"
                  >this panel</span>
                  <button
                    v-else
                    type="button"
                    class="btn compact"
                    :disabled="readOnly"
                    :aria-label="`Kill client ${c.id}`"
                    @click="killClient(c)"
                  >
                    Kill
                  </button>
                </td>
              </tr>
            </tbody>
          </table>
          <div
            v-if="clients && visibleClients.length === 0"
            class="empty"
          >
            No clients match the filters.
          </div>
        </div>
      </section>
    </div>

    <LiveConfirm
      v-if="confirm.pending.value"
      :title="confirm.pending.value.title"
      :message="confirm.pending.value.message"
      :confirm-label="confirm.pending.value.confirmLabel"
      :statement="confirm.pending.value.statement ?? ''"
      :details="confirm.pending.value.details ?? []"
      :type-to-confirm="confirm.pending.value.typeToConfirm ?? ''"
      :danger="confirm.pending.value.danger ?? true"
      :environment="session.environment"
      @done="confirm.answer"
    />
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
  gap: 16px;
  padding: 16px;
  background: var(--ground);
}

.top {
  display: grid;
  grid-template-columns: minmax(0, 3fr) minmax(0, 2fr);
  gap: 16px;
}

.box {
  display: flex;
  flex-direction: column;
  min-width: 0;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
  overflow: hidden;
}

.box .section-head {
  flex-wrap: wrap;
  padding: 10px 12px;
}

.inner-error {
  margin: 0 12px 8px;
}

.table-wrap {
  max-height: 320px;
  overflow: auto;
}

.table-wrap.clients {
  max-height: 420px;
}

.history {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 10px 12px 12px;
  border-top: 1px solid var(--border-inner);
  font-size: 12px;
}

.history-head {
  display: flex;
  justify-content: space-between;
}

.chart {
  display: grid;
  grid-template-columns: 40px minmax(0, 1fr);
  grid-template-rows: 120px auto;
  gap: 4px 6px;
}

.y {
  display: flex;
  flex-direction: column;
  justify-content: space-between;
  color: var(--text-subtle);
  font-size: 10px;
  text-align: right;
}

.plot {
  position: relative;
  border-bottom: 1px solid var(--border);
  border-left: 1px solid var(--border);
  background: linear-gradient(var(--border-inner) 1px, transparent 1px) 0 0 / 100% 50%;
}

.spike {
  position: absolute;
  bottom: 0;
  width: 4px;
  margin-left: -2px;
  border-radius: 2px 2px 0 0;
  background: var(--warning);
}

.x {
  display: flex;
  grid-column: 2;
  justify-content: space-between;
  color: var(--text-subtle);
  font-size: 10px;
}

.note {
  margin: 0;
}

@media (max-width: 900px) {
  .top {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
