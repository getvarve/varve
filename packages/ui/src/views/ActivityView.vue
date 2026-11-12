<script setup lang="ts">
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, useId, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { CATEGORIES, clearActivity, exportActivity, fetchActivity, OUTCOMES, statementOf } from '../api/activity';
import type { ActivityEvent, ActivityExportFormat, ActivityFilter, Category, Outcome } from '../api/activity';
import { presentError, toCmdError } from '../api/errors';
import Icon from '../components/common/Icon.vue';
import { showWorkspace } from '../components/tree/showWorkspace';
import { useDialogs } from '../composables/useDialogs';
import { useToasts } from '../composables/useToasts';
import { useWorkspace } from '../composables/useWorkspace';
import { useWorkspaceActions } from '../composables/useWorkspaceActions';
import { formatDateTime, formatDay } from '../lib/display';
import {
  actionFilterTitle,
  CATEGORY_ICONS,
  categoryFilterTitle,
  categoryTone,
  connectionFilterTitle,
  DEFAULT_SORT,
  eventTime,
  groupEvents,
  hiddenSortTitle,
  nextSort,
  OUTCOME_LABELS,
  outcomeFilterTitle,
  popoverTab,
  sortHiddenWhenNarrow,
  sortTitle,
} from '../lib/activityColumns';
import type { ActivitySort, ActivitySortKey } from '../lib/activityColumns';
import { parseActions } from '../lib/activityFilter';
import { activityFilterSchema } from '../lib/forms';
import type { ActivityFilterValues } from '../lib/forms';
import { formatCount, formatDuration, reindentJson } from '../lib/values';

/** The local activity log (design doc 9.6): filters, sorted by any column over the whole log, details, export and clear. */
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

/** The time range (toggle buttons) and the categories (the category column's filter); the other fields are a validated form. */
const filters = reactive<{ range: Range; categories: Category[] }>({ range: '7d', categories: [] });

/**
 * The order the column headers ask for. The backend sorts over every
 * matching event and pages in that order, so a click loads the list again
 * from the top and "Load more" continues the same order.
 */
const sort = ref<ActivitySort>(DEFAULT_SORT);

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
/** The order the shown events were loaded in; the header shows `sort` at once, the rows follow when the page arrives. */
const listedSort = ref<ActivitySort>(DEFAULT_SORT);

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
  const out: ActivityFilter = { limit: 100, order: { ...sort.value } };
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
/** The filter that produced `nextCursor`; "Load more" continues that filter and its order, never one chosen since. */
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
    listedSort.value = used.order ?? DEFAULT_SORT;
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
  () => [
    filters.range,
    filters.categories.join(','),
    formValues.value.outcome,
    formValues.value.connectionId,
    sort.value.column,
    sort.value.direction,
  ],
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

function resetFilters(): void {
  filters.range = '7d';
  filters.categories = [];
  filterForm.resetForm({ values: { text: '', outcome: '', connectionId: '', actions: '' } });
  void router.replace({ name: 'activity' });
}

// Display

function connectionName(id: string | null): string | null {
  if (!id) return null;
  return workspace.connection(id)?.name ?? 'Deleted connection';
}

// Sorting: a new order reloads the list from the first page (the watch above).

/** `key` is null for a column without sorting (the header passes its column's key as is). */
function sortBy(key: ActivitySortKey | null): void {
  if (key === null) return;
  sort.value = nextSort(sort.value, key);
}

/** Back to newest first (the mark a narrow list shows for a sort on a column it hides). */
function sortByDefault(): void {
  sort.value = DEFAULT_SORT;
}

function ariaSort(key: ActivitySortKey): 'ascending' | 'descending' | undefined {
  return sort.value.column === key ? sort.value.direction : undefined;
}

/** Day headings when the shown events are in time order; one group without a heading otherwise. */
const groups = computed(() => groupEvents(events.value, listedSort.value, formatDay));

/** A row's time, with its date above it when the list has no day headings. */
function rowTime(event: ActivityEvent): { date: string | null; time: string } {
  return eventTime(event.occurredAt, listedSort.value);
}

// Column filters: each funnel opens a popover over the same filter state the list loads with.

