<script setup lang="ts">
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, onMounted, reactive, ref, useId, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { CATEGORIES, clearActivity, exportActivity, fetchActivity, OUTCOMES, statementOf } from '../api/activity';
import type { ActivityEvent, ActivityExportFormat, ActivityFilter, Category, Outcome } from '../api/activity';
import { presentError, toCmdError } from '../api/errors';
import Icon from '../components/common/Icon.vue';
import { useDialogs } from '../composables/useDialogs';
import { useToasts } from '../composables/useToasts';
import { useWorkspace } from '../composables/useWorkspace';
import { useWorkspaceActions } from '../composables/useWorkspaceActions';
import { formatDateTime, formatDay, formatTime } from '../lib/display';
import { parseActions } from '../lib/activityFilter';
import { activityFilterSchema } from '../lib/forms';
import type { ActivityFilterValues } from '../lib/forms';
import { formatCount, formatDuration, reindentJson } from '../lib/values';

/** The local activity log (design doc 9.6): filters, newest first, details, export and clear. */
const route = useRoute();
const router = useRouter();
const workspace = useWorkspace();
const actions = useWorkspaceActions();
const dialogs = useDialogs();
const toasts = useToasts();
const uid = useId();

type Range = 'today' | '7d' | '30d' | '90d' | 'all';
const RANGES: { value: Range; label: string }[] = [
  { value: 'today', label: 'Today' },
  { value: '7d', label: '7 d' },
  { value: '30d', label: '30 d' },
  { value: '90d', label: '90 d' },
  { value: 'all', label: 'All' },
];

/** Range and categories are toggle buttons; the input fields below are a validated form. */
const filters = reactive<{ range: Range; categories: Category[] }>({ range: '7d', categories: [] });

const filterForm = useForm({
  validationSchema: toTypedSchema(activityFilterSchema),
  initialValues: {
    text: '',
    outcome: '',
    connectionId: typeof route.query.connection === 'string' ? route.query.connection : '',
    actions: '',
  } satisfies ActivityFilterValues,
});
const [searchText, searchAttrs] = filterForm.defineField('text', { validateOnModelUpdate: true });
const [outcome] = filterForm.defineField('outcome');
const [connectionId] = filterForm.defineField('connectionId');
const [actionsText, actionsAttrs] = filterForm.defineField('actions', { validateOnModelUpdate: true });
const formValues = computed(() => filterForm.values as ActivityFilterValues);
const formValid = computed(() => activityFilterSchema.safeParse(filterForm.values).success);

const events = ref<ActivityEvent[]>([]);
const nextCursor = ref<string | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);
const selected = ref<ActivityEvent | null>(null);

function rangeStart(range: Range): number | undefined {
  const now = new Date();
  if (range === 'all') return undefined;
  if (range === 'today') return new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  const days = range === '7d' ? 7 : range === '30d' ? 30 : 90;
  return now.getTime() - days * 86_400_000;
}

/**
 * The filter for the current controls, without paging. A function, not a
 * cached value: the time range ("last 7 days") is computed when it is used.
 */
function currentFilter(): ActivityFilter {
  const out: ActivityFilter = { limit: 100 };
  const from = rangeStart(filters.range);
  if (from !== undefined) out.from = from;
  if (filters.categories.length > 0) out.categories = [...filters.categories];
  const v = formValues.value;
  if (v.outcome) out.outcomes = [v.outcome as Outcome];
  if (v.connectionId) out.connectionId = v.connectionId;
  const actions = parseActions(v.actions);
  if (actions.length > 0) out.actions = actions;
  if (v.text.trim()) out.text = v.text.trim();
  return out;
}

/** Known actions for the action filter suggestions, from the events loaded so far. */
const knownActions = computed(() => [...new Set(events.value.map((e) => e.action))].sort());

let request = 0;
/** The filter that produced `nextCursor`; "Load more" continues that filter, never one typed since. */
let pageFilter: ActivityFilter | null = null;

async function load(more = false): Promise<void> {
  if (more && (!pageFilter || !nextCursor.value)) return;
  if (!more && !formValid.value) return;
  const mine = ++request;
  const used = more && pageFilter ? pageFilter : currentFilter();
  loading.value = true;
  try {
    const page = await fetchActivity(more && nextCursor.value ? { ...used, beforeId: nextCursor.value } : used);
    if (mine !== request) return;
    events.value = more ? [...events.value, ...page.events] : page.events;
    nextCursor.value = page.nextCursor;
    pageFilter = used;
    error.value = null;
    if (!more) selected.value = null;
  } catch (e: unknown) {
    if (mine === request) error.value = presentError(toCmdError(e)).message;
  } finally {
    if (mine === request) loading.value = false;
  }
}

