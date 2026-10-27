<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { kindLabel } from '../../api/catalog';
import type { ObjectKind } from '../../api/catalog';
import { useTabs } from '../../composables/useTabs';
import type { OverviewTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import type { MenuItem } from '../../lib/menu';
import ContextMenu from '../common/ContextMenu.vue';
import Icon from '../common/Icon.vue';
import { defaultAction, isObjectAction, objectMenuItems, runObjectAction } from '../tree/objectActions';
import ObjectIcon from '../tree/ObjectIcon.vue';
import {
  effectiveKinds,
  filterRows,
  firstRowInView,
  formatBytes,
  formatRowCount,
  kindChips,
  rowNode,
  sortRows,
} from './overview';
import type { OverviewRow, SortColumn } from './overview';
import { overviewView } from './overviewStore';

/**
 * The objects of one schema (PostgreSQL) or database (MySQL, MariaDB) as a
 * table: name, kind, estimated rows, size and comment, with search, kind
 * filters and sorting. Enter or a double click opens a row like the tree
 * does. Rendered by WorkspaceView while the tab is connected, one
 * instance per tab (keyed by tab id). What the user did here is kept per
 * tab (`overviewStore`), so switching back restores it without loading again.
 */
const props = defineProps<{ tab: OverviewTab }>();
const tabs = useTabs();
const workspace = useWorkspace();

const profile = computed(() => workspace.connection(props.tab.connectionId));
const pg = computed(() => (props.tab.session?.engine ?? profile.value?.engine) === 'postgres');

// The tab never changes for one instance: WorkspaceView keys the view by tab id.
const view = overviewView(props.tab.id);
const loader = view.loader;
const state = computed(() => loader.state);
// Only rows read for the container in the picker, on this session, are ever shown.
const all = computed(() => loader.rows());
const error = computed(() => state.value.prepareError ?? state.value.loadError);
const busy = computed(() => state.value.preparing || state.value.loading);

// Search, kind filter and sort

const search = computed({
  get: () => view.search,
  set: (value: string) => {
    view.search = value;
  },
});
const menu = ref<{ x: number; y: number; row: OverviewRow } | null>(null);

const chips = computed(() => kindChips(all.value));
const shownKinds = computed(() => effectiveKinds(view.kinds, all.value));
const visible = computed(() => sortRows(filterRows(all.value, { text: view.search, kinds: shownKinds.value }), view.sort));

function toggleKind(kind: ObjectKind): void {
  const next = new Set(shownKinds.value);
  if (next.has(kind)) next.delete(kind);
  else next.add(kind);
  view.kinds = next;
}

function clearFilters(): void {
  view.kinds = new Set();
  view.search = '';
}

function sortBy(column: SortColumn): void {
  view.sort =
    view.sort.column === column
      ? { column, descending: !view.sort.descending }
      : { column, descending: column === 'rows' || column === 'size' };
}

function ariaSort(column: SortColumn): 'ascending' | 'descending' | 'none' {
  if (view.sort.column !== column) return 'none';
  return view.sort.descending ? 'descending' : 'ascending';
}

const COLUMNS: { id: SortColumn; label: string; numeric: boolean }[] = [
  { id: 'name', label: 'Name', numeric: false },
  { id: 'kind', label: 'Kind', numeric: false },
  { id: 'rows', label: 'Rows (est.)', numeric: true },
  { id: 'size', label: 'Size', numeric: true },
  { id: 'comment', label: 'Comment', numeric: false },
];

// Loading: the first time and on a new session (reconnect) everything is read; coming back to the
// tab reads nothing; a picker change lists the new container. F5 and Refresh read everything again.

watch(
  () => props.tab.session?.id,
  (id) => {
    if (!id || id === view.sessionId) return;
    view.sessionId = id;
    void loader.refresh();
  },
  { immediate: true },
);
// Immediate: the tab may come to the front with a container chosen elsewhere (the tree) while it
// was in the background. `load` reads nothing when that container is already loaded.
watch(
  () => JSON.stringify(loader.container()),
  () => {
    void loader.load();
  },
  { immediate: true },
);

// Virtual rows

const ROW_HEIGHT = 28;
const OVERSCAN = 10;
const body = ref<HTMLElement | null>(null);
const viewport = reactive({ top: 0, height: 0 });
let resizeObserver: ResizeObserver | null = null;

function measure(): void {
  if (!body.value) return;
  viewport.top = body.value.scrollTop;
  viewport.height = body.value.clientHeight;
}

function onScroll(): void {
  measure();
  if (body.value) view.scrollTop = body.value.scrollTop;
}

onMounted(async () => {
  // Back on this tab: where the user had scrolled to.
  await nextTick();
  if (body.value) body.value.scrollTop = view.scrollTop;
  measure();
  if (typeof ResizeObserver !== 'undefined' && body.value) {
    resizeObserver = new ResizeObserver(measure);
    resizeObserver.observe(body.value);
  }
});

onBeforeUnmount(() => resizeObserver?.disconnect());

const windowed = computed(() => {
  const height = viewport.height > 0 ? viewport.height : window.innerHeight;
  const start = Math.max(0, Math.floor(viewport.top / ROW_HEIGHT) - OVERSCAN);
  const end = Math.min(visible.value.length, Math.ceil((viewport.top + height) / ROW_HEIGHT) + OVERSCAN);
  return visible.value.slice(start, end).map((row, i) => ({ row, index: start + i }));
});

// Selection and keyboard. Focus never moves the list; a click selects the row clicked.

const activeIndex = computed(() => visible.value.findIndex((r) => r.key === view.activeKey));
/** The active row's element id, only while that row is rendered (aria-activedescendant must point at a real element). */
const activeDescendant = computed(() => {
  const at = activeIndex.value;
  return at >= 0 && windowed.value.some((w) => w.index === at) ? rowDomId(at) : undefined;
});

function rowDomId(index: number): string {
  return `${props.tab.id}-ov-${index}`;
}

/** Makes a row active; with `reveal`, scrolls just enough to show it (keyboard). */
function select(index: number, reveal: boolean): void {
  const row = visible.value[index];
  if (!row) return;
  view.activeKey = row.key;
  const el = body.value;
  if (!reveal || !el) return;
  const top = index * ROW_HEIGHT;
  if (top < el.scrollTop) el.scrollTop = top;
  else if (top + ROW_HEIGHT > el.scrollTop + el.clientHeight) el.scrollTop = top + ROW_HEIGHT - el.clientHeight;
  measure();
}

function open(row: OverviewRow): void {
  const p = profile.value;
  if (!p) return;
  const node = rowNode(props.tab.connectionId, row);
  const action = defaultAction(node);
  if (action) runObjectAction(action, node, p);
}

function onKeydown(event: KeyboardEvent): void {
  // Keys of the sort buttons and other controls inside the grid are theirs.
  if (event.target !== event.currentTarget) return;
  const count = visible.value.length;
  // Without an active row, the keys start at the first row in view, not at the top.
  const at = activeIndex.value >= 0 ? activeIndex.value : firstRowInView(body.value?.scrollTop ?? 0, ROW_HEIGHT, count);
  const fresh = activeIndex.value < 0;
  const page = Math.max(1, Math.floor((body.value?.clientHeight ?? ROW_HEIGHT * 10) / ROW_HEIGHT) - 1);
  switch (event.key) {
    case 'ArrowDown':
      select(fresh ? at : Math.min(at + 1, count - 1), true);
      break;
    case 'ArrowUp':
      select(fresh ? at : Math.max(at - 1, 0), true);
      break;
    case 'PageDown':
      select(Math.min(at + page, count - 1), true);
      break;
    case 'PageUp':
      select(Math.max(at - page, 0), true);
      break;
    case 'Home':
      select(0, true);
      break;
    case 'End':
      select(count - 1, true);
      break;
    case 'Enter': {
      const row = fresh ? undefined : visible.value[at];
      if (row) open(row);
      break;
    }
    case 'ContextMenu':
      if (!fresh) openMenuAt(at);
      break;
    case 'F10':
      if (!event.shiftKey) return;
      if (!fresh) openMenuAt(at);
      break;
    case 'F5':
      void loader.refresh();
      break;
    default:
      return;
  }
  event.preventDefault();
}

// Context menu (the tree's object actions)

const menuItems = computed<MenuItem[]>(() => {
  const row = menu.value?.row;
  if (!row) return [];
  return objectMenuItems(rowNode(props.tab.connectionId, row), state.value.capabilities).filter((item) => item.id !== 'refresh');
});

function openMenu(event: MouseEvent, row: OverviewRow, index: number): void {
  select(index, false);
  menu.value = { x: event.clientX, y: event.clientY, row };
}

function openMenuAt(index: number): void {
  const row = visible.value[index];
  if (!row) return;
  select(index, true);
  const rect = document.getElementById(rowDomId(index))?.getBoundingClientRect();
  menu.value = { x: (rect?.left ?? 0) + 24, y: (rect?.bottom ?? 0) - 4, row };
}

function onMenuSelect(id: string): void {
  const row = menu.value?.row;
  const p = profile.value;
  if (!row || !p || !isObjectAction(id)) return;
  runObjectAction(id, rowNode(props.tab.connectionId, row), p);
}

// Keep the active row valid when the list changes.
watch(visible, (list) => {
  if (view.activeKey !== null && !list.some((r) => r.key === view.activeKey)) view.activeKey = null;
  measure();
});

function onPick(event: Event): void {
  const value = (event.target as HTMLSelectElement).value;
  view.kinds = new Set();
  view.activeKey = null;
  tabs.setOverviewScope(props.tab.id, pg.value ? { schema: value } : { database: value });
}

const pickerLabel = computed(() => (pg.value ? 'Schema' : 'Database'));
const picked = computed(() => (pg.value ? props.tab.scope.schema : props.tab.scope.database) ?? '');
const filtered = computed(() => view.search.trim() !== '' || shownKinds.value.size > 0);
const summaryText = computed(() => {
  const shown = visible.value.length;
  const total = all.value.length;
  return shown === total ? `${total.toLocaleString()} objects` : `${shown.toLocaleString()} of ${total.toLocaleString()} objects`;
});

function kindText(kind: ObjectKind): string {
  return kindLabel(kind);
}
</script>

<template>
  <div class="overview">
    <div class="toolbar">
      <label class="picker">
        <span class="picker-label">{{ pickerLabel }}</span>
        <select
          class="select"
          :value="picked"
          :disabled="state.choices.length === 0"
          @change="onPick"
        >
          <option
            v-for="choice in state.choices"
            :key="choice.object.name"
            :value="choice.object.name"
          >
            {{ choice.object.name }}
          </option>
        </select>
      </label>
      <label
        class="search"
        :for="`${tab.id}-ov-search`"
      >
        <Icon name="search" />
        <span class="visually-hidden">Search names and comments</span>
        <input
          :id="`${tab.id}-ov-search`"
          v-model="search"
          type="search"
          placeholder="Search"
          autocomplete="off"
          spellcheck="false"
        >
      </label>
      <span
        class="count"
        role="status"
      >{{ busy ? 'Loading…' : summaryText }}</span>
      <button
        type="button"
        class="btn compact"
        :disabled="busy"
        @click="loader.refresh()"
      >
        <Icon name="refresh" />Refresh
      </button>
    </div>

    <div
      v-if="chips.length > 0"
      class="chips"
      role="group"
      aria-label="Show kinds"
    >
      <button
        v-for="chip in chips"
        :key="chip.kind"
        type="button"
        class="chip"
        :aria-pressed="shownKinds.has(chip.kind)"
        @click="toggleKind(chip.kind)"
      >
        {{ chip.label }} <span class="chip-count">{{ chip.count.toLocaleString() }}</span>
      </button>
      <button
        v-if="filtered"
        type="button"
        class="chip clear"
        @click="clearFilters"
      >
        Show all
      </button>
    </div>

    <p
      v-if="state.capabilitiesError"
      class="notice tone-warning error"
      role="status"
    >
      <Icon name="info" />Edit actions are not available: {{ state.capabilitiesError }}
    </p>
    <p
      v-if="error"
      class="notice tone-danger error"
      role="alert"
    >
      <Icon name="warning" />{{ error }}
    </p>

    <p
      v-if="!busy && !error && visible.length === 0"
      class="empty"
    >
      <template v-if="all.length === 0">
        No objects here.
      </template>
      <template v-else>
        Nothing matches the search or the kind filter.
        <button
          type="button"
          class="btn compact"
          @click="clearFilters"
        >
          Show all
        </button>
      </template>
    </p>
    <div
      class="grid"
      role="grid"
      tabindex="0"
      :aria-label="`Objects in ${picked || 'this connection'}`"
      :aria-rowcount="visible.length + 1"
      :aria-busy="busy"
      :aria-activedescendant="activeDescendant"
      @keydown="onKeydown"
    >
      <div
        class="row head"
        role="row"
        aria-rowindex="1"
      >
        <div
          v-for="column in COLUMNS"
          :key="column.id"
          role="columnheader"
          class="cell"
          :class="[column.id, { numeric: column.numeric }]"
          :aria-sort="ariaSort(column.id)"
        >
          <button
            type="button"
            class="sort"
            @click="sortBy(column.id)"
          >
            {{ column.label }}
            <Icon
              v-if="view.sort.column === column.id"
              :name="view.sort.descending ? 'chevronDown' : 'chevronRight'"
              :size="10"
            />
          </button>
        </div>
      </div>
      <div
        ref="body"
        class="body"
        @scroll="onScroll"
      >
        <div
          class="rows"
          :style="{ height: `${visible.length * ROW_HEIGHT}px` }"
        >
          <div
            v-for="{ row, index } in windowed"
            :id="rowDomId(index)"
            :key="row.key"
            role="row"
            class="row"
            :class="{ active: index === activeIndex, system: row.summary.system }"
            :aria-rowindex="index + 2"
            :aria-selected="index === activeIndex"
            :style="{ top: `${index * ROW_HEIGHT}px` }"
            :title="row.summary.detail ?? undefined"
            @click="select(index, false)"
            @dblclick="open(row)"
            @contextmenu.prevent="openMenu($event, row, index)"
          >
            <div
              role="gridcell"
              class="cell name"
            >
              <ObjectIcon :kind="row.kind" />
              <span class="text">{{ row.name }}</span>
              <span
                v-if="row.summary.flags.length > 0"
                class="flag"
              >{{ row.summary.flags[0] }}</span>
            </div>
            <div
              role="gridcell"
              class="cell kind"
            >
              {{ kindText(row.kind) }}
            </div>
            <div
              role="gridcell"
              class="cell rows-cell numeric"
            >
              {{ formatRowCount(row.rows) }}
            </div>
            <div
              role="gridcell"
              class="cell size numeric"
            >
              {{ row.size !== null ? formatBytes(row.size) : (row.rows === null ? (row.summary.detail ?? '') : '') }}
            </div>
            <div
              role="gridcell"
              class="cell comment"
            >
              {{ row.comment ?? '' }}
            </div>
          </div>
        </div>
      </div>
    </div>

    <ContextMenu
      v-if="menu && menuItems.length > 0"
      :items="menuItems"
      :x="menu.x"
      :y="menu.y"
      :label="`Actions for ${menu.row.name}`"
      @select="onMenuSelect"
      @close="menu = null"
    />
  </div>
</template>

<style scoped>
.overview {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--surface);
}