type FilterColumn = 'category' | 'action' | 'connection' | 'outcome';

/** The column header row. `filterLabel` names the filter for its funnel's accessible name. */
const COLUMNS: {
  cls: string;
  label: string;
  sort: ActivitySortKey | null;
  filter: FilterColumn | null;
  filterLabel: string;
}[] = [
  { cls: 'time', label: 'Time', sort: 'time', filter: null, filterLabel: '' },
  { cls: 'cat', label: 'Category', sort: null, filter: 'category', filterLabel: 'Filter by category' },
  { cls: 'what', label: 'Action and target', sort: 'action', filter: 'action', filterLabel: 'Filter by action' },
  { cls: 'conn', label: 'Connection', sort: 'connection', filter: 'connection', filterLabel: 'Filter by connection' },
  { cls: 'outcome', label: 'Outcome', sort: 'outcome', filter: 'outcome', filterLabel: 'Filter by outcome' },
  { cls: 'dur', label: 'Took', sort: 'duration', filter: null, filterLabel: '' },
];

const FILTER_TITLES: Record<FilterColumn, string> = {
  category: 'Category',
  action: 'Action',
  connection: 'Connection',
  outcome: 'Outcome',
};

const filterActive = computed<Record<FilterColumn, boolean>>(() => ({
  category: filters.categories.length > 0,
  // Typed text counts even while it is not valid yet: it is still there to fix or clear.
  action: formValues.value.actions.trim() !== '',
  connection: formValues.value.connectionId !== '',
  outcome: formValues.value.outcome !== '',
}));

const filterTitles = computed<Record<FilterColumn, string>>(() => {
  const v = formValues.value;
  return {
    category: categoryFilterTitle(filters.categories, CATEGORIES.length),
    action: filterForm.errors.value.actions
      ? 'Action: not a valid filter yet. Click to fix.'
      : actionFilterTitle(parseActions(v.actions)),
    connection: connectionFilterTitle(connectionName(v.connectionId || null)),
    outcome: outcomeFilterTitle(v.outcome as Outcome | ''),
  };
});

/** The connections to choose from, plus the filtered one when it no longer exists. */
const connectionChoices = computed(() => {
  const list = workspace.records.value.connections.map((c) => ({ id: c.id, name: c.name }));
  const current = formValues.value.connectionId;
  if (current && !list.some((c) => c.id === current)) list.push({ id: current, name: 'Deleted connection' });
  return list;
});

const openFilter = ref<FilterColumn | null>(null);
const popover = ref<HTMLElement | null>(null);
const popoverPos = ref({ left: 0, top: 0 });
let filterOpener: HTMLElement | null = null;

function clamp(value: number, min: number, max: number): number {
  return Math.max(min, Math.min(value, max));
}

async function toggleFilter(column: FilterColumn | null, event: MouseEvent): Promise<void> {
  if (column === null) return;
  if (openFilter.value === column) {
    closeFilter(true);
    return;
  }
  const button = event.currentTarget as HTMLElement;
  filterOpener = button;
  const rect = button.getBoundingClientRect();
  popoverPos.value = { left: rect.left, top: rect.bottom + 4 };
  openFilter.value = column;
  await nextTick();
  const el = popover.value;
  if (!el) return;
  // Under the funnel, kept inside the window; above it when there is no room below.
  const below = rect.bottom + 4;
  const fitsBelow = below + el.offsetHeight <= window.innerHeight - 4;
  popoverPos.value = {
    left: clamp(rect.left, 4, window.innerWidth - el.offsetWidth - 4),
    top: fitsBelow ? below : clamp(rect.top - 4 - el.offsetHeight, 4, window.innerHeight - el.offsetHeight - 4),
  };
  focusPopover();
}

/** Puts focus on the popover's first choice (the checked one of a radio group), or on the popover itself. */
function focusPopover(): void {
  const el = popover.value;
  if (!el) return;
  const first = el.querySelector<HTMLElement>('input:checked') ?? el.querySelector<HTMLElement>('input, button:not(:disabled)');
  (first ?? el).focus();
}

/**
 * "Show all categories" and "Any action" turn themselves off by their own
 * click, and a button that is off cannot keep focus: move it to the first
 * choice, so the keyboard stays in the popover.
 */