let debounce: ReturnType<typeof setTimeout> | undefined;
watch(
  () => [filters.range, filters.categories.join(','), formValues.value.outcome, formValues.value.connectionId],
  () => {
    clearTimeout(debounce);
    void load();
  },
);
watch(
  () => [formValues.value.text, formValues.value.actions],
  () => {
    clearTimeout(debounce);
    // While a typed filter waits, "Load more" is off: its cursor belongs to the old filter.
    nextCursor.value = null;
    debounce = setTimeout(() => void load(), 250);
  },
);
watch(
  () => route.query.connection,
  (connection) => {
    filterForm.setFieldValue('connectionId', typeof connection === 'string' ? connection : '');
  },
);
onMounted(() => void load());

function toggleCategory(category: Category): void {
  const at = filters.categories.indexOf(category);
  if (at >= 0) filters.categories.splice(at, 1);
  else filters.categories.push(category);
}

function resetFilters(): void {
  filters.range = '7d';
  filters.categories = [];
  filterForm.resetForm({ values: { text: '', outcome: '', connectionId: '', actions: '' } });
  void router.replace({ name: 'activity' });
}

// Display

const groups = computed(() => {
  const out: { day: string; events: ActivityEvent[] }[] = [];
  for (const event of events.value) {
    const day = formatDay(event.occurredAt);
    const last = out.at(-1);
    if (last && last.day === day) last.events.push(event);
    else out.push({ day, events: [event] });
  }
  return out;
});

function connectionName(id: string | null): string | null {
  if (!id) return null;
  return workspace.connection(id)?.name ?? 'Deleted connection';
}

function summary(event: ActivityEvent): string {
  const statement = statementOf(event);
  if (statement) return statement.replace(/\s+/g, ' ');
  const details = event.details;
  if (typeof details === 'object' && details !== null && !Array.isArray(details)) {
    const errorDetail = (details as Record<string, unknown>).error;
    if (typeof errorDetail === 'object' && errorDetail !== null) {
      const e = errorDetail as Record<string, unknown>;
      return [e.code, e.message].filter((x) => typeof x === 'string').join(' ');
    }
  }
  return '';
}

const OUTCOME = {
  success: { label: 'Success', tone: 'tone-success' },
  failure: { label: 'Failed', tone: 'tone-danger' },
  cancelled: { label: 'Cancelled', tone: 'tone-neutral' },
} as const;

/** Details JSON as stored. Backend values pass through JSON.parse in the IPC layer, so this re-indents its text. */
const detailsJson = computed(() => (selected.value ? reindentJson(JSON.stringify(selected.value.details) ?? 'null') : ''));

async function copyDetails(): Promise<void> {
  try {
    await navigator.clipboard.writeText(detailsJson.value);
    toasts.push('success', 'Details copied', '', 2000);
  } catch {
    toasts.push('warning', 'Could not copy to the clipboard');
  }
}

function openConnection(id: string | null): void {
  const profile = workspace.connection(id);
  if (!profile) return;
  actions.openConnection(profile);
  void router.push({ name: 'home' });
}

// Export and clear

async function exportLog(format: ActivityExportFormat): Promise<void> {
  // The export uses the filter the list was loaded with (same time range), never controls still being edited.
  const { valid } = await filterForm.validate();
  if (!valid || !pageFilter) {
    toasts.push('warning', 'Fix the filter first', 'The export uses the same filter as the list.');
    return;
  }
  const applied = pageFilter;
  try {
    const stamp = new Date().toISOString().slice(0, 10);
    const path = await saveFileDialog({
      title: format === 'csv' ? 'Export activity as CSV' : 'Export activity as JSON Lines',
      defaultPath: `varve-activity-${stamp}.${format}`,
      filters: [format === 'csv' ? { name: 'CSV', extensions: ['csv'] } : { name: 'JSON Lines', extensions: ['jsonl'] }],
    });
    if (!path) return;
    const { limit: _limit, ...exportFilter } = applied;
    const count = await exportActivity(exportFilter, path, format);
    toasts.push('success', `Exported ${formatCount(count)} events`, path);
  } catch (e: unknown) {
    toasts.pushError(e, 'Export failed');
  }
}