.toolbar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border);
}

.picker {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--text-muted);
}

.picker .select {
  height: 28px;
  min-width: 140px;
}

.search {
  display: flex;
  flex: 0 1 280px;
  align-items: center;
  gap: 6px;
  height: 28px;
  padding: 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
  color: var(--text-subtle);
}

.search:focus-within {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--focus-ring);
}

.search input {
  flex: 1;
  min-width: 0;
  border: 0;
  background: transparent;
  color: var(--text);
  outline: none;
}

.count {
  margin-left: auto;
  font-size: 12px;
  color: var(--text-subtle);
}

.chips {
  display: flex;
  flex-shrink: 0;
  flex-wrap: wrap;
  gap: 4px;
  padding: 6px 12px;
  border-bottom: 1px solid var(--border);
}

.chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 24px;
  padding: 0 10px;
  border: 1px solid var(--border);
  border-radius: 12px;
  background: var(--surface);
  color: var(--text-muted);
  font-size: 12px;
  cursor: pointer;
}

.chip.clear {
  border-style: dashed;
}

.chip[aria-pressed='true'] {
  border-color: var(--accent);
  background: var(--accent-tint);
  color: var(--text);
}

.chip-count {
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--text-subtle);
}

.error {
  flex-shrink: 0;
  align-items: center;
  margin: 8px 12px 0;
}

