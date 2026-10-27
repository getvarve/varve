<script setup lang="ts">
import { computed, onMounted, reactive, ref, useId } from 'vue';
import { catalogDescribe } from '../../api/ddl';
import type { MyTable } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import { execute } from '../../api/sessions';
import type { StatementResult } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import {
  buildAddVersioning,
  buildDropVersioning,
  changedColumns,
  deleteHistorySql,
  errorsOf,
  explicitVersioning,
  historyQuerySchema,
  historySelect,
  versioningSchema,
} from '../../lib/ddlPlan';
import type { HistoryQuery, SystemTimeMode, VersioningForm } from '../../lib/ddlPlan';
import { applyProblem } from '../../lib/ddlApply';
import type { ApplyProblem } from '../../lib/ddlApply';
import { runWithConfirmation } from '../../lib/destructive';
import { cellText, formatCount, formatDuration } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import DdlView from './DdlView.vue';
import PlanPreview from './PlanPreview.vue';
import SqlCode from './SqlCode.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';

/**
 * MariaDB system-versioned table (design canvas SystemVersionedTable):
 * browse row versions with FOR SYSTEM_TIME (AS OF, BETWEEN, FROM ... TO,
 * ALL), compare two versions, delete old history, and add or drop system
 * versioning. Reads work on read-only connections; changes are disabled
 * there. Props and events: `ObjectEditorProps` / `ObjectEditorEmits`;
 * `object` is a MariaDB `table`.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const table = ref<MyTable | null>(null);
const loading = ref(false);
const loadError = ref<string | null>(null);
const tab = ref<'history' | 'versioning' | 'ddl'>('history');

async function load(): Promise<void> {
  if (!props.object) {
    loadError.value = 'Open this view on a table.';
    return;
  }
  loading.value = true;
  loadError.value = null;
  try {
    const d = await catalogDescribe(props.session.id, props.object);
    if (d.engine === 'mysql' && d.object.kind === 'table') table.value = d.object;
    else loadError.value = 'This object is not a MariaDB table.';
  } catch (error: unknown) {
    props.reportError?.(error);
    loadError.value = presentError(toCmdError(error)).message;
  } finally {
    loading.value = false;
  }
}

const versioned = computed(() => table.value?.systemVersioning ?? null);
const explicit = computed(() => (table.value ? explicitVersioning(table.value.columns) : null));
const rowStart = computed(() => versioned.value?.rowStart ?? 'row_start');
const rowEnd = computed(() => versioned.value?.rowEnd ?? 'row_end');
const keyColumns = computed(() => table.value?.columns.filter((c) => c.key === 'PRI').map((c) => c.name) ?? []);
const title = computed(() => (props.object ? displayName(props.object, 'mysql') : 'Table'));

// History browser.
const today = new Date();
const pad = (n: number): string => String(n).padStart(2, '0');
const stamp = (d: Date): string => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
const query = reactive<HistoryQuery>({ mode: 'between', from: stamp(new Date(today.getTime() - 7 * 86_400_000)), to: stamp(today), where: '', limit: 500 });
const queryErrors = computed(() => errorsOf(historyQuerySchema, query));
const sql = computed(() =>
  table.value ? historySelect(table.value.database, table.value.name, rowStart.value, rowEnd.value, keyColumns.value, query) : '',
);
const result = ref<Extract<StatementResult, { kind: 'rows' }> | null>(null);
const resultMs = ref(0);
const running = ref(false);
const runProblem = ref<ApplyProblem | null>(null);
const picked = ref<number[]>([]);
const ids = { from: useId(), to: useId(), where: useId(), limit: useId(), before: useId(), start: useId(), end: useId() };
const MODES: { value: SystemTimeMode; label: string }[] = [
  { value: 'asOf', label: 'AS OF' },
  { value: 'between', label: 'BETWEEN' },
  { value: 'fromTo', label: 'FROM ... TO' },
  { value: 'all', label: 'ALL' },
];

async function showVersions(): Promise<void> {
  if (Object.keys(queryErrors.value).length > 0 || !table.value) return;
  running.value = true;
  runProblem.value = null;
  picked.value = [];
  try {
    const out = await execute(props.session.id, sql.value, false, query.limit);
    const rows = out.results.find((r): r is Extract<StatementResult, { kind: 'rows' }> => r.kind === 'rows') ?? null;
    result.value = rows;
    resultMs.value = out.elapsedMs;
  } catch (error: unknown) {
    props.reportError?.(error);
    runProblem.value = applyProblem(error);
  } finally {
    running.value = false;
  }
}

const columnNames = computed(() => result.value?.columns.map((c) => c.name) ?? []);
const textRows = computed(() => result.value?.rows.map((r) => r.map(cellText)) ?? []);
const comparison = computed(() => {
  if (picked.value.length !== 2) return null;
  const [a, b] = [...picked.value].sort((x, y) => x - y);
  const ra = textRows.value[a as number];
  const rb = textRows.value[b as number];
  if (!ra || !rb) return null;
  const skip = [rowStart.value, rowEnd.value];
  const changed = changedColumns(columnNames.value, ra, rb, skip);
  return {
    changed: changed.map((c) => {
      const i = columnNames.value.indexOf(c);
      return { column: c, before: ra[i] ?? '', after: rb[i] ?? '' };
    }),
    unchanged: columnNames.value.filter((c) => !skip.includes(c) && !changed.includes(c)),
  };
});

function togglePick(index: number): void {
  const at = picked.value.indexOf(index);
  if (at >= 0) picked.value.splice(at, 1);
  else picked.value = [...picked.value.slice(-1), index];
}

// Delete history.
const before = ref('');
const deleteProblem = ref<ApplyProblem | null>(null);
const deleteNote = ref<string | null>(null);
const deleting = ref(false);
const beforeValid = computed(() => /^\d{4}-\d{2}-\d{2}( \d{2}:\d{2}(:\d{2})?)?$/.test(before.value.trim()));

async function deleteHistory(): Promise<void> {
  const t = table.value;
  if (!t || !beforeValid.value || deleting.value) return;
  const statement = deleteHistorySql(t.database, t.name, before.value);
  const ok = await useDialogs().askConfirm({
    title: 'Delete history?',
    message: `${statement}\n\nRow versions that ended before ${before.value.trim()} are deleted for good.`,
    confirmLabel: 'Delete history',
    danger: true,
  });
  if (!ok) return;
  deleting.value = true;
  deleteProblem.value = null;
  deleteNote.value = null;
  try {
    const out = await runWithConfirmation(
      (confirmed) => execute(props.session.id, statement, confirmed),
      (request) => useDialogs().askDestructive({ connectionName: props.connectionName, environment: request.environment, statements: request.statements }),
    );
    if (out) {
      const command = out.results.find((r) => r.kind === 'command');
      const n = command && command.kind === 'command' ? command.rowsAffected : null;
      deleteNote.value = n === null ? 'History deleted.' : `${formatCount(n)} history rows deleted.`;
      emit('changed');
    }
  } catch (error: unknown) {
    props.reportError?.(error);
    deleteProblem.value = applyProblem(error);
  } finally {
    deleting.value = false;
  }
}

// Add or drop versioning.
const plan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => emit('changed'),
  reportError: (error) => props.reportError?.(error),
});
const versioning = reactive<VersioningForm>({ columns: 'implicit', start: 'row_start', end: 'row_end', precision: 'timestamp', invisible: true });
const versioningErrors = computed(() => errorsOf(versioningSchema, versioning));

async function planAdd(): Promise<void> {
  if (!table.value || Object.keys(versioningErrors.value).length > 0) return;
  await plan.plan([{ kind: 'edit', change: buildAddVersioning(table.value.database, table.value.name, versioning) }]);
}

async function planDrop(): Promise<void> {
  const t = table.value;
  if (!t || !versioned.value) return;
  await plan.plan([{ kind: 'edit', change: buildDropVersioning(t.database, t.name, versioned.value.rowStart, versioned.value.rowEnd, explicit.value !== null) }]);
}

async function changed(): Promise<void> {
  plan.clear();
  await load();
}

onMounted(load);
</script>

<template>
  <div class="versioned">
    <header class="header">
      <h1 class="mono">
        {{ title }}
      </h1>
      <span
        v-if="versioned"
        class="pill tone-info"
      >WITH SYSTEM VERSIONING</span>
      <span
        v-else-if="table"
        class="pill tone-neutral"
      >not versioned</span>
      <EnvBadge :environment="session.environment" />
      <span
        v-if="session.readOnly"
        class="pill tone-neutral"
      ><Icon name="lock" />Read only</span>
      <span
        v-if="table?.rowEstimate !== null && table?.rowEstimate !== undefined"
        class="subtle"
      >{{ formatCount(table.rowEstimate) }} current rows (estimate)</span>
    </header>
    <div
      role="tablist"
      aria-label="Table sections"
      class="tabs"
    >
      <button
        v-for="t in (['history', 'versioning', 'ddl'] as const)"
        :key="t"
        type="button"
        role="tab"
        :aria-selected="tab === t"
        @click="tab = t"
      >
        {{ t === 'history' ? 'History' : t === 'versioning' ? 'Versioning' : 'DDL' }}
      </button>
    </div>
    <p
      v-if="session.readOnly"
      class="notice tone-neutral state"
      role="status"
    >
      <Icon name="lock" />Read-only connection. History browsing works; deleting history and changing versioning are disabled.
    </p>
    <p
      v-if="loading"
      class="muted state"
      role="status"
    >
      Loading
    </p>
    <p
      v-else-if="loadError"
      class="notice tone-danger state"
      role="alert"
    >
      <Icon name="warning" />{{ loadError }}
    </p>

    <template v-else-if="table && tab === 'history'">
      <p
        v-if="!versioned"
        class="notice tone-info state"
      >
        <Icon name="info" />This table keeps no history. Add system versioning in the Versioning section.
      </p>
      <div
        v-else
        class="layout"
      >
        <div class="column">
          <section
            class="section"
            aria-label="History query"
          >
            <div class="row">
              <span class="label">FOR SYSTEM_TIME</span>
              <div
                role="radiogroup"
                aria-label="FOR SYSTEM_TIME"
                class="segmented"
              >
                <button
                  v-for="m in MODES"
                  :key="m.value"
                  type="button"
                  role="radio"
                  :aria-checked="query.mode === m.value"
                  @click="query.mode = m.value"
                >
                  {{ m.label }}
                </button>
              </div>
            </div>
            <div class="grid">
              <FormField
                v-if="query.mode !== 'all'"
                :label="query.mode === 'asOf' ? 'As of' : 'From'"
                :input-id="ids.from"
                :error="queryErrors.from ?? ''"
              >
                <input
                  :id="ids.from"
                  v-model="query.from"
                  class="input mono"
                >
              </FormField>
              <FormField
                v-if="query.mode === 'between' || query.mode === 'fromTo'"
                label="To"
                :input-id="ids.to"
                :error="queryErrors.to ?? ''"
              >
                <input
                  :id="ids.to"
                  v-model="query.to"
                  class="input mono"
                >
              </FormField>
              <FormField
                label="WHERE"
                :input-id="ids.where"
                class="wide"
                :error="queryErrors.where ?? ''"
              >
                <input
                  :id="ids.where"
                  v-model="query.where"
                  class="input mono"
                  placeholder="id IN (88412, 88413)"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                label="Row limit"
                :input-id="ids.limit"
                :error="queryErrors.limit ?? ''"
              >
                <input
                  :id="ids.limit"
                  v-model.number="query.limit"
                  type="number"
                  min="1"
                  class="input mono"
                >
              </FormField>
            </div>
            <div class="code-box small">
              <SqlCode
                :model-value="sql"
                engine="mariadb"
                label="History query"
                readonly
                :min-height="40"
              />
            </div>
            <div>
              <button
                type="button"
                class="btn primary compact"
                :disabled="running || Object.keys(queryErrors).length > 0"
                @click="showVersions"
              >
                {{ running ? 'Running' : 'Show versions' }}
              </button>
            </div>
            <p
              v-if="runProblem"
              class="notice tone-danger"
              role="alert"
            >
              <Icon name="warning" />{{ runProblem.title }}: {{ runProblem.message }}{{ runProblem.detail ? ` (${runProblem.detail})` : '' }}
            </p>
          </section>
          <section
            v-if="result"
            class="section"
            aria-label="Row versions"
          >
            <div class="section-head">
              <h2>{{ formatCount(result.rows.length) }} versions</h2>
              <span class="hint">{{ formatDuration(resultMs) }}{{ result.truncated ? ' · stopped at the row limit' : '' }}</span>
              <span class="grow" />
              <span class="hint">Pick two rows to compare</span>
            </div>
            <div class="scroll">
              <table class="table">
                <thead>
                  <tr>
                    <th><span class="visually-hidden">Compare</span></th>
                    <th
                      v-for="c in columnNames"
                      :key="c"
                      class="mono"
                    >
                      {{ c }}
                    </th>
                  </tr>
                </thead>
                <tbody>
                  <tr
                    v-for="(r, i) in textRows"
                    :key="i"
                    :class="{ picked: picked.includes(i) }"
                  >
                    <td>
                      <input
                        type="checkbox"
                        :checked="picked.includes(i)"
                        :aria-label="`Compare row ${i + 1}`"
                        @change="togglePick(i)"
                      >
                    </td>
                    <td
                      v-for="(v, j) in r"
                      :key="j"
                      class="mono"
                    >
                      {{ v }}
                    </td>
                  </tr>
                </tbody>
              </table>
            </div>
          </section>
        </div>
        <div class="column">
          <section
            v-if="comparison"
            class="section"
            aria-label="Compare versions"
          >
            <h2>{{ comparison.changed.length }} of {{ comparison.changed.length + comparison.unchanged.length }} columns changed</h2>
            <table class="table">
              <thead>
                <tr>
                  <th>Column</th>
                  <th>Earlier row</th>
                  <th>Later row</th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="c in comparison.changed"
                  :key="c.column"
                >
                  <td class="mono">
                    {{ c.column }}
                  </td>
                  <td class="mono">
                    {{ c.before }}
                  </td>
                  <td class="mono">
                    {{ c.after }}
                  </td>
                </tr>
              </tbody>
            </table>
            <p class="hint">
              Unchanged: {{ comparison.unchanged.join(', ') || 'none' }}. System versioning stores when a version changed, not who changed it.
            </p>
          </section>
          <section
            class="section"
            aria-label="Delete history"
          >
            <h2>Delete history</h2>
            <FormField
              label="Delete versions that ended before"
              :input-id="ids.before"
              :error="before !== '' && !beforeValid ? 'Write it as YYYY-MM-DD HH:MM:SS.' : ''"
            >
              <input
                :id="ids.before"
                v-model="before"
                class="input mono"
                placeholder="YYYY-MM-DD HH:MM:SS"
              >
            </FormField>
            <div>
              <button
                type="button"
                class="btn compact danger"
                :disabled="session.readOnly || !beforeValid || deleting"
                @click="deleteHistory"
              >
                Delete history
              </button>
            </div>
            <p
              v-if="deleteNote"
              class="notice tone-success"
              role="status"
            >
              {{ deleteNote }}
            </p>
            <p
              v-if="deleteProblem"
              class="notice tone-danger"
              role="alert"
            >
              <Icon name="warning" />{{ deleteProblem.title }}: {{ deleteProblem.message }}
            </p>
          </section>
        </div>
      </div>
    </template>

    <div
      v-else-if="table && tab === 'versioning'"
      class="layout"
    >
      <section
        class="section"
        aria-label="System versioning"
      >
        <template v-if="versioned">
          <h2>System versioning is on</h2>
          <p class="hint">
            Row start <span class="mono">{{ rowStart }}</span>, row end <span class="mono">{{ rowEnd }}</span>
            ({{ explicit ? 'named columns' : 'implicit hidden columns' }}).
          </p>
          <p class="notice tone-warning">
            <Icon name="warning" />Dropping system versioning deletes all history rows{{ explicit ? ' and the row start and end columns' : '' }}.
          </p>
          <div>
            <button
              type="button"
              class="btn compact"
              :disabled="session.readOnly"
              @click="planDrop"
            >
              Preview drop
            </button>
          </div>
        </template>
        <template v-else>
          <h2>Add system versioning</h2>
          <div
            role="radiogroup"
            aria-label="Row period columns"
            class="segmented"
          >
            <button
              type="button"
              role="radio"
              :aria-checked="versioning.columns === 'implicit'"
              @click="versioning.columns = 'implicit'"
            >
              Implicit (hidden)
            </button>
            <button
              type="button"
              role="radio"
              :aria-checked="versioning.columns === 'explicit'"
              @click="versioning.columns = 'explicit'"
            >
              Named columns
            </button>
          </div>
          <div
            v-if="versioning.columns === 'explicit'"
            class="grid"
          >
            <FormField
              label="Row start column"
              :input-id="ids.start"
              :error="versioningErrors.start ?? ''"
            >
              <input
                :id="ids.start"
                v-model="versioning.start"
                class="input mono"
              >
            </FormField>
            <FormField
              label="Row end column"
              :input-id="ids.end"
              :error="versioningErrors.end ?? ''"
            >
              <input
                :id="ids.end"
                v-model="versioning.end"
                class="input mono"
              >
            </FormField>
            <label class="check"><input
              v-model="versioning.precision"
              type="radio"
              value="timestamp"
            ><span>TIMESTAMP(6)</span></label>
            <label class="check"><input
              v-model="versioning.precision"
              type="radio"
              value="transaction"
            ><span>BIGINT UNSIGNED<span class="sub">transaction-precise (InnoDB)</span></span></label>
            <label class="check"><input
              v-model="versioning.invisible"
              type="checkbox"
            ><span>INVISIBLE (not in SELECT *)</span></label>
          </div>
          <p class="hint">
            Adding versioning copies the table.
          </p>
          <div>
            <button
              type="button"
              class="btn compact"
              :disabled="session.readOnly || Object.keys(versioningErrors).length > 0"
              @click="planAdd"
            >
              Preview
            </button>
          </div>
        </template>
      </section>
      <PlanPreview
        v-if="plan.state.steps.length > 0 || plan.state.planProblem"
        :plan="plan"
        :engine="session.engine"
        :read-only="session.readOnly"
        :apply-label="versioned ? 'Drop versioning' : 'Add versioning'"
        @applied="changed"
        @reconnect="emit('reconnect')"
        @refresh="load"
      />
    </div>

    <DdlView
      v-else-if="table && tab === 'ddl' && object"
      :session="session"
      :connection-name="connectionName"
      :object="object"
      :report-error="reportError"
      @open-sql="emit('openSql', $event)"
      @changed="load(); emit('changed')"
      @reconnect="emit('reconnect')"
    />
  </div>
</template>

<style scoped src="./editor.css"></style>
<style scoped>
.versioned {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: auto;
  background: var(--ground);
}

.header {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px;
  min-height: 48px;
  padding: 8px 16px;
  border-bottom: 1px solid var(--border-inner);
  background: var(--surface);
}

h1 {
  margin: 0;
  font-size: 15px;
  font-weight: 500;
}

.tabs {
  display: flex;
  gap: 2px;
  height: 36px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border);
  background: var(--surface);
}

.tabs button {
  height: 35px;
  padding: 0 12px;
  border: 0;
  background: transparent;
  color: var(--text-muted);
  font-size: 13px;
  cursor: pointer;
}

.tabs button[aria-selected='true'] {
  color: var(--text);
  font-weight: 500;
  box-shadow: inset 0 -2px 0 var(--accent);
}

.state {
  margin: 16px 16px 0;
}

.row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px;
}

.code-box.small {
  height: 72px;
}

.scroll {
  overflow: auto;
  max-height: 480px;
}

tr.picked td {
  background: var(--accent-tint);
}
</style>