async function clearLog(): Promise<void> {
  const name = connectionName(formValues.value.connectionId || null);
  const ok = await dialogs.askConfirm({
    title: name ? `Clear the activity of ${name}?` : 'Clear the whole activity log?',
    message: name
      ? `Every event of ${name} is removed from the local log, including its query history. This cannot be undone.`
      : 'Every event in the local log is removed, including query history. This cannot be undone.',
    confirmLabel: 'Clear log',
    danger: true,
  });
  if (!ok) return;
  try {
    const removed = await clearActivity(formValues.value.connectionId || null);
    toasts.push('success', `Removed ${formatCount(removed)} events`);
    await load();
  } catch (e: unknown) {
    toasts.pushError(e, 'Could not clear the log');
  }
}
</script>

<template>
  <section class="activity">
    <header class="head">
      <h1>Activity</h1>
      <span class="subtle">Local log on this device · {{ formatCount(events.length) }} events shown</span>
      <span class="spacer" />
      <div
        class="segmented"
        role="group"
        aria-label="Time range"
      >
        <button
          v-for="r in RANGES"
          :key="r.value"
          type="button"
          :aria-pressed="filters.range === r.value"
          @click="filters.range = r.value"
        >
          {{ r.label }}
        </button>
      </div>
      <button
        type="button"
        class="btn compact"
        @click="exportLog('csv')"
      >
        <Icon name="download" />Export CSV
      </button>
      <button
        type="button"
        class="btn compact"
        @click="exportLog('jsonl')"
      >
        <Icon name="download" />Export JSONL
      </button>
      <button
        type="button"
        class="btn ghost compact"
        @click="clearLog"
      >
        Clear log…
      </button>
    </header>

    <div class="filters">
      <label
        class="search"
        :for="`${uid}-search`"
      >
        <Icon name="search" />
        <span class="visually-hidden">Search actions, targets and details</span>
        <input
          :id="`${uid}-search`"
          v-model="searchText"
          v-bind="searchAttrs"
          :aria-invalid="!!filterForm.errors.value.text"
          :aria-describedby="filterForm.errors.value.text ? 'activity-text-error' : undefined"
          type="search"
          placeholder="Search actions, targets and details"
        >
      </label>
      <label :for="`${uid}-outcome`">Outcome</label>
      <select
        :id="`${uid}-outcome`"
        v-model="outcome"
        class="select compact"
      >
        <option value="">
          Any outcome
        </option>
        <option
          v-for="o in OUTCOMES"
          :key="o"
          :value="o"
        >
          {{ OUTCOME[o].label }}
        </option>
      </select>
      <label :for="`${uid}-action`">Action</label>
      <input
        :id="`${uid}-action`"
        v-model="actionsText"
        v-bind="actionsAttrs"
        class="input compact action"
        :aria-invalid="!!filterForm.errors.value.actions"
        :list="`${uid}-actions`"
        placeholder="query.failed, connection.*"
        spellcheck="false"
        autocomplete="off"
        :aria-describedby="filterForm.errors.value.actions ? 'activity-action-error' : 'activity-action-help'"
      >
      <datalist :id="`${uid}-actions`">
        <option
          v-for="a in knownActions"
          :key="a"
          :value="a"
        />
      </datalist>
      <span
        id="activity-action-help"
        class="visually-hidden"
      >Comma separated actions; end one with .* to match a prefix.</span>
      <label :for="`${uid}-conn`">Connection</label>
      <select
        :id="`${uid}-conn`"
        v-model="connectionId"
        class="select compact"
      >
        <option value="">
          All connections
        </option>
        <option
          v-for="c in workspace.records.value.connections"
          :key="c.id"
          :value="c.id"
        >
          {{ c.name }}
        </option>
      </select>
    </div>

    <p
      v-if="filterForm.errors.value.text"
      id="activity-text-error"
      class="filter-error"
      role="alert"
    >
      {{ filterForm.errors.value.text }}
    </p>
    <p
      v-if="filterForm.errors.value.actions"
      id="activity-action-error"
      class="filter-error"
      role="alert"
    >
      {{ filterForm.errors.value.actions }} The list shows the last valid filter.
    </p>

    <div
      class="chips"
      role="group"
      aria-label="Categories"
    >
      <span class="label">Category</span>
      <button
        v-for="c in CATEGORIES"
        :key="c"
        type="button"
        class="chip"
        :aria-pressed="filters.categories.includes(c)"
        @click="toggleCategory(c)"
      >
        {{ c }}
      </button>
      <span class="spacer" />
      <button
        type="button"
        class="btn ghost compact"
        @click="resetFilters"
      >
        Reset filters
      </button>
    </div>

    <div class="body">
      <div class="list">
        <p
          v-if="error"
          class="notice tone-danger"
          role="alert"
        >
          {{ error }}
        </p>
        <p
          v-else-if="!loading && events.length === 0"
          class="subtle empty"
        >
          No events match these filters.
        </p>
        <template
          v-for="g in groups"
          :key="g.day"
        >
          <h2 class="day">
            {{ g.day }}
          </h2>
          <ul class="events">
            <li
              v-for="e in g.events"
              :key="e.id"
            >
              <button
                type="button"
                class="event"
                :aria-pressed="selected?.id === e.id"
                @click="selected = e"
              >
                <span class="time mono">{{ formatTime(e.occurredAt) }}</span>
                <span class="cat">{{ e.category }}</span>
                <span class="what">
                  <span class="line"><span class="action mono">{{ e.action }}</span><span
                    v-if="e.target || connectionName(e.connectionId)"
                    class="target"
                  >{{ e.target ?? connectionName(e.connectionId) }}</span></span>
                  <span
                    v-if="summary(e)"
                    class="detail mono"
                  >{{ summary(e) }}</span>
                </span>
                <span
                  class="pill"
                  :class="OUTCOME[e.outcome].tone"
                ><span class="dot" />{{ OUTCOME[e.outcome].label }}</span>
                <span class="dur mono">{{ e.durationMs !== null ? formatDuration(e.durationMs) : '' }}</span>
              </button>
            </li>
          </ul>
        </template>
        <div
          v-if="nextCursor"
          class="more"
        >
          <button
            type="button"
            class="btn"
            :disabled="loading"
            @click="load(true)"
          >
            Load more
          </button>
        </div>
        <p
          v-if="loading"
          class="subtle empty"
          role="status"
        >
          Loading…
        </p>
      </div>

      <aside
        v-if="selected"
        class="side"
        aria-label="Event details"
      >
        <header>
          <div>
            <div class="mono">
              {{ selected.action }}
            </div>
            <div class="subtle">
              Category {{ selected.category }}
            </div>
          </div>
          <span class="spacer" />
          <button
            type="button"
            class="icon-btn"
            aria-label="Close details"
            @click="selected = null"
          >
            <Icon name="close" />
          </button>
        </header>
        <dl class="facts">
          <dt>Occurred</dt>
          <dd class="mono">
            {{ formatDateTime(selected.occurredAt) }}
          </dd>
          <dt>Outcome</dt>
          <dd>{{ OUTCOME[selected.outcome].label }}</dd>
          <template v-if="selected.connectionId">
            <dt>Connection</dt>
            <dd>{{ connectionName(selected.connectionId) }}</dd>
          </template>
          <template v-if="selected.target">
            <dt>Target</dt>
            <dd>{{ selected.target }}</dd>
          </template>
          <template v-if="selected.serverKey">
            <dt>Server key</dt>
            <dd class="mono">
              {{ selected.serverKey }}
            </dd>
          </template>
          <template v-if="selected.durationMs !== null">
            <dt>Duration</dt>
            <dd class="mono">
              {{ formatDuration(selected.durationMs) }}
            </dd>
          </template>
          <dt>Actor</dt>
          <dd class="mono">
            {{ selected.actorId }}
          </dd>
          <template v-if="selected.deviceId">
            <dt>Device</dt>
            <dd class="mono">
              {{ selected.deviceId }}
            </dd>
          </template>
        </dl>
        <div class="json-head">
          <span class="label">Details JSON</span>
          <span class="spacer" />
          <button
            type="button"
            class="icon-btn"
            aria-label="Copy details JSON"
            @click="copyDetails"
          >
            <Icon name="copy" />
          </button>
        </div>
        <pre class="json mono">{{ detailsJson }}</pre>
        <div class="side-actions">
          <button
            v-if="workspace.connection(selected.connectionId)"
            type="button"
            class="btn compact"
            @click="openConnection(selected.connectionId)"
          >
            Open connection
          </button>
        </div>
      </aside>
    </div>
  </section>