.grid {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
}

.row {
  display: grid;
  grid-template-columns: minmax(200px, 2fr) 140px 120px 110px minmax(160px, 3fr);
  align-items: center;
  height: 28px;
  font-size: 13px;
}

.row.head {
  flex-shrink: 0;
  border-bottom: 1px solid var(--border);
  background: var(--surface-raised);
  color: var(--text-muted);
  font-size: 12px;
}

.body {
  position: relative;
  flex: 1;
  min-height: 0;
  overflow: auto;
  outline: none;
}

.grid:focus {
  outline: none;
}

.grid:focus-visible {
  box-shadow: inset 0 0 0 2px var(--focus-ring);
}

.rows {
  position: relative;
}

.rows .row {
  position: absolute;
  right: 0;
  left: 0;
  border-bottom: 1px solid var(--border-inner);
  cursor: default;
  user-select: none;
}

.rows .row:hover {
  background: var(--surface-raised);
}

.rows .row.active {
  background: var(--accent-tint);
  box-shadow: inset 2px 0 0 var(--accent);
}

.rows .row.system .text {
  color: var(--text-muted);
}

.cell {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
  padding: 0 10px;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.cell.name {
  color: var(--text);
}

.cell.name .object-icon {
  color: var(--text-subtle);
}

.text {
  overflow: hidden;
  text-overflow: ellipsis;
}

.cell.kind,
.cell.comment {
  color: var(--text-muted);
}

.numeric {
  justify-content: flex-end;
  font-family: var(--font-mono);
  font-size: 12px;
  font-variant-numeric: tabular-nums;
}

.head .numeric {
  font-family: inherit;
}

.sort {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 0;
  border: 0;
  background: transparent;
  color: inherit;
  font: inherit;
  cursor: pointer;
}

.flag {
  flex-shrink: 0;
  padding: 0 4px;
  border: 1px solid var(--border);
  border-radius: 3px;
  font-size: 10px;
  line-height: 14px;
  color: var(--text-subtle);
}

.empty {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  margin: 0;
  padding: 12px;
  color: var(--text-subtle);
  font-size: 12px;
}
</style>
