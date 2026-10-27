<script setup lang="ts">
import { computed, onMounted, reactive, ref, shallowRef, watch } from 'vue';
import { presentError } from '../../api/errors';
import type { CmdError } from '../../api/errors';
import { explain, explainCapabilities } from '../../api/explain';
import type { ExplainCapabilities, ExplainFormat, ExplainOptions, Plan } from '../../api/explain';
import { cancel } from '../../api/sessions';
import type { SessionInfo } from '../../api/sessions';
import { useToasts } from '../../composables/useToasts';
import { HIGHLIGHT_LABELS, PG_BLOCK_BYTES, explainLabel, findNode, flattenPlan, formatMs, formatNumber, planOutline } from '../../lib/explainPlan';
import type { HighlightMode } from '../../lib/explainPlan';
import { presentExplainError, runExplain } from '../../lib/explainRun';
import type { ExplainConfirmation } from '../../lib/explainRun';
import { formatBytes, prettyJson } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import ExplainConfirmDialog from './ExplainConfirmDialog.vue';
import PlanNodeDetails from './PlanNodeDetails.vue';
import PlanTimeline from './PlanTimeline.vue';
import PlanTree from './PlanTree.vue';
import { useReporter } from '../live/report';

/**
 * EXPLAIN and EXPLAIN ANALYZE of one statement on the tab's own session
 * (design: ExplainPlan): options the server offers, the confirmation the
 * backend asks for (every reason listed), Cancel while it runs, and the plan
 * as a visual tree, a timeline, indented text or the server's raw output,
 * with node details and hot spots. A failure shows as the statement's own
 * error.
 */
const props = withDefaults(
  defineProps<{
    /** The tab's user session (PostgreSQL, MySQL or MariaDB). */
    session: SessionInfo;
    /** Connection name, for the confirmation dialog. */
    connectionName: string;
    /** The statement to explain (one statement). */
    statement: string;
    /** Run once on mount: `'explain'`, `'analyze'`, or `null` to wait for the user. */
    autoRun?: 'explain' | 'analyze' | null;
    /** The workspace tab the view runs in; its errors are reported to the tab. */
    tabId?: string | null;
  }>(),
  { autoRun: null, tabId: null },
);
const emit = defineEmits<{ running: [running: boolean] }>();
const report = useReporter(() => props.tabId);

const toasts = useToasts();

// Capabilities and options

const caps = shallowRef<ExplainCapabilities | null>(null);
const capsError = shallowRef<CmdError | null>(null);
const options = reactive({ buffers: true, verbose: false, settings: false, wal: false, format: null as ExplainFormat | null });
const showOptions = ref(false);

async function loadCaps(): Promise<void> {
  try {
    caps.value = await explainCapabilities(props.session.id);
    capsError.value = null;
  } catch (e: unknown) {
    capsError.value = report(e);
  }
}

/** The formats a mode offers; an option the server lacks is never sent. */
function formatsFor(analyze: boolean): ExplainFormat[] {
  const c = caps.value;
  if (!c) return [];
  return analyze ? c.analyzeFormats : c.formats;
}

function optionsFor(analyze: boolean): ExplainOptions {
  const c = caps.value;
  const format = options.format !== null && formatsFor(analyze).includes(options.format) ? options.format : null;
  return {
    analyze,
    buffers: !!c?.buffers && options.buffers,
    verbose: !!c?.verbose && options.verbose,
    settings: !!c?.settings && options.settings,
    wal: analyze && !!c?.wal && options.wal,
    format,
    confirmed: false,
  };
}

const optionSummary = computed(() => {
  const o = optionsFor(true);
  const on = [o.buffers && 'BUFFERS', o.verbose && 'VERBOSE', o.settings && 'SETTINGS', o.wal && 'WAL', o.format && `FORMAT ${o.format.toUpperCase()}`].filter(Boolean);
  return on.length > 0 ? on.join(', ') : 'defaults';
});

// Running