async function clearFilter(column: 'category' | 'action'): Promise<void> {
  if (column === 'category') filters.categories = [];
  else filterForm.setFieldValue('actions', '');
  await nextTick();
  focusPopover();
}

/** A press on the popover's padding or a legend would move focus to the page: keep it in the popover. */
function onPopoverMouseDown(event: MouseEvent): void {
  const el = popover.value;
  if (!el || !(event.target instanceof Element) || event.target.closest('input, label, button:not(:disabled)')) return;
  event.preventDefault();
  if (!el.contains(document.activeElement)) el.focus();
}

function closeFilter(returnFocus: boolean): void {
  openFilter.value = null;
  if (returnFocus && filterOpener?.isConnected) filterOpener.focus();
  filterOpener = null;
}

/** The popover's Tab stops in order. A radio group is one stop: its checked radio. */
function tabStops(): HTMLElement[] {
  const all = [...(popover.value?.querySelectorAll<HTMLElement>('input, button:not(:disabled)') ?? [])];
  return all.filter((el) => !(el instanceof HTMLInputElement && el.type === 'radio') || el.checked);
}

/**
 * The popover's keys, heard on the document while it is open: focus can be
 * outside it (a click on a button that is off, or one that turned itself
 * off, drops focus to the page in some engines) and the keys must still
 * work. Escape closes; Tab past either end closes too and goes back to the
 * funnel, as menus do; Tab with focus lost brings it back into the popover.
 */
function onDocumentKeydown(event: KeyboardEvent): void {
  const el = popover.value;
  if (!openFilter.value || !el) return;
  if (event.key === 'Escape') {
    event.preventDefault();
    // The popover is what Escape closes now, not what lies under it.
    event.stopPropagation();
    closeFilter(true);
    return;
  }
  if (event.key !== 'Tab') return;
  const list = tabStops();
  const active = document.activeElement;
  const at = active && el.contains(active) ? list.indexOf(active as HTMLElement) : null;
  const move = popoverTab(list.length, at, event.shiftKey);
  if (move === 'browser') return;
  event.preventDefault();
  if (move === 'close') closeFilter(true);
  else list[move.focus]?.focus();
}

function onDocumentPointerDown(event: PointerEvent): void {
  if (!openFilter.value) return;
  const target = event.target as Node;
  // The funnel's own click toggles; anything else outside the popover closes it.
  if (popover.value?.contains(target) || filterOpener?.contains(target)) return;
  closeFilter(false);
}

function onWindowResize(): void {
  if (openFilter.value) closeFilter(false);
}

onMounted(() => {
  document.addEventListener('pointerdown', onDocumentPointerDown, true);
  document.addEventListener('keydown', onDocumentKeydown, true);
  window.addEventListener('resize', onWindowResize);
});
onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', onDocumentPointerDown, true);
  document.removeEventListener('keydown', onDocumentKeydown, true);
  window.removeEventListener('resize', onWindowResize);
  clearTimeout(debounce);
});

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

