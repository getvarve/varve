<script setup lang="ts">
import { computed, ref, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { killProcess, processList } from '../../api/live';
import type { KillTarget, Process, ProcessList } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { useToasts } from '../../composables/useToasts';
import {
  PG_STATES,
  blockingChains,
  filterProcesses,
  formatAgeMs,
  isLongRunning,
  isRunning,
  pgStateOf,
  sortProcesses,
} from '../../lib/live';
import type { PgStateFilter, ProcessSort } from '../../lib/live';
import { formatBytes, formatCount } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import LiveConfirm from './LiveConfirm.vue';
import LiveError from './LiveError.vue';
import RefreshControls from './RefreshControls.vue';
import { useAutoRefresh } from './useAutoRefresh';
import { useConfirm } from './useConfirm';
import { useReporter } from './report';

/**
 * Process list (MySQL, MariaDB; design: ProcessList) and server activity
 * (PostgreSQL `pg_stat_activity`; design: PgActivity) with blocking chains,
 * cancel and terminate. Reads on the profile's admin session; the tab's own
 * user session shows up in the list and can be stopped from here.
 */
const props = defineProps<{
  /** The tab's user session (PostgreSQL, MySQL or MariaDB). */
  session: SessionInfo;
  /** Connection name, for the confirmation dialogs. */
  connectionName: string;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const engine = computed(() => props.session.engine);
const isPg = computed(() => engine.value === 'postgres');
const toasts = useToasts();
const confirm = useConfirm();

const list = shallowRef<ProcessList | null>(null);
const error = shallowRef<CmdError | null>(null);

const refresh = useAutoRefresh(
  async () => {
    const result = await processList(props.session.id);
    if (!refresh.alive()) return;
    list.value = result;
    error.value = null;
  },
  5000,
  (e) => {
    error.value = report(e);
  },
);

// Filters

const text = ref('');
const hideIdle = ref(false);
const showBackground = ref(false);
const includeOwn = ref(true);
const longSeconds = ref(300);
const states = ref<Set<PgStateFilter>>(new Set());
const sortKey = ref<ProcessSort>('duration');
const descending = ref(true);

const all = computed(() => list.value?.processes ?? []);
const thresholdMs = computed(() => Math.max(0, Number(longSeconds.value) || 0) * 1000);

const visible = computed(() =>
  sortProcesses(
    filterProcesses(all.value, engine.value, {
      text: text.value,
      hideIdle: hideIdle.value,
      showBackground: showBackground.value,
      includeOwn: includeOwn.value,
      states: states.value,
    }),
    sortKey.value,
    descending.value,
  ),
);

const stateCounts = computed(() => {
  const counts = new Map<PgStateFilter, number>();
  for (const p of all.value) counts.set(pgStateOf(p), (counts.get(pgStateOf(p)) ?? 0) + 1);
  return PG_STATES.map((s) => ({ state: s, count: counts.get(s) ?? 0 })).filter((c) => c.count > 0 || c.state !== 'other');
});

function toggleState(state: PgStateFilter): void {
  const next = new Set(states.value);
  if (next.has(state)) next.delete(state);
  else next.add(state);
  states.value = next;
}

function sortBy(key: ProcessSort): void {
  if (sortKey.value === key) descending.value = !descending.value;
  else {
    sortKey.value = key;
    descending.value = key === 'duration';
  }
}

function ariaSort(key: ProcessSort): 'ascending' | 'descending' | 'none' {
  if (sortKey.value !== key) return 'none';
  return descending.value ? 'descending' : 'ascending';
}

const summary = computed(() => {
  const running = all.value.filter((p) => isRunning(p, engine.value)).length;
  const long = all.value.filter((p) => isLongRunning(p, engine.value, thresholdMs.value)).length;
  const noun = isPg.value ? 'sessions' : 'threads';
  return `${visible.value.length} of ${all.value.length} ${noun} · ${running} running · ${long} over ${longSeconds.value} s`;
});

// Blocking chains

const chains = computed(() => blockingChains(all.value));

// Selection

const selectedId = ref<number | null>(null);
const selected = computed<Process | null>(() => all.value.find((p) => p.id === selectedId.value) ?? null);
const selectedGone = computed(() => selectedId.value !== null && list.value !== null && selected.value === null);

function select(p: Process): void {
  selectedId.value = selectedId.value === p.id ? null : p.id;
}

function onRowKey(event: KeyboardEvent, p: Process): void {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    select(p);
  }
}

function client(p: Process): string {
  const host = p.clientHostname ?? p.clientAddress ?? '';
  if (isPg.value && p.clientPort !== null && p.clientPort >= 0 && host) return `${host}:${p.clientPort}`;
  if (isPg.value && p.clientPort === -1) return 'local socket';
  return host;
}

function stateText(p: Process): string {
  if (isPg.value) {
    const wait = p.waitEventType ? ` · ${p.waitEventType}${p.waitEvent ? `: ${p.waitEvent}` : ''}` : '';
    return `${p.state ?? p.backendType ?? ''}${wait}`;
  }
  return p.state ?? '';
}

function stateTone(p: Process): string {
  const s = p.state ?? '';
  if (s === 'active') return 'tone-success';
  if (s.startsWith('idle in transaction')) return 'tone-warning';
  if (p.blockedBy.length > 0) return 'tone-danger';
  return 'tone-neutral';
}

function oneLine(text: string | null): string {
  return (text ?? '').replace(/\s+/g, ' ').trim();
}

// Cancel and terminate

const readOnly = computed(() => props.session.readOnly);
const busy = ref(false);

function statementFor(id: number, target: KillTarget): string {
  if (isPg.value) return target === 'query' ? `SELECT pg_cancel_backend(${id})` : `SELECT pg_terminate_backend(${id})`;
  return target === 'query' ? `KILL QUERY ${id}` : `KILL ${id}`;
}

function actionLabel(target: KillTarget): string {
  if (isPg.value) return target === 'query' ? 'Cancel query' : 'Terminate backend';
  return target === 'query' ? 'Kill query' : 'Kill connection';
}

async function kill(p: Process, target: KillTarget): Promise<void> {
  if (busy.value) return;
  const what = isPg.value ? `backend ${p.id}` : `connection ${p.id}`;
  const effect =
    target === 'query'
      ? 'The running statement stops with an error; the connection stays open.'
      : 'The connection closes and its open transaction is rolled back. The client sees a dropped connection.';
  const yes = await confirm.ask({
    title: `${actionLabel(target)} ${p.id} on ${props.connectionName}?`,
    message: `${effect} It runs on a side connection of this tab.`,
    confirmLabel: target === 'query' ? actionLabel('query') : `${isPg.value ? 'Terminate' : 'Kill'} ${p.id}`,
    statement: statementFor(p.id, target),
    details: p.query ? [`${what}: ${oneLine(p.query).slice(0, 200)}`] : [],
    typeToConfirm: props.session.environment === 'production' ? String(p.id) : '',
    danger: true,
  });
  if (!yes) return;
  busy.value = true;
  try {
    const outcome = await killProcess(props.session.id, p.id, target);
    if (outcome.signalled) toasts.push('success', `${actionLabel(target)}: ${p.id}`, outcome.statement);
    else toasts.push('warning', `Nothing to stop for ${p.id}`, outcome.message ?? 'The process has ended or cannot be signalled.');
  } catch (e: unknown) {
    toasts.pushError(report(e), `Could not ${actionLabel(target).toLowerCase()} ${p.id}`);
  } finally {
    busy.value = false;
  }
  await refresh.refreshNow();
}

const killDisabledReason = computed(() => {
  if (readOnly.value) return 'This connection is read-only, so it cannot stop other sessions.';
  if (selected.value?.own) return "This is the panel's own side connection.";
  return '';
});
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>{{ isPg ? 'Server activity' : 'Process list' }}</h1>
      <span
        v-if="list"
        class="source"
      >{{ list.source }}</span>
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

    <div class="bar">
      <label class="search">
        <Icon name="search" />
        <input
          v-model="text"
          :placeholder="isPg ? 'Filter by user, app, client or query' : 'Filter by user, host, db or Info'"
          :aria-label="isPg ? 'Filter sessions' : 'Filter threads'"
        >
      </label>
      <template v-if="isPg">
        <span class="count">State</span>
        <button
          v-for="c in stateCounts"
          :key="c.state"
          type="button"
          class="chip"
          :aria-pressed="states.has(c.state)"
          @click="toggleState(c.state)"
        >
          {{ c.state }}<span class="n">{{ c.count }}</span>
        </button>
      </template>
      <label
        v-else
        class="check"
      ><input
        v-model="hideIdle"
        type="checkbox"
      >Hide Sleep</label>
      <label class="check"><input
        v-model="showBackground"
        type="checkbox"
      >{{ isPg ? 'Show background workers' : 'Show system threads' }}</label>
      <label class="check"><input
        v-model="includeOwn"
        type="checkbox"
      >Include this panel's connection</label>
      <label class="check">Highlight longer than
        <input
          v-model.number="longSeconds"
          class="input small-input mono"
          type="number"
          min="0"
          aria-label="Long running threshold in seconds"
        >s</label>
      <span class="grow" />
      <span class="count">{{ summary }}</span>
    </div>

    <LiveError
      v-if="error"
      :error="error"
      class="panel-error"
      @reconnect="emit('reconnect')"
    />

    <div class="scroll">
      <table
        class="grid"
        :aria-label="isPg ? 'Sessions' : 'Threads'"
      >
        <thead>
          <tr>
            <th :aria-sort="ariaSort('id')">
              <button
                type="button"
                @click="sortBy('id')"
              >
                {{ isPg ? 'pid' : 'Id' }}
              </button>
            </th>
            <th :aria-sort="ariaSort('user')">
              <button
                type="button"
                @click="sortBy('user')"
              >
                User
              </button>
            </th>
            <th v-if="isPg">
              Application
            </th>
            <th>{{ isPg ? 'Client' : 'Host' }}</th>
            <th>{{ isPg ? 'Database' : 'db' }}</th>
            <th v-if="!isPg">
              Command
            </th>
            <th :aria-sort="ariaSort('duration')">
              <button
                type="button"
                @click="sortBy('duration')"
              >
                {{ isPg ? 'Duration' : 'Time' }}
              </button>
            </th>
            <th :aria-sort="ariaSort('state')">
              <button
                type="button"
                @click="sortBy('state')"
              >
                State
              </button>
            </th>
            <th>{{ isPg ? 'Query' : 'Info' }}</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="p in visible"
            :key="p.id"
            class="selectable"
            :class="{ selected: p.id === selectedId, long: isLongRunning(p, engine, thresholdMs), own: p.own }"
            tabindex="0"
            :aria-selected="p.id === selectedId"
            @click="select(p)"
            @keydown="onRowKey($event, p)"
          >
            <td class="mono">
              {{ p.id }}<span
                v-if="p.own"
                class="note"
              > (this panel)</span>
            </td>
            <td>{{ p.user ?? '' }}</td>
            <td v-if="isPg">
              {{ p.application ?? '' }}
            </td>
            <td class="mono">
              {{ client(p) }}
            </td>
            <td>{{ p.database ?? '' }}</td>
            <td v-if="!isPg">
              {{ p.command ?? '' }}
            </td>
            <td class="mono num">
              {{ formatAgeMs(p.durationMs) }}
            </td>
            <td>
              <span
                v-if="isPg"
                class="pill"
                :class="stateTone(p)"
              ><span class="dot" />{{ stateText(p) }}</span>
              <template v-else>
                {{ stateText(p) }}
              </template>
            </td>
            <td
              class="mono wrap"
              :title="p.query ?? ''"
            >
              {{ oneLine(p.query) }}
            </td>
          </tr>
        </tbody>
      </table>
      <div
        v-if="list && visible.length === 0"
        class="empty"
      >
        No {{ isPg ? 'sessions' : 'threads' }} match the filters.
      </div>
      <div
        v-else-if="!list && !error"
        class="empty"
      >
        Loading…
      </div>
    </div>

    <section
      v-if="chains.length > 0"
      class="section"
      aria-label="Blocking chains"
    >
      <div class="section-head">
        <Icon name="warning" />
        <h2>Blocking chains</h2>
        <span class="note">{{ chains.length }} {{ chains.length === 1 ? 'chain' : 'chains' }} · ending a root blocker releases its whole chain</span>
      </div>
      <ol class="chains">
        <li
          v-for="(chain, ci) in chains"
          :key="ci"
        >
          <button
            v-for="link in chain"
            :key="link.process.id"
            type="button"
            class="link"
            :style="{ paddingLeft: `${8 + link.depth * 20}px` }"
            @click="selectedId = link.process.id"
          >
            <span class="mono pid">{{ link.process.id }}</span>
            <span class="who">{{ link.process.user ?? '' }} · {{ link.process.application ?? link.process.database ?? '' }} ·
              <span :class="link.waitsFor === null ? 'warn' : 'bad'">{{ link.process.state ?? '' }} {{ formatAgeMs(link.process.durationMs) }}</span>
            </span>
            <span
              class="pill"
              :class="link.waitsFor === null ? 'tone-danger' : 'tone-neutral'"
            >{{ link.waitsFor === null ? 'root blocker' : `waits for ${link.waitsFor}` }}</span>
          </button>
        </li>
      </ol>
    </section>

    <section
      v-if="selected || selectedGone"
      class="section detail"
      :aria-label="isPg ? 'Selected session' : 'Selected thread'"
    >
      <template v-if="selected">
        <div class="section-head">
          <h2>{{ isPg ? 'pid' : 'Thread' }} {{ selected.id }}</h2>
          <span
            v-if="isLongRunning(selected, engine, thresholdMs)"
            class="pill tone-warning"
          ><span class="dot" />Long running · {{ formatAgeMs(selected.durationMs) }}</span>
          <span class="note">{{ selected.user ?? '' }}{{ selected.database ? ` · db ${selected.database}` : '' }}{{ client(selected) ? ` from ${client(selected)}` : '' }}</span>
          <span class="grow" />
          <button
            type="button"
            class="icon-btn"
            aria-label="Close details"
            @click="selectedId = null"
          >
            <Icon name="close" />
          </button>
        </div>
        <div class="detail-body">
          <pre
            class="code query"
            aria-label="Statement"
          >{{ selected.query || 'No statement' }}</pre>
          <div class="side">
            <dl class="facts">
              <template v-if="selected.backendType">
                <dt>Backend type</dt><dd>{{ selected.backendType }}</dd>
              </template>
              <template v-if="selected.waitEventType">
                <dt>Waiting for</dt><dd>{{ selected.waitEventType }}{{ selected.waitEvent ? `: ${selected.waitEvent}` : '' }}</dd>
              </template>
              <template v-if="selected.blockedBy.length > 0">
                <dt>Blocked by</dt><dd>{{ selected.blockedBy.join(', ') }}</dd>
              </template>
              <template v-if="selected.backendStart">
                <dt>Connected</dt><dd>{{ selected.backendStart }}</dd>
              </template>
              <template v-if="selected.xactStart">
                <dt>Transaction started</dt><dd>{{ selected.xactStart }}</dd>
              </template>
              <template v-if="selected.queryStart">
                <dt>Query started</dt><dd>{{ selected.queryStart }}</dd>
              </template>
              <template v-if="selected.rowsExamined !== null">
                <dt>Rows examined</dt><dd>{{ formatCount(selected.rowsExamined) }}</dd>
              </template>
              <template v-if="selected.memoryBytes !== null">
                <dt>Memory</dt><dd>{{ formatBytes(selected.memoryBytes) }}</dd>
              </template>
              <template v-if="selected.progress !== null && selected.progress > 0">
                <dt>Progress</dt><dd>{{ selected.progress.toFixed(1) }}%{{ selected.stage !== null && selected.maxStage !== null ? ` (stage ${selected.stage} of ${selected.maxStage})` : '' }}</dd>
              </template>
              <template v-if="selected.leaderId !== null">
                <dt>Parallel leader</dt><dd>{{ selected.leaderId }}</dd>
              </template>
              <template v-if="selected.queryId !== null">
                <dt>Query id</dt><dd>{{ selected.queryId }}</dd>
              </template>
            </dl>
            <div class="actions">
              <button
                type="button"
                class="btn compact"
                :disabled="busy || killDisabledReason !== ''"
                @click="kill(selected, 'query')"
              >
                <Icon name="stop" />{{ actionLabel('query') }} {{ selected.id }}
              </button>
              <button
                type="button"
                class="btn compact danger"
                :disabled="busy || killDisabledReason !== ''"
                @click="kill(selected, 'connection')"
              >
                {{ actionLabel('connection') }}
              </button>
            </div>
            <p class="note">
              <template v-if="killDisabledReason">
                {{ killDisabledReason }}
              </template>
              <template v-else>
                <span class="mono">{{ statementFor(selected.id, 'query') }}</span> stops the statement and keeps the connection.
                <span class="mono">{{ statementFor(selected.id, 'connection') }}</span> also closes the connection. Both run on a side connection.
              </template>
            </p>
          </div>
        </div>
      </template>
      <div
        v-else
        class="section-head"
      >
        <span class="note">{{ isPg ? 'Session' : 'Thread' }} {{ selectedId }} has ended.</span>
        <span class="grow" />
        <button
          type="button"
          class="icon-btn"
          aria-label="Close details"
          @click="selectedId = null"
        >
          <Icon name="close" />
        </button>
      </div>
    </section>

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

.chains {
  display: flex;
  flex-direction: column;
  gap: 8px;
  max-height: 200px;
  margin: 0;
  padding: 0 16px 12px;
  overflow: auto;
  list-style: none;
}

.chains li {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
}

.link {
  display: flex;
  align-items: center;
  gap: 10px;
  min-height: 32px;
  padding: 4px 8px;
  border: 0;
  border-bottom: 1px solid var(--border-inner);
  background: transparent;
  color: var(--text);
  font-size: 12px;
  text-align: left;
  cursor: pointer;
}

.link:last-child {
  border-bottom: 0;
}

.link:hover {
  background: var(--surface-raised);
}

.pid {
  min-width: 56px;
  font-weight: 500;
}

.who {
  flex: 1;
  min-width: 0;
  color: var(--text-muted);
}

.warn {
  color: var(--warning-text);
}

.bad {
  color: var(--danger-text);
}

.detail-body {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(260px, 380px);
  gap: 16px;
  max-height: 260px;
  padding: 0 16px 12px;
  overflow: auto;
}

.query {
  max-height: 220px;
  overflow: auto;
}

.side {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.actions {
  display: flex;
  gap: 8px;
}

.note {
  margin: 0;
}
</style>