type Result =
  | { kind: 'plan'; plan: Plan; options: ExplainOptions; at: number }
  | { kind: 'error'; error: CmdError; analyze: boolean }
  | { kind: 'cancelled'; analyze: boolean }
  | { kind: 'declined' };

const result = shallowRef<Result | null>(null);
const running = ref<'explain' | 'analyze' | null>(null);
const cancelling = ref(false);
const confirmRequest = shallowRef<ExplainConfirmation | null>(null);
let answerConfirm: ((yes: boolean) => void) | null = null;

function askConfirm(request: ExplainConfirmation): Promise<boolean> {
  confirmRequest.value = request;
  return new Promise((resolve) => {
    answerConfirm = resolve;
  });
}

function onConfirmDone(yes: boolean): void {
  const resolve = answerConfirm;
  answerConfirm = null;
  confirmRequest.value = null;
  resolve?.(yes);
}

const canRun = computed(() => props.statement.trim() !== '' && running.value === null && props.session.engine !== 'redis');

async function run(analyze: boolean): Promise<void> {
  if (!canRun.value) return;
  if (!caps.value) await loadCaps();
  const opts = optionsFor(analyze);
  running.value = analyze ? 'analyze' : 'explain';
  cancelling.value = false;
  emit('running', true);
  try {
    const outcome = await runExplain((confirmed) => explain(props.session.id, props.statement, opts, confirmed), askConfirm);
    switch (outcome.status) {
      case 'done':
        result.value = { kind: 'plan', plan: outcome.value, options: opts, at: Date.now() };
        selected.value = hottest(outcome.value);
        break;
      case 'failed':
        result.value = { kind: 'error', error: report(outcome.error), analyze };
        break;
      case 'cancelled':
        result.value = { kind: 'cancelled', analyze };
        break;
      case 'declined':
        result.value = { kind: 'declined' };
        break;
    }
  } finally {
    running.value = null;
    cancelling.value = false;
    emit('running', false);
  }
}

/** Stops the running explain as a whole (its check, the run and every later step). */
async function stop(): Promise<void> {
  if (running.value === null || cancelling.value) return;
  cancelling.value = true;
  try {
    await cancel(props.session.id);
  } catch (e: unknown) {
    cancelling.value = false;
    toasts.pushError(report(e), 'Could not cancel EXPLAIN');
  }
}

onMounted(async () => {
  await loadCaps();
  if (props.autoRun) await run(props.autoRun === 'analyze');
});

// A new statement makes the shown plan stale; keep it visible but say so.
const stale = ref(false);
watch(
  () => props.statement,
  () => {
    stale.value = result.value?.kind === 'plan';
  },
);
watch(result, () => {
  stale.value = false;
});

// Plan display

type View = 'visual' | 'timeline' | 'text' | 'raw';
const view = ref<View>('visual');
const highlight = ref<HighlightMode>('share');
const selected = ref<number | null>(null);

const plan = computed(() => (result.value?.kind === 'plan' ? result.value.plan : null));
const selectedNode = computed(() => (plan.value && selected.value !== null ? findNode(plan.value.root, selected.value) : null));
const hasBuffers = computed(() => !!plan.value && flattenPlan(plan.value.root).some((f) => f.node.buffers !== null));
const hasMisestimates = computed(() => !!plan.value?.analyzed);

function hottest(p: Plan): number | null {
  let best: { id: number; share: number } | null = null;
  for (const { node } of flattenPlan(p.root)) {
    if (node.share !== null && (best === null || node.share > best.share)) best = { id: node.id, share: node.share };
  }
  return best?.id ?? null;
}

function select(id: number): void {
  selected.value = selected.value === id ? null : id;
}

const sharedRead = computed(() => {
  const b = plan.value?.root.buffers;
  if (!b || plan.value?.format !== 'postgresJson') return null;
  return formatBytes(b.sharedRead * PG_BLOCK_BYTES);
});

const rawText = computed(() => {
  const p = plan.value;
  if (!p) return '';
  return p.format === 'mysqlTree' ? p.raw : (prettyJson(p.raw) ?? p.raw);
});