const OUTCOME_TONES: Record<Outcome, string> = {
  success: 'tone-success',
  failure: 'tone-danger',
  cancelled: 'tone-neutral',
};

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
  showWorkspace(router);
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
    // The export is always newest first: it leaves the list's order out.
    const { limit: _limit, order: _order, ...exportFilter } = applied;
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
      <button
        type="button"
        class="btn ghost compact"
        title="Clears the search and every column filter, and sets the range back to 7 days"
        @click="resetFilters"
      >
        Reset filters
      </button>
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

    <div class="body">
      <div class="list">
        <!-- A header-only table: the rows below stay buttons, so the list keeps its keyboard and selection behaviour. -->
        <div
          class="cols-table"
          role="table"
          aria-label="Activity columns"
        >
          <div
            class="cols"
            role="row"
          >
            <span
              v-for="c in COLUMNS"
              :key="c.cls"
              role="columnheader"
              class="col"
              :class="c.cls"
              :aria-sort="c.sort ? ariaSort(c.sort) : undefined"
            >
              <button
                v-if="c.sort"
                type="button"
                class="sort"
                :class="{ sorted: sort.column === c.sort }"
                :title="sortTitle(c.sort, sort)"
                @click="sortBy(c.sort)"
              >
                <span class="sort-label">{{ c.label }}</span><Icon
                  v-if="sort.column === c.sort"
                  :name="sort.direction === 'ascending' ? 'chevronUp' : 'chevronDown'"
                  :size="10"
                />
              </button>
              <span
                v-else
                class="visually-hidden"
              >{{ c.label }}</span>
              <button
                v-if="c.filter"
                type="button"
                class="icon-btn funnel"
                :class="{ on: filterActive[c.filter] }"
                :aria-label="c.filterLabel"
                aria-haspopup="dialog"
                :aria-expanded="openFilter === c.filter"
                :title="filterTitles[c.filter]"
                @click="toggleFilter(c.filter, $event)"
              >
                <Icon
                  name="filter"
                  :size="12"
                />
              </button>
              <!-- The connection column drops out of a narrow list; its filter moves next to the action's. -->
              <button
                v-if="c.cls === 'what'"
                type="button"
                class="icon-btn funnel narrow-only"
                :class="{ on: filterActive.connection }"
                aria-label="Filter by connection"
                aria-haspopup="dialog"
                :aria-expanded="openFilter === 'connection'"
                :title="filterTitles.connection"
                @click="toggleFilter('connection', $event)"
              >
                <Icon
                  name="plug"
                  :size="12"
                />
              </button>
              <!-- A narrow list hides the connection and duration columns: a sort on one of them shows here. -->
              <button
                v-if="c.cls === 'what' && sortHiddenWhenNarrow(sort)"
                type="button"
                class="icon-btn funnel on narrow-only"
                aria-label="Sort by time again"
                :title="hiddenSortTitle(sort)"
                @click="sortByDefault"
              >
                <Icon
                  :name="sort.direction === 'ascending' ? 'chevronUp' : 'chevronDown'"
                  :size="12"
                />
              </button>
            </span>
          </div>
        </div>
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
          :key="g.day ?? 'sorted'"
        >
          <h2
            v-if="g.day !== null"
            class="day"
          >
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
                <span
                  class="time mono"
                  :class="{ dated: rowTime(e).date !== null }"
                ><span
                  v-if="rowTime(e).date !== null"
                  class="date"
                >{{ rowTime(e).date }}</span><span>{{ rowTime(e).time }}</span></span>
                <span
                  class="cat"
                  :class="`tone-${categoryTone(e.outcome)}`"
                  :title="e.category"
                >
                  <Icon :name="CATEGORY_ICONS[e.category]" />
                  <span class="visually-hidden">{{ e.category }}</span>
                </span>
                <span class="what">
                  <span class="line"><span class="action mono">{{ e.action }}</span><span
                    v-if="e.target"
                    class="target"
                  >{{ e.target }}</span><span
                    v-else-if="connectionName(e.connectionId)"
                    class="target narrow-only"
                  >{{ connectionName(e.connectionId) }}</span></span>
                  <span
                    v-if="summary(e)"
                    class="detail mono"
                  >{{ summary(e) }}</span>
                </span>
                <span class="conn">{{ connectionName(e.connectionId) ?? '' }}</span>
                <span class="outcome"><span
                  class="pill"
                  :class="OUTCOME_TONES[e.outcome]"
                ><span class="dot" />{{ OUTCOME_LABELS[e.outcome] }}</span></span>
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
          <span
            class="cat"
            :class="`tone-${categoryTone(selected.outcome)}`"
          >
            <Icon :name="CATEGORY_ICONS[selected.category]" />
          </span>
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
          <dd>{{ OUTCOME_LABELS[selected.outcome] }}</dd>
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

    <!-- The open column filter. Choices apply at once, like the search; Escape or a click outside closes it. -->
    <Teleport to="body">
      <div
        v-if="openFilter"
        ref="popover"
        class="filter-pop"
        role="dialog"
        :aria-label="`${FILTER_TITLES[openFilter]} filter`"
        :style="{ left: `${popoverPos.left}px`, top: `${popoverPos.top}px` }"
        tabindex="-1"
        @mousedown="onPopoverMouseDown"
      >
        <fieldset v-if="openFilter === 'outcome'">
          <legend>Outcome</legend>
          <label class="opt">
            <input
              v-model="outcome"
              type="radio"
              :name="`${uid}-outcome`"
              value=""
            >Any outcome
          </label>
          <label
            v-for="o in OUTCOMES"
            :key="o"
            class="opt"
          >
            <input
              v-model="outcome"
              type="radio"
              :name="`${uid}-outcome`"
              :value="o"
            >{{ OUTCOME_LABELS[o] }}
          </label>
        </fieldset>

        <fieldset v-else-if="openFilter === 'connection'">
          <legend>Connection</legend>
          <div class="opts">
            <label class="opt">
              <input
                v-model="connectionId"
                type="radio"
                :name="`${uid}-conn`"
                value=""
              >All connections
            </label>
            <label
              v-for="c in connectionChoices"
              :key="c.id"
              class="opt"
            >
              <input
                v-model="connectionId"
                type="radio"
                :name="`${uid}-conn`"
                :value="c.id"
              ><span class="opt-text">{{ c.name }}</span>
            </label>
          </div>
        </fieldset>

        <fieldset v-else-if="openFilter === 'category'">
          <legend>Category</legend>
          <div class="opts">
            <label
              v-for="c in CATEGORIES"
              :key="c"
              class="opt"
            >
              <input
                v-model="filters.categories"
                type="checkbox"
                :value="c"
              ><span class="opt-icon"><Icon :name="CATEGORY_ICONS[c]" /></span>{{ c }}
            </label>
          </div>
          <div class="pop-actions">
            <button
              type="button"
              class="btn ghost compact"
              :disabled="filters.categories.length === 0"
              @click="clearFilter('category')"
            >
              Show all categories
            </button>
          </div>
        </fieldset>

        <div
          v-else
          class="action-filter"
        >
          <label
            class="legend"
            :for="`${uid}-action`"
          >Action</label>
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
          <p
            id="activity-action-help"
            class="help"
          >
            Separate actions with commas. End one with .* to match a prefix.
          </p>
          <div class="pop-actions">
            <button
              type="button"
              class="btn ghost compact"
              :disabled="!filterActive.action"
              @click="clearFilter('action')"
            >
              Any action
            </button>
          </div>
        </div>
      </div>
    </Teleport>
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
  font-size: var(--text-title);
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