</template>

<style scoped>
.activity {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}

.head {
  display: flex;
  flex-shrink: 0;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
  padding: 16px 20px 12px;
}

h1 {
  margin: 0;
  font-size: 18px;
  font-weight: 600;
}

.head .subtle {
  font-size: 12px;
}

.spacer {
  flex: 1;
}

.filters {
  display: flex;
  flex-shrink: 0;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  padding: 0 20px 8px;
  font-size: 12px;
  color: var(--text-muted);
}

.search {
  display: flex;
  flex: 1;
  align-items: center;
  gap: 8px;
  min-width: 240px;
  height: 28px;
  padding: 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
}

.search:focus-within {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--focus-ring);
}

.search input {
  flex: 1;
  border: 0;
  background: transparent;
  outline: none;
}

.input.compact.action {
  width: 200px;
  height: 28px;
  font-family: var(--font-mono);
  font-size: 12px;
}

.input.compact.action[aria-invalid='true'] {
  border-color: var(--danger);
}

.filter-error {
  margin: 0;
  padding: 0 20px 8px;
  font-size: 12px;
  color: var(--danger-text);
}

.select.compact {
  width: auto;
  height: 28px;
  font-size: 12px;
}

.chips {
  display: flex;
  flex-shrink: 0;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
  padding: 0 20px 12px;
  border-bottom: 1px solid var(--border);
}