const outline = computed(() => (plan.value ? planOutline(plan.value) : ''));

async function copyPlan(): Promise<void> {
  const text = view.value === 'text' ? outline.value : rawText.value;
  try {
    await navigator.clipboard.writeText(text);
    toasts.push('success', 'Plan copied', view.value === 'text' ? 'Indented text' : 'Server output');
  } catch (e: unknown) {
    toasts.pushError(report(e), 'Could not copy the plan');
  }
}

const errorView = computed(() => (result.value?.kind === 'error' ? presentExplainError(result.value.error) : null));
const capsErrorText = computed(() => (capsError.value ? presentError(capsError.value).message : ''));

defineExpose({
  /** Explains the statement; `analyze` runs it (asking first when needed). */
  run,
  /** Cancels a running explain. */
  stop,
});
</script>

<template>
  <section
    class="explain"
    aria-label="Execution plan"
  >
    <div class="toolbar">
      <button
        type="button"
        class="btn compact"
        :disabled="!canRun"
        @click="run(false)"
      >
        Explain
      </button>
      <div class="split">
        <button
          type="button"
          class="btn compact primary"
          :disabled="!canRun || (caps !== null && !caps.analyze)"
          :title="caps && !caps.analyze ? 'This server version has no EXPLAIN ANALYZE' : ''"
          @click="run(true)"
        >
          <Icon name="play" />Explain analyze
        </button>
        <button
          type="button"
          class="btn compact more"
          :aria-expanded="showOptions"
          :aria-label="`Explain options: ${optionSummary}`"
          @click="showOptions = !showOptions"
        >
          <Icon name="chevronDown" />
        </button>
      </div>
      <button
        v-if="running"
        type="button"
        class="btn compact danger"
        :disabled="cancelling"
        @click="stop"
      >
        <Icon name="stop" />{{ cancelling ? 'Cancelling…' : 'Cancel' }}
      </button>
      <span
        class="hint"
        aria-live="polite"
      >
        <template v-if="running === 'analyze'">Running EXPLAIN ANALYZE…</template>
        <template v-else-if="running === 'explain'">Planning…</template>
        <template v-else>ANALYZE runs the statement inside a transaction that is rolled back. Varve asks first unless the server proves it only reads.</template>
      </span>
      <span class="grow" />
      <span
        v-if="session.database"
        class="subtle"
      >{{ session.database }}</span>
      <EnvBadge :environment="session.environment" />
    </div>

    <div
      v-if="showOptions"
      class="options"
      role="group"
      aria-label="Explain options"
    >
      <p
        v-if="capsErrorText"
        class="problem"
      >
        {{ capsErrorText }}
      </p>
      <template v-else-if="caps">
        <label
          v-if="caps.buffers"
          class="check"
        ><input
          v-model="options.buffers"
          type="checkbox"
        >BUFFERS</label>
        <label
          v-if="caps.verbose"
          class="check"
        ><input
          v-model="options.verbose"
          type="checkbox"
        >VERBOSE</label>
        <label
          v-if="caps.settings"
          class="check"
        ><input
          v-model="options.settings"
          type="checkbox"
        >SETTINGS</label>
        <label
          v-if="caps.wal"
          class="check"
          title="Only with ANALYZE"
        ><input
          v-model="options.wal"
          type="checkbox"
        >WAL (with ANALYZE)</label>
        <label
          v-if="caps.formats.length > 1 || caps.analyzeFormats.length > 1"
          class="check"
        >Format
          <select
            v-model="options.format"
            class="select small-select"
          >
            <option :value="null">Server default</option>
            <option value="json">JSON</option>
            <option value="tree">Tree</option>
          </select>
        </label>
        <span
          v-if="!caps.buffers && !caps.verbose && !caps.settings && caps.formats.length <= 1 && caps.analyzeFormats.length <= 1"
          class="subtle"
        >This server has no EXPLAIN options.</span>
      </template>
      <span
        v-else
        class="subtle"
      >Loading options…</span>
    </div>

    <div
      v-if="!result"
      class="placeholder"
    >
      <p v-if="running">
        Waiting for the server…
      </p>
      <p v-else-if="!statement.trim()">
        Select or write a statement to explain.
      </p>
      <p v-else>
        Explain shows the plan without running the statement. Explain analyze runs it and reports actual rows and times.
      </p>
    </div>

    <div
      v-else-if="result.kind === 'error' && errorView"
      class="error"
      role="alert"
    >
      <Icon
        name="warning"
        :size="16"
      />
      <div class="error-body">
        <strong>{{ errorView.title }}</strong>
        <span>{{ errorView.message }}</span>
        <span
          v-if="errorView.code"
          class="mono"
        >Code {{ errorView.code }}</span>
        <span v-if="errorView.detail">{{ errorView.detail }}</span>
        <span v-if="errorView.hint">Hint: {{ errorView.hint }}</span>
        <span
          v-if="errorView.position !== null"
          class="subtle"
        >At character {{ errorView.position }} of the statement.</span>
      </div>
    </div>

    <div
      v-else-if="result.kind === 'cancelled'"
      class="placeholder"
    >
      <p>{{ result.analyze ? 'EXPLAIN ANALYZE' : 'EXPLAIN' }} was cancelled. Anything it opened was rolled back.</p>
    </div>

    <div
      v-else-if="result.kind === 'declined'"
      class="placeholder"
    >
      <p>Not run. Use Explain to see the plan without running the statement.</p>
    </div>

    <template v-else-if="plan && result.kind === 'plan'">
      <div class="planbar">
        <div
          class="views"
          role="group"
          aria-label="Plan view"
        >
          <button
            v-for="v in (['visual', 'timeline', 'text', 'raw'] as const)"
            :key="v"
            type="button"
            :aria-pressed="view === v"
            @click="view = v"
          >
            {{ v === 'visual' ? 'Visual' : v === 'timeline' ? 'Timeline' : v === 'text' ? 'Text' : plan.format === 'mysqlTree' ? 'Raw' : 'JSON' }}
          </button>
        </div>
        <span class="mono label-cmd"><span class="kw">{{ explainLabel(result.options).split(' ')[0] }}</span> {{ explainLabel(result.options).split(' ').slice(1).join(' ') }}</span>
        <span v-if="plan.planningMs !== null">Planning <span class="mono val">{{ formatMs(plan.planningMs) }}</span></span>
        <span v-if="plan.executionMs !== null">Execution <span class="mono val">{{ formatMs(plan.executionMs) }}</span></span>
        <span v-if="plan.totalCost !== null">Cost <span class="mono val">{{ formatNumber(plan.totalCost, 2) }}</span></span>
        <span v-if="sharedRead">Shared read <span class="mono val">{{ sharedRead }}</span></span>
        <span
          v-if="stale"
          class="pill tone-warning"
        >Statement changed since</span>
        <span class="grow" />
        <template v-if="view === 'visual'">
          <label for="explain-highlight">Highlight</label>
          <select
            id="explain-highlight"
            v-model="highlight"
            class="select small-select"
          >
            <option value="share">
              {{ plan.shareBasis === 'cost' ? 'Self cost' : HIGHLIGHT_LABELS.share }}
            </option>
            <option
              value="misestimate"
              :disabled="!hasMisestimates"
            >
              {{ HIGHLIGHT_LABELS.misestimate }}
            </option>
            <option
              value="buffersRead"
              :disabled="!hasBuffers"
            >
              {{ HIGHLIGHT_LABELS.buffersRead }}
            </option>
          </select>
        </template>
        <button
          type="button"
          class="btn compact"
          @click="copyPlan"
        >
          <Icon name="copy" />Copy plan
        </button>
      </div>

      <ul
        v-if="plan.warnings.length > 0 || plan.notices.length > 0"
        class="plan-notes"
      >
        <li
          v-for="(w, i) in plan.warnings"
          :key="`w${i}`"
          class="notice tone-warning"
        >
          <Icon name="warning" />{{ w }}
        </li>
        <li
          v-for="(n, i) in plan.notices"
          :key="`n${i}`"
          class="notice tone-info"
        >
          <Icon name="info" />{{ n }}
        </li>
      </ul>

      <div class="planbody">
        <PlanTree
          v-if="view === 'visual'"
          :plan="plan"
          :selected="selected"
          :highlight="highlight"
          @select="select"
        />
        <PlanTimeline
          v-else-if="view === 'timeline'"
          :plan="plan"
          :selected="selected"
          @select="select"
        />
        <pre
          v-else
          class="rawtext mono"
          :aria-label="view === 'text' ? 'Plan as text' : 'Server output'"
        >{{ view === 'text' ? outline : rawText }}</pre>
        <PlanNodeDetails
          v-if="view === 'visual' || view === 'timeline'"
          :plan="plan"
          :node="selectedNode"
        />
      </div>
    </template>

    <ExplainConfirmDialog
      v-if="confirmRequest"
      :connection-name="connectionName"
      :environment="confirmRequest.environment ?? session.environment"
      :reasons="confirmRequest.reasons"
      @done="onConfirmDone"
    />
  </section>