.filter-error {
  margin: 0;
  padding: 0 20px 8px;
  font-size: 12px;
  color: var(--danger-text);
}

.body {
  display: flex;
  flex: 1;
  min-height: 0;
  border-top: 1px solid var(--border);
}

.list {
  flex: 1;
  min-width: 0;
  overflow: auto;
  container-type: inline-size;
}

/* Column header: stays on top while the list scrolls; the day headings stick under it. */
.cols-table {
  position: sticky;
  top: 0;
  z-index: 2;
}

.cols,
.event {
  display: grid;
  grid-template-columns: 72px 28px minmax(0, 1fr) 136px 112px 72px;
  align-items: center;
  gap: 12px;
  padding: 0 20px;
}

.cols {
  height: 28px;
  border-bottom: 1px solid var(--border);
  background: var(--sticky-surface);
}

.col {
  display: flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
}

.col.cat {
  justify-content: center;
}

.col.dur {
  justify-content: flex-end;
}

.sort {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
  height: 24px;
  padding: 0;
  border: 0;
  background: transparent;
  color: var(--text-muted);
  font-size: 11px;
  font-weight: 500;
  letter-spacing: 0.06em;
  text-transform: uppercase;
  white-space: nowrap;
  cursor: pointer;
}

/* A long header in a narrow column is cut with an ellipsis; its tooltip names the column. */
.sort-label {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
}

.sort .icon,
.sort svg {
  flex: none;
}

.sort.sorted,
.sort:hover {
  color: var(--text);
}

.icon-btn.funnel {
  flex: none;
  width: 22px;
  height: 22px;
}

.icon-btn.funnel.on,
.icon-btn.funnel.on:hover {
  color: var(--accent);
}

.narrow-only {
  display: none;
}