.chip {
  height: 24px;
  padding: 0 10px;
  border: 1px solid var(--border);
  border-radius: 12px;
  background: var(--surface);
  color: var(--text-muted);
  font-size: 12px;
  cursor: pointer;
}

.chip[aria-pressed='true'] {
  border-color: var(--accent);
  background: var(--accent-tint);
  color: var(--accent-hover);
}

.body {
  display: flex;
  flex: 1;
  min-height: 0;
}

.list {
  flex: 1;
  min-width: 0;
  overflow: auto;
}

.day {
  position: sticky;
  top: 0;
  z-index: 1;
  margin: 0;
  padding: 6px 20px;
  border-bottom: 1px solid var(--border-inner);
  background: var(--surface-raised);
  color: var(--text-muted);
  font-size: 12px;
  font-weight: 500;
}

.events {
  margin: 0;
  padding: 0;
  list-style: none;
}

.event {
  display: grid;
  grid-template-columns: 72px 90px 1fr auto 72px;
  align-items: center;
  gap: 12px;
  width: 100%;
  min-height: 48px;
  padding: 6px 20px;
  border: 0;
  border-bottom: 1px solid var(--border-inner);
  background: var(--surface);
  color: var(--text);
  text-align: left;
  cursor: pointer;
}

.event:hover {
  background: var(--zebra);
}

.event[aria-pressed='true'] {
  background: var(--accent-tint);
  box-shadow: inset 2px 0 0 var(--accent);
}

.time,
.dur {
  font-size: 12px;
  color: var(--text-muted);
}

.dur {
  text-align: right;
}

.cat {
  font-size: 12px;
  color: var(--text-subtle);
}

.what {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.line {
  display: flex;
  gap: 8px;
  min-width: 0;
}

.action {
  font-size: 12px;
  font-weight: 500;
}

.target {
  overflow: hidden;
  color: var(--text-muted);
  text-overflow: ellipsis;
  white-space: nowrap;
}

.detail {
  overflow: hidden;
  font-size: 11px;
  color: var(--text-subtle);
  text-overflow: ellipsis;
  white-space: nowrap;
}

.more {
  padding: 12px 20px;
}

.empty,
.notice {
  margin: 16px 20px;
}

.side {
  display: flex;
  flex-direction: column;
  width: 400px;
  min-height: 0;
  border-left: 1px solid var(--border);
  background: var(--surface);
  overflow: auto;
}

.side header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 12px 8px 12px 16px;
  border-bottom: 1px solid var(--border-inner);
}

.facts {
  display: grid;
  grid-template-columns: 100px 1fr;
  gap: 6px 12px;
  margin: 0;
  padding: 12px 16px;
  font-size: 12px;
}

.facts dt {
  color: var(--text-muted);
}

.facts dd {
  margin: 0;
  overflow-wrap: anywhere;
}

.json-head {
  display: flex;
  align-items: center;
  padding: 4px 8px 4px 16px;
}

.json {
  margin: 0 16px;
  padding: 12px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-panel);
  background: var(--ground);
  font-size: 12px;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.side-actions {
  display: flex;
  gap: 8px;
  padding: 12px 16px;
}
</style>
