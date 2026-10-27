<script setup lang="ts">
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
import { computed, onBeforeUnmount, reactive, ref, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { DEFAULT_MEMORY_SCAN, redisInfo, redisMemoryAnalysis, redisMemoryAnalysisStop } from '../../api/live';
import type { MemoryAnalysis, RedisInfo } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import { useToasts } from '../../composables/useToasts';
import { formatTime } from '../../lib/display';
import { toCsv } from '../../lib/exportResult';
import { bigKeys, memoryOptionsFrom, prefixBytes } from '../../lib/live';
import type { MemoryForm } from '../../lib/live';
import { cellText, formatBytes, formatCount, formatDuration } from '../../lib/values';
import Icon from '../common/Icon.vue';
import LiveError from './LiveError.vue';
import RefreshControls from './RefreshControls.vue';
import { useAutoRefresh } from './useAutoRefresh';
import { useReporter } from './report';

/**
 * Redis memory (design: RedisMemory): `INFO memory` figures, and a sampled
 * analysis by key prefix and type with the biggest keys. The analysis runs
 * on the admin session for at most its time budget; the tab's own session
 * stays free.
 */
const props = defineProps<{
  /** The tab's Redis session. */
  session: SessionInfo;
  /** Used for the CSV file name. */
  connectionName: string;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const toasts = useToasts();
const info = shallowRef<RedisInfo | null>(null);
const infoError = shallowRef<CmdError | null>(null);

const refresh = useAutoRefresh(
  async () => {
    const result = await redisInfo(props.session.id);
    if (!refresh.alive()) return;
    info.value = result;
    infoError.value = null;
  },
  null,
  (e) => {
    infoError.value = report(e);
  },
);

const memory = computed(() => info.value?.memory ?? null);
const keyTotal = computed(() => (info.value?.keyspace ?? []).reduce((s, d) => s + d.keys, 0));

function b(n: number | null): string {
  return n === null ? 'n/a' : formatBytes(n);
}

const kpis = computed(() => {
  const m = memory.value;
  if (!m) return [];
  const limit = m.maxmemory ? `of ${formatBytes(m.maxmemory)} maxmemory` : 'no maxmemory limit';
  return [
    { label: 'Used memory', value: b(m.usedMemory), note: limit },
    { label: 'Dataset', value: b(m.usedMemoryDataset), note: m.usedMemoryOverhead === null ? '' : `overhead ${formatBytes(m.usedMemoryOverhead)}` },
    { label: 'Peak', value: b(m.usedMemoryPeak), note: m.usedMemoryRss === null ? '' : `RSS ${formatBytes(m.usedMemoryRss)}` },
    {
      label: 'Fragmentation',
      value: m.memFragmentationRatio === null ? 'n/a' : m.memFragmentationRatio.toFixed(2),
      note: m.memAllocator ?? '',
    },
    { label: 'Eviction policy', value: m.maxmemoryPolicy ?? 'n/a', note: `${formatCount(keyTotal.value)} keys` },
  ];
});

const usedShare = computed(() => {
  const m = memory.value;
  if (!m?.maxmemory || m.usedMemory === null) return null;
  return Math.min(1, m.usedMemory / m.maxmemory);
});

// Analysis

const form = reactive<MemoryForm>({
  pattern: '*',
  delimiter: DEFAULT_MEMORY_SCAN.delimiter,
  depth: DEFAULT_MEMORY_SCAN.depth,
  maxKeys: DEFAULT_MEMORY_SCAN.maxKeys,
  timeBudgetSeconds: DEFAULT_MEMORY_SCAN.timeBudgetMs / 1000,
  samples: DEFAULT_MEMORY_SCAN.samples,
});
const checked = computed(() => memoryOptionsFrom(form, DEFAULT_MEMORY_SCAN));
const analysis = shallowRef<MemoryAnalysis | null>(null);
const analysisError = shallowRef<CmdError | null>(null);
const running = ref(false);
/** The id of the running analysis, for Stop. */
const runId = ref<string | null>(null);
const stopping = ref(false);
const startedAt = ref(0);
const now = ref(Date.now());
const budgetMs = ref(0);
const finishedAt = ref<number | null>(null);
let ticker: ReturnType<typeof setInterval> | null = null;
let alive = true;

onBeforeUnmount(() => {
  alive = false;
  if (ticker !== null) clearInterval(ticker);
  // Nobody reads the result any more: free the admin session at once.
  if (runId.value !== null) void redisMemoryAnalysisStop(props.session.id, runId.value).catch(() => undefined);
});

/** Stops the running analysis; it returns what it measured so far, marked partial. */
async function stopAnalysis(): Promise<void> {
  const id = runId.value;
  if (!running.value || id === null || stopping.value) return;
  stopping.value = true;
  try {
    await redisMemoryAnalysisStop(props.session.id, id);
  } catch (e: unknown) {
    stopping.value = false;
    if (alive) analysisError.value = report(e);
  }
}

async function runAnalysis(): Promise<void> {
  const result = checked.value;
  if (running.value || !result.options) return;
  running.value = true;
  stopping.value = false;
  const id = newRunId();
  runId.value = id;
  analysisError.value = null;
  startedAt.value = Date.now();
  now.value = startedAt.value;
  budgetMs.value = result.options.timeBudgetMs;
  ticker = setInterval(() => {
    now.value = Date.now();
  }, 250);
  try {
    const out = await redisMemoryAnalysis(props.session.id, result.options, id);
    if (!alive) return;
    analysis.value = out;
    finishedAt.value = Date.now();
  } catch (e: unknown) {
    if (alive) analysisError.value = report(e);
  } finally {
    if (ticker !== null) clearInterval(ticker);
    ticker = null;
    running.value = false;
    stopping.value = false;
    runId.value = null;
  }
}

function newRunId(): string {
  return typeof crypto !== 'undefined' && 'randomUUID' in crypto ? crypto.randomUUID() : `memory-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

const elapsedMs = computed(() => Math.max(0, now.value - startedAt.value));
const progress = computed(() => (budgetMs.value > 0 ? Math.min(99, Math.round((elapsedMs.value / budgetMs.value) * 100)) : 0));

const STOP_TEXT = {
  complete: 'the whole database was scanned',
  keyBudget: 'the key budget was used up',
  timeBudget: 'the time budget was used up',
  stopped: 'stopped by you, so this is a partial result',
} as const;

const summary = computed(() => {
  const a = analysis.value;
  if (!a) return '';
  const of = a.dbSize > 0 ? ` of ${formatCount(a.dbSize)}` : '';
  return `${formatCount(a.scannedKeys)}${of} keys measured in ${formatDuration(a.elapsedMs)}; ${STOP_TEXT[a.stoppedBy]}.`;
});

const prefixRows = computed(() => {
  const a = analysis.value;
  if (!a) return [];
  const rows = a.prefixes.map((p) => ({ key: p.prefix === null ? '(no delimiter)' : cellText(p.prefix), keys: p.keys, avg: p.avgBytes, ...prefixBytes(p) }));
  if (a.otherPrefixes) {
    const o = a.otherPrefixes;
    rows.push({ key: `${formatCount(o.prefixes)} other prefixes`, keys: o.keys, avg: o.keys > 0 ? Math.round(o.bytes / o.keys) : 0, ...prefixBytes(o) });
  }
  const max = Math.max(1, ...rows.map((r) => r.bytes));
  const total = rows.reduce((s, r) => s + r.bytes, 0) || 1;
  return rows.map((r) => ({ ...r, width: r.bytes / max, share: r.bytes / total }));
});

const estimateNote = computed(() => {
  const a = analysis.value;
  if (!a || a.complete) return '';
  const unknown = a.prefixes.some((p) => p.estimatedBytes === null);
  if (unknown) return 'Partial scan with a pattern: sizes are what was measured; the number of matching keys outside the sample is unknown.';
  return `Partial scan: sizes are scaled from ${formatCount(a.scannedKeys)} measured keys to ${formatCount(a.dbSize)} keys.`;
});

const bigType = ref('');
const typeNames = computed(() => (analysis.value?.types ?? []).map((t) => t.keyType));
const bigRows = computed(() => (analysis.value ? bigKeys(analysis.value, bigType.value) : []));

async function exportBigKeys(): Promise<void> {
  if (bigRows.value.length === 0) return;
  try {
    const path = await saveFileDialog({
      title: 'Export big keys as CSV',
      defaultPath: `${props.connectionName.replace(/[^\w.-]+/g, '_')}-big-keys.csv`,
      filters: [{ name: 'CSV', extensions: ['csv'] }],
    });
    if (!path) return;
    const columns = [
      { name: 'key', typeName: 'text' },
      { name: 'type', typeName: 'text' },
      { name: 'bytes', typeName: 'int' },
      { name: 'elements', typeName: 'int' },
    ];
    const rows = bigRows.value.map((k) => [
      k.key,
      { t: 'text' as const, v: k.keyType },
      { t: 'int' as const, v: k.bytes },
      k.length === null ? { t: 'null' as const } : { t: 'int' as const, v: k.length },
    ]);
    await writeTextFile(path, toCsv(columns, rows));
    toasts.push('success', `Exported ${formatCount(rows.length)} keys`, path);
  } catch (e: unknown) {
    toasts.pushError(report(e), 'Export failed');
  }
}
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Memory</h1>
      <span class="note">From <span class="mono">INFO memory</span><template v-if="refresh.state.value.lastAt"> at {{ formatTime(refresh.state.value.lastAt) }}</template></span>
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
      v-if="infoError"
      :error="infoError"
      class="panel-error"
      @reconnect="emit('reconnect')"
    />

    <div class="scroll body">
      <div
        v-if="kpis.length > 0"
        class="cards kpis"
      >
        <div
          v-for="k in kpis"
          :key="k.label"
          class="card"
        >
          <span class="k">{{ k.label }}</span>
          <span class="v">{{ k.value }}</span>
          <span class="s">{{ k.note }}</span>
        </div>
      </div>
      <span
        v-if="usedShare !== null"
        class="meter used"
        role="img"
        :aria-label="`${Math.round(usedShare * 100)}% of maxmemory in use`"
      ><span :style="{ width: `${usedShare * 100}%`, background: usedShare >= 0.9 ? 'var(--danger)' : usedShare >= 0.75 ? 'var(--warning)' : 'var(--accent)' }" /></span>

      <form
        class="box run"
        aria-label="Memory analysis"
        @submit.prevent="runAnalysis"
      >
        <div class="fields">
          <label class="field">Key pattern
            <input
              v-model="form.pattern"
              class="input mono"
              :disabled="running"
              spellcheck="false"
            >
          </label>
          <label class="field narrow">Delimiter
            <input
              v-model="form.delimiter"
              class="input mono"
              :disabled="running"
              spellcheck="false"
            >
          </label>
          <label class="field narrow">Prefix depth
            <input
              v-model="form.depth"
              class="input mono"
              type="number"
              min="1"
              :disabled="running"
            >
          </label>
          <label class="field">Most keys
            <input
              v-model="form.maxKeys"
              class="input mono"
              type="number"
              min="1"
              :disabled="running"
            >
          </label>
          <label class="field narrow">Time budget (s)
            <input
              v-model="form.timeBudgetSeconds"
              class="input mono"
              type="number"
              min="1"
              :disabled="running"
            >
          </label>
          <label class="field narrow">Samples per key
            <input
              v-model="form.samples"
              class="input mono"
              type="number"
              min="0"
              :disabled="running"
            >
          </label>
          <button
            type="submit"
            class="btn primary"
            :disabled="running || !checked.options"
          >
            {{ running ? 'Analysing…' : 'Run analysis' }}
          </button>
          <button
            v-if="running"
            type="button"
            class="btn"
            :disabled="stopping"
            @click="stopAnalysis"
          >
            {{ stopping ? 'Stopping…' : 'Stop' }}
          </button>
        </div>
        <ul
          v-if="checked.problems.length > 0"
          class="problems"
          role="alert"
        >
          <li
            v-for="p in checked.problems"
            :key="p"
          >
            {{ p }}
          </li>
        </ul>
        <p class="note">
          Scans keys with <span class="mono">SCAN</span> and measures each with <span class="mono">MEMORY USAGE</span> on a side connection, stopping at the key or time budget. Samples per key 0 measures every element of nested values (exact but slow).
        </p>
        <div
          v-if="running"
          class="progress"
        >
          <span>Sampling · {{ formatDuration(elapsedMs) }} elapsed of at most {{ formatDuration(budgetMs) }}</span>
          <div
            class="meter"
            role="progressbar"
            aria-label="Time budget used"
            :aria-valuenow="progress"
            aria-valuemin="0"
            aria-valuemax="100"
          >
            <span :style="{ width: `${progress}%` }" />
          </div>
        </div>
        <LiveError
          v-if="analysisError"
          :error="analysisError"
          @reconnect="emit('reconnect')"
        />
      </form>

      <template v-if="analysis">
        <p class="note">
          {{ summary }}<template v-if="finishedAt">
            Finished {{ formatTime(finishedAt) }}.
          </template> Total measured {{ formatBytes(analysis.totalBytes) }}.
        </p>
        <div class="results">
          <section
            class="box"
            aria-label="Memory by key prefix"
          >
            <div class="section-head">
              <h2>Memory by key prefix</h2>
              <span class="note">split on <span class="mono">{{ form.delimiter }}</span></span>
            </div>
            <div
              v-if="estimateNote"
              class="notice tone-info estimate"
            >
              <Icon name="info" />{{ estimateNote }}
            </div>
            <div
              class="prefixes"
              role="list"
            >
              <div
                v-for="r in prefixRows"
                :key="r.key"
                class="prefix"
                role="listitem"
              >
                <div class="prefix-line">
                  <span
                    class="mono name"
                    :title="r.key"
                  >{{ r.key }}</span>
                  <span class="note">{{ formatCount(r.keys) }} keys · avg {{ formatBytes(r.avg) }}</span>
                  <span class="grow" />
                  <span class="mono">{{ r.estimated ? '~' : '' }}{{ formatBytes(r.bytes) }}</span>
                  <span class="mono pct">{{ Math.round(r.share * 100) }}%</span>
                </div>
                <span
                  class="meter"
                  role="img"
                  :aria-label="`${r.key}: ${formatBytes(r.bytes)}, ${Math.round(r.share * 100)}% of the measured memory`"
                ><span :style="{ width: `${r.width * 100}%` }" /></span>
              </div>
              <p
                v-if="prefixRows.length === 0"
                class="empty"
              >
                No keys matched.
              </p>
            </div>
            <div class="types">
              <span class="label">By type</span>
              <div
                v-for="t in analysis.types"
                :key="t.keyType"
                class="type-line"
              >
                <span class="mono">{{ t.keyType }}</span>
                <span class="note">{{ formatCount(t.keys) }} keys</span>
                <span class="grow" />
                <span class="mono">{{ formatBytes(t.bytes) }}</span>
              </div>
            </div>
          </section>

          <section
            class="box"
            aria-label="Big keys"
          >
            <div class="section-head">
              <h2>Big keys</h2>
              <span class="note">{{ analysis.complete ? 'largest in the database' : 'largest in the sample' }}</span>
              <span class="grow" />
              <label class="check">Type
                <select
                  v-model="bigType"
                  class="select small-select"
                >
                  <option value="">All types</option>
                  <option
                    v-for="t in typeNames"
                    :key="t"
                    :value="t"
                  >{{ t }}</option>
                </select>
              </label>
              <button
                type="button"
                class="btn compact"
                :disabled="bigRows.length === 0"
                @click="exportBigKeys"
              >
                Export CSV
              </button>
            </div>
            <div class="table-wrap">
              <table
                class="grid"
                aria-label="Big keys"
              >
                <thead>
                  <tr>
                    <th>Key</th><th>Type</th><th class="num">
                      Size
                    </th><th class="num">
                      Elements
                    </th>
                  </tr>
                </thead>
                <tbody>
                  <tr
                    v-for="(k, i) in bigRows"
                    :key="i"
                  >
                    <td
                      class="mono wrap"
                      :title="cellText(k.key)"
                    >
                      {{ cellText(k.key) }}
                    </td>
                    <td class="mono">
                      {{ k.keyType }}
                    </td>
                    <td class="mono num">
                      {{ formatBytes(k.bytes) }}
                    </td>
                    <td class="mono num">
                      {{ k.length === null ? '' : formatCount(k.length) }}
                    </td>
                  </tr>
                </tbody>
              </table>
            </div>
            <p
              v-if="!analysis.complete"
              class="note foot"
            >
              Keys outside the sample can be larger. Raise the key and time budget for a complete report.
            </p>
          </section>
        </div>
      </template>
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

.kpis {
  padding: 0;
}

.used {
  height: 8px;
}

.box {
  display: flex;
  flex-direction: column;
  gap: 8px;
  min-width: 0;
  padding: 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

.box .section-head {
  padding: 0;
}

.fields {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-end;
  gap: 10px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  width: 200px;
  color: var(--text-muted);
  font-size: 12px;
}

.field.narrow {
  width: 110px;
}

.field .input {
  height: 28px;
  font-size: 12px;
}

.problems {
  margin: 0;
  padding-left: 18px;
  color: var(--danger-text);
  font-size: 12px;
}

.progress {
  display: flex;
  flex-direction: column;
  gap: 6px;
  font-size: 12px;
}

.results {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 12px;
}

.estimate {
  align-items: center;
}

.prefixes {
  display: flex;
  flex-direction: column;
  gap: 8px;
  max-height: 360px;
  overflow: auto;
}

.prefix {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
}

.prefix-line,
.type-line {
  display: flex;
  align-items: baseline;
  gap: 8px;
  font-size: 12px;
}

.name {
  max-width: 50%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.pct {
  width: 40px;
  color: var(--text-muted);
  text-align: right;
}

.types {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding-top: 8px;
  border-top: 1px solid var(--border-inner);
}

.table-wrap {
  max-height: 420px;
  overflow: auto;
}

.note {
  margin: 0;
  color: var(--text-subtle);
  font-size: 12px;
}

.foot {
  padding-top: 4px;
}

@media (max-width: 1000px) {
  .results {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