.day {
  position: sticky;
  top: 28px;
  z-index: 1;
  margin: 0;
  padding: 6px 20px;
  border-bottom: 1px solid var(--border-inner);
  background: var(--sticky);
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
  width: 100%;
  min-height: 48px;
  padding-top: 6px;
  padding-bottom: 6px;
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

/* The row touches the edges of the scrolling list: draw the ring inside it. */
.event:focus-visible {
  outline-offset: -2px;
}

/* Narrow list (details open in a small window): connection and duration stay in the details panel,
   in the header as in the rows. The connection filter moves next to the action's, and a row without
   a target names its connection there. */
@container (max-width: 520px) {
  .cols,
  .event {
    grid-template-columns: 72px 28px minmax(0, 1fr) 112px;
  }

  .conn,
  .dur {
    display: none;
  }

  .funnel.narrow-only {
    display: inline-flex;
  }

  .target.narrow-only {
    display: inline;
  }
}

.event[aria-pressed='true'] {
  background: var(--accent-tint);
  box-shadow: inset 2px 0 0 var(--accent);
}

.event .time,
.event .dur,
.event .conn {
  font-size: 12px;
  color: var(--text-muted);
}

.event .dur {
  text-align: right;
}

/* Sorted on another column there are no day headings: the date stands above the time. */
.event .time.dated {
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.event .time .date {
  color: var(--text-subtle);
  font-size: 11px;
}

.event .conn {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.event .outcome {
  display: flex;
  min-width: 0;
}

/* The category icon; its tone follows the outcome (tone-danger or tone-neutral from base.css). */
.event .cat,
.side .cat {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  justify-content: center;
  width: 28px;
  height: 28px;
  border-radius: var(--radius-panel);
}

.event .what {
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
  flex-shrink: 0;
  width: min(400px, 45%);
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
  background: var(--inset);
  font-size: 12px;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.side-actions {
  display: flex;
  gap: 8px;
  padding: 12px 16px;
}

/* Column filter popover (teleported to the body, like the context menu, and drawn the same way). */
.filter-pop {
  position: fixed;
  z-index: 200;
  min-width: 200px;
  max-width: 320px;
  padding: 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--overlay);
  box-shadow: var(--shadow-dialog), var(--glass-edge);
  color: var(--text);
  font-size: 13px;
  -webkit-backdrop-filter: var(--overlay-blur);
  backdrop-filter: var(--overlay-blur);
}

/* The popover itself only holds focus when none of its controls can (a press on its padding). */
.filter-pop:focus {
  outline: none;
}

.filter-pop fieldset {
  min-width: 0;
  margin: 0;
  padding: 0;
  border: 0;
}

.filter-pop legend,
.filter-pop .legend {
  display: block;
  padding: 0 6px 6px;
  color: var(--text-muted);
  font-size: 11px;
  font-weight: 500;
  letter-spacing: 0.06em;
  text-transform: uppercase;
}

.opts {
  display: flex;
  flex-direction: column;
  max-height: min(360px, 60vh);
  overflow: auto;
}

.opt {
  display: flex;
  align-items: center;
  gap: 8px;
  min-height: 28px;
  padding: 0 6px;
  border-radius: var(--radius-control);
  white-space: nowrap;
  cursor: pointer;
}

.opt:hover {
  background: var(--accent-tint);
}

.opt input {
  flex-shrink: 0;
  margin: 0;
  accent-color: var(--accent);
}

.opt-text {
  overflow: hidden;
  text-overflow: ellipsis;
}

.opt-icon {
  display: inline-flex;
  color: var(--text-muted);
}

.pop-actions {
  display: flex;
  justify-content: flex-end;
  margin-top: 6px;
  padding-top: 6px;
  border-top: 1px solid var(--border-inner);
}

.action-filter {
  display: flex;
  flex-direction: column;
  gap: 6px;
  width: 280px;
}

.action-filter .legend {
  padding-bottom: 0;
}

.input.compact.action {
  width: 100%;
  height: 28px;
  font-family: var(--font-mono);
  font-size: 12px;
}

.input.compact.action[aria-invalid='true'] {
  border-color: var(--danger);
}

.help {
  margin: 0;
  padding: 0 2px;
  color: var(--text-muted);
  font-size: 12px;
}
</style>