</template>

<style scoped>
.explain {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--surface);
  color: var(--text);
}

.toolbar,
.planbar,
.options {
  display: flex;
  flex-shrink: 0;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  min-height: 40px;
  padding: 6px 12px;
  border-bottom: 1px solid var(--border-inner);
  background: var(--surface);
  font-size: 12px;
}

.options {
  gap: 16px;
  background: var(--ground);
}

.split {
  display: flex;
}

.split .btn:first-child {
  border-top-right-radius: 0;
  border-bottom-right-radius: 0;
}

.split .more {
  width: 26px;
  padding: 0;
  border-left: 0;
  border-top-left-radius: 0;
  border-bottom-left-radius: 0;
}

.hint,
.subtle {
  color: var(--text-subtle);
}

.grow {
  flex: 1;
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  color: var(--text-muted);
}

.small-select {
  width: auto;
  height: 26px;
  padding: 0 6px;
  font-size: 12px;
}

.problem {
  margin: 0;
  color: var(--danger-text);
}

.placeholder {
  display: flex;
  flex: 1;
  align-items: center;
  justify-content: center;
  padding: 24px;
  color: var(--text-subtle);
  font-size: 13px;
  text-align: center;
}

.error {
  display: flex;
  gap: 10px;
  margin: 12px;
  padding: 10px 12px;
  border-radius: var(--radius-panel);
  background: var(--danger-tint);
  color: var(--danger-text);
}

.error-body {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  color: var(--text);
  font-size: 12px;
  word-break: break-word;
}

.error-body strong {
  color: var(--danger-text);
}

.views {
  display: inline-flex;
  padding: 2px;
  border-radius: var(--radius-panel);
  background: var(--surface-raised);
}

.views button {
  height: 24px;
  padding: 0 10px;
  border: 1px solid transparent;
  border-radius: var(--radius-control);
  background: transparent;
  color: var(--text-muted);
  font-size: 12px;
  cursor: pointer;
}

.views button[aria-pressed='true'] {
  border-color: var(--border);
  background: var(--surface);
  color: var(--text);
  font-weight: 500;
}

.planbar {
  gap: 12px;
  color: var(--text-muted);
}

.label-cmd {
  color: var(--text-muted);
}

.kw {
  color: var(--code-keyword);
}

.val {
  color: var(--text);
}

.plan-notes {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0;
  padding: 8px 12px;
  border-bottom: 1px solid var(--border-inner);
  list-style: none;
}

.planbody {
  display: flex;
  flex: 1;
  min-height: 0;
}

.rawtext {
  flex: 1;
  margin: 0;
  padding: 12px 16px;
  overflow: auto;
  background: var(--ground);
  font-size: 12px;
  line-height: 18px;
  white-space: pre;
}
</style>
