<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, markRaw, nextTick, onBeforeUnmount, onMounted, reactive, ref, useId, watch } from 'vue';
import { presentError } from '../../api/errors';
import { KEY_TYPE_FILTERS, keyTypeName, keyTypeOfFilter, moduleTypeFilters, redisModules, redisScan } from '../../api/redis';
import type { BulkFilter, DbKeyspace, KeyEntry, RedisEditRequest } from '../../api/redis';
import { scanBinding, useTabs } from '../../composables/useTabs';
import type { KeysTab } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import { useWorkspace } from '../../composables/useWorkspace';
import { isLostSession } from '../../lib/connectFlow';
import { scanFormSchema, SCAN_COUNTS } from '../../lib/forms';
import { domId } from '../../lib/domId';
import { KeyIndex, MAX_SCANNED_KEYS } from '../../lib/keyIndex';
import type { KeyRow } from '../../lib/keyIndex';
import { redisErrorView } from '../../lib/redisErrors';
import { createdKeyType } from '../../lib/redisEdit';
import { entryForKey } from '../../lib/redisKeys';
import type { RedisErrorView } from '../../lib/redisErrors';
import { initCommandProblem } from '../../lib/redisInit';
import { requiredFields } from '../../lib/schemaRequired';
import { formatCount } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import InitCommandNotice from '../notices/InitCommandNotice.vue';
import RedisBulkTtlDialog from './RedisBulkTtlDialog.vue';
import RedisDbSelect from './RedisDbSelect.vue';
import RedisNewKeyDialog from './RedisNewKeyDialog.vue';
import RedisPlanDialog from './RedisPlanDialog.vue';
import { useKeyEdit } from './useKeyEdit';
import ValueViewer from './ValueViewer.vue';

/** Read-only key browser: SCAN with MATCH and TYPE, keys grouped by a delimiter, never KEYS. */
const props = defineProps<{ tab: KeysTab }>();

const tabs = useTabs();
const workspace = useWorkspace();
const uid = useId();
const profile = computed(() => workspace.connection(props.tab.connectionId));

/** The tab object in shared state, to write scan results to. */
function state(): KeysTab | null {
  const tab = tabs.tabs.find((t) => t.id === props.tab.id);
  return tab?.kind === 'keys' ? tab : null;
}

// The scan form. Its fields are a draft; the tab keeps the submitted scan,
// which "Load more" continues, so an edited but unsubmitted pattern never
// mixes into a running scan.
const { values, errors, defineField, handleSubmit } = useForm({
  validationSchema: toTypedSchema(scanFormSchema),
  initialValues: {
    pattern: props.tab.pattern,
    typeFilter: props.tab.typeFilter,
    delimiter: props.tab.delimiter,
    count: props.tab.count,
  },
});
const [pattern, patternAttrs] = defineField('pattern', { validateOnModelUpdate: false });
const [typeFilter] = defineField('typeFilter');
const [delimiter, delimiterAttrs] = defineField('delimiter');
const [count] = defineField('count');
const required = computed(() => requiredFields(scanFormSchema, values, ['pattern']));

// The delimiter only changes how keys are grouped, so it applies at once:
// the keys found so far are regrouped (this is the one full rebuild).
watch(delimiter, (value) => {
  const tab = state();
  if (!tab || value === undefined || value.length > 8 || value === tab.delimiter) return;
  tab.delimiter = value;
  tab.index = markRaw(new KeyIndex(value, tab.index.entries()));
  tab.keysVersion += 1;
});

const ui = reactive<{ scanning: boolean; error: RedisErrorView | null; retryFromStart: boolean; keyspace: DbKeyspace[] }>({
  scanning: false,
  error: null,
  retryFromStart: true,
  keyspace: [],
});

const sessionId = computed(() => props.tab.session?.id ?? null);
const currentDb = computed(() => props.tab.db ?? profile.value?.options.redisDb ?? 0);
const dbInfo = computed(() => ui.keyspace.find((d) => d.db === currentDb.value) ?? null);

/** One SCAN step with the submitted pattern, type and COUNT of the tab. */
async function scan(fromStart: boolean): Promise<void> {
  const tab = state();
  const session = sessionId.value;
  if (!tab || !session) return;
  // A cursor only continues the scan of the session and database that produced it.
  const binding = scanBinding(tab);
  const restart = fromStart || tab.scanFor !== binding;
  if (!restart && ui.scanning) return;
  // The token lives on the tab, not in this component: a remounted browser
  // (or a newer scan) makes every older answer stale.
  tab.scanToken += 1;
  const token = tab.scanToken;
  const isCurrent = (): boolean => state()?.scanToken === token;
  ui.scanning = true;
  ui.error = null;
  if (restart) {
    tab.index = markRaw(new KeyIndex(tab.delimiter));
    tab.keysVersion += 1;
    tab.capped = false;
    tab.cursor = null;
    tab.selected = null;
    tab.scanned = false;
    tab.scanFor = binding;
  }
  try {
    const page = await redisScan(session, tab.cursor ?? '0', tab.pattern, keyTypeOfFilter(tab.typeFilter), tab.count);
    if (!isCurrent()) return;
    const current = state();
    if (!current || current.scanFor !== binding || scanBinding(current) !== binding) return;
    const { capped } = current.index.add(page.keys);
    current.keysVersion += 1;
    current.capped = capped;
    // A page can hold fewer keys than COUNT (even none) while the cursor is not 0; keep going on "load more".
    // At the cap the scan stops: the user narrows the pattern instead.
    current.cursor = capped ? '0' : page.cursor;
    current.scanned = true;
  } catch (e: unknown) {
    if (!isCurrent()) return;
    const cmd = tabs.reportTabError(props.tab.id, e);
    if (!isLostSession(cmd)) {
      // MULTI_OPEN (a console MULTI block), a profile edited to another
      // server, and other refusals each say what to do next.
      ui.error = redisErrorView(cmd);
      ui.retryFromStart = restart;
    }
  } finally {
    if (isCurrent()) ui.scanning = false;
  }
}

const submitScan = handleSubmit((form) => {
  const tab = state();
  if (!tab) return;
  tab.pattern = form.pattern.trim();
  tab.typeFilter = form.typeFilter;
  tab.count = form.count;
  tab.delimiter = form.delimiter;
  void scan(true);
});

const done = computed(() => props.tab.scanned && props.tab.cursor === '0');

function start(): void {
  if (sessionId.value && !props.tab.scanned) void scan(true);
}

// A reconnect changes the session and the database in one go, and a
// database switch clears the keys: one watcher, so either starts one SCAN.
watch([sessionId, () => props.tab.db], start);
onMounted(start);

/**
 * The module key types this server stores (`redis_modules`), offered in the
 * type filter after the core types. A server whose report cannot be read
 * offers the core types only.
 */
const moduleTypes = ref<string[]>([]);
watch(
  sessionId,
  async (session) => {
    moduleTypes.value = [];
    if (!session) return;
    try {
      const report = await redisModules(session);
      if (sessionId.value === session) moduleTypes.value = report.keyTypes;
    } catch {
      // Core types only; the scan itself reports a lost session.
    }
  },
  { immediate: true },
);
const typeFilters = computed(() => {
  const offered = [...KEY_TYPE_FILTERS, ...moduleTypeFilters(moduleTypes.value)];
  // A filter the tab already has stays selectable (and visible) while the report loads.
  const current = props.tab.typeFilter;
  if (current !== '' && !offered.some((f) => f.value === current)) offered.push({ value: current, label: current === 'json' ? 'ReJSON-RL (JSON)' : current });
  return offered;
});

// Key tree: rows come from the incremental index; only the rows in view are rendered.

const OVERSCAN = 10;
/** Row height from the `--row-tree` token, read once the list is mounted. */
const rowHeight = ref(28);
const expanded = reactive(new Set<string>());
const rows = computed<KeyRow[]>(() => {
  // The index is a raw object updated in place; `keysVersion` tells when it changed.
  void props.tab.keysVersion;
  return props.tab.index.rows(expanded);
});
/** Row key to position, so focus and selection lookups are O(1). */
const rowIndex = computed(() => new Map(rows.value.map((r, i) => [r.node.key, i])));
const focused = ref<string | null>(null);
const tabStop = computed(() =>
  focused.value !== null && rowIndex.value.has(focused.value) ? focused.value : (rows.value[0]?.node.key ?? null),
);
/** The selected key, only while it belongs to the current session and database. */
const selectedEntry = computed(() => {
  void props.tab.keysVersion;
  if (props.tab.selected === null || props.tab.scanFor !== scanBinding(props.tab)) return null;
  const item = props.tab.index.get(props.tab.selected);
  return item?.openable ? item.entry : null;
});

const list = ref<HTMLElement | null>(null);
const scrollTop = ref(0);
const viewport = ref(480);

/**
 * Rows in view (plus overscan), and always the tab stop row: it stays
 * mounted when it scrolls out, so keyboard focus is never lost and there is
 * always exactly one row with tabindex 0. Rows are placed absolutely.
 */
const windowRows = computed(() => {
  const first = Math.max(0, Math.floor(scrollTop.value / rowHeight.value) - OVERSCAN);
  const last = Math.min(rows.value.length, Math.ceil((scrollTop.value + viewport.value) / rowHeight.value) + OVERSCAN);
  const shown: { row: KeyRow; index: number }[] = [];
  for (let i = first; i < last; i += 1) shown.push({ row: rows.value[i] as KeyRow, index: i });
  const stop = tabStop.value === null ? undefined : rowIndex.value.get(tabStop.value);
  if (stop !== undefined && (stop < first || stop >= last)) shown.push({ row: rows.value[stop] as KeyRow, index: stop });
  return shown;
});
const totalHeight = computed(() => rows.value.length * rowHeight.value);

function onScroll(): void {
  if (!list.value) return;
  scrollTop.value = list.value.scrollTop;
}

// The pane's real height, kept up to date, so tall panes have no blank rows
// and PageUp/PageDown move by what is visible.
let resizeObserver: ResizeObserver | null = null;
onMounted(() => {
  if (!list.value) return;
  const token = Number.parseFloat(getComputedStyle(list.value).getPropertyValue('--row-tree'));
  if (Number.isFinite(token) && token > 0) rowHeight.value = token;
  viewport.value = list.value.clientHeight || viewport.value;
  if (typeof ResizeObserver === 'undefined') return;
  resizeObserver = new ResizeObserver((entries) => {
    const height = entries[0]?.contentRect.height;
    if (height) viewport.value = height;
  });
  resizeObserver.observe(list.value);
});
onBeforeUnmount(() => {
  resizeObserver?.disconnect();
  // A scan still running in this instance must not land in the next one.
  const tab = state();
  if (tab && ui.scanning) {
    tab.scanToken += 1;
    // A first page that never arrived leaves the tab unscanned, so the next instance scans again.
    if (tab.cursor === null) tab.scanned = false;
  }
});

function rowId(key: string): string {
  return `${uid}-${domId(key)}`;
}

/** Scrolls a row into the rendered window, then focuses it. */
async function focusRow(key: string | null | undefined): Promise<void> {
  if (!key) return;
  focused.value = key;
  const at = rowIndex.value.get(key);
  const el = list.value;
  if (at !== undefined && el) {
    const top = at * rowHeight.value;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + rowHeight.value > el.scrollTop + el.clientHeight) el.scrollTop = top + rowHeight.value - el.clientHeight;
    onScroll();
  }
  await nextTick();
  document.getElementById(rowId(key))?.focus();
}

function activate(row: KeyRow): void {
  if (row.node.kind === 'prefix') {
    if (expanded.has(row.node.key)) expanded.delete(row.node.key);
    else expanded.add(row.node.key);
    return;
  }
  // Keys whose name was too large to read cannot be opened.
  if (!row.node.item.openable) return;
  const tab = state();
  if (tab) tab.selected = row.node.item.id;
}

function onKeydown(event: KeyboardEvent, row: KeyRow): void {
  const at = rowIndex.value.get(row.node.key) ?? 0;
  const page = Math.max(1, Math.floor(viewport.value / rowHeight.value) - 1);
  switch (event.key) {
    case 'ArrowDown':
      void focusRow(rows.value[Math.min(at + 1, rows.value.length - 1)]?.node.key);
      break;
    case 'ArrowUp':
      void focusRow(rows.value[Math.max(at - 1, 0)]?.node.key);
      break;
    case 'PageDown':
      void focusRow(rows.value[Math.min(at + page, rows.value.length - 1)]?.node.key);
      break;
    case 'PageUp':
      void focusRow(rows.value[Math.max(at - page, 0)]?.node.key);
      break;
    case 'Home':
      void focusRow(rows.value[0]?.node.key);
      break;
    case 'End':
      void focusRow(rows.value.at(-1)?.node.key);
      break;
    case 'ArrowRight':
      if (row.node.kind !== 'prefix') return;
      if (!row.expanded) expanded.add(row.node.key);
      else void focusRow(rows.value[at + 1]?.node.key);
      break;
    case 'ArrowLeft':
      if (row.node.kind === 'prefix' && row.expanded) expanded.delete(row.node.key);
      else void focusRow(row.parentKey);
      break;
    case 'Enter':
    case ' ':
      activate(row);
      break;
    default:
      return;
  }
  event.preventDefault();
}

const TYPE_LABELS: Record<string, string> = {
  string: 'STR',
  hash: 'HASH',
  list: 'LIST',
  set: 'SET',
  zset: 'ZSET',
  stream: 'STREAM',
  'ReJSON-RL': 'JSON',
  // Module types (shown when the server has the module).
  'TSDB-TYPE': 'TS',
  'MBbloom--': 'BLOOM',
  bloomfltr: 'BLOOM',
  MBbloomCF: 'CUCKOO',
  'CMSk-TYPE': 'CMS',
  'TopK-TYPE': 'TOPK',
  'TDIS-TYPE': 'TDIGEST',
  // The scan did not read the type; opening the key reads it (key info).
  unknown: '?',
};

// Key changes made here: follow them in the scanned keys without a new SCAN.

/** Replaces scanned keys: drops `removeName` (hex) and adds `entry`, then selects `entry`. */
function replaceKey(removeName: string | null, entry: KeyEntry | null): void {
  const tab = state();
  if (!tab) return;
  const kept = tab.index.entries().filter((e) => e.name !== removeName && e.name !== entry?.name);
  tab.index = markRaw(new KeyIndex(tab.delimiter, entry ? [...kept, entry] : kept));
  tab.keysVersion += 1;
  tab.selected = null;
  if (entry) {
    const id = `key:${entry.name}`;
    if (tab.index.get(id)?.openable) tab.selected = id;
  }
}

function onRenamed(newKey: string): void {
  const old = selectedEntry.value;
  if (!old) return;
  const entry = entryForKey(newKey, old.keyType, old.ttlMs);
  replaceKey(old.name, entry);
  useToasts().push('success', 'Key renamed', newKey, 3000);
}

function onDeleted(): void {
  const old = selectedEntry.value;
  if (!old) return;
  replaceKey(old.name, null);
  useToasts().push('success', 'Key deleted', old.display, 3000);
}

// New key
const readOnly = computed(() => props.tab.session?.readOnly ?? true);
const creating = ref(false);

/** The bulk TTL dialog works on the submitted filter (the keys the list shows), not on an edited draft. */
const bulkTtl = ref(false);
const bulkFilter = computed<BulkFilter>(() => ({
  pattern: props.tab.pattern,
  keyType: keyTypeOfFilter(props.tab.typeFilter),
}));

function onBulkClosed(changed: boolean): void {
  bulkTtl.value = false;
  // TTLs changed: scan again so the list shows them.
  if (changed) void scan(true);
}
/** The request of the new key being planned, to add it to the list once applied. */
let newKeyRequest: RedisEditRequest | null = null;
const createEdit = useKeyEdit({
  tabId: () => props.tab.id,
  sessionId: () => sessionId.value ?? '',
  connectionName: () => profile.value?.name ?? props.tab.title,
  done: (view) => {
    const request = newKeyRequest;
    newKeyRequest = null;
    if (view.tone === 'success' && request) {
      const ttl = request.edits.find((e) => e.op === 'expire');
      replaceKey(null, entryForKey(request.key, createdKeyType(request.edits), ttl?.op === 'expire' ? ttl.ttlMs : null));
      useToasts().push('success', 'Key created', request.key, 3000);
    } else useToasts().push(view.tone === 'danger' ? 'danger' : 'warning', view.title, view.message);
  },
});

/** The prefix of the selected key (up to its last delimiter), to start a new key's name with. */
const newKeyPrefix = computed(() => {
  const display = selectedEntry.value?.display ?? '';
  const at = props.tab.delimiter === '' ? -1 : display.lastIndexOf(props.tab.delimiter);
  return at < 0 ? '' : display.slice(0, at + props.tab.delimiter.length);
});

function onNewKey(request: RedisEditRequest | null): void {
  creating.value = false;
  if (!request) return;
  newKeyRequest = request;
  void createEdit.review(request);
}

const connectError = computed(() => (props.tab.connectError ? presentError(props.tab.connectError) : null));
/** A refused init command of the profile gets its own explanation and an "Edit connection" button. */
const initProblem = computed(() => (props.tab.connectError ? initCommandProblem(props.tab.connectError) : null));

function retryAfterError(): void {
  const view = ui.error;
  if (!view) return;
  if (view.action === 'reconnect') void tabs.reconnect(props.tab.id);
  else void scan(ui.retryFromStart);
}
</script>

<template>
  <div class="browser">
    <section
      class="keys"
      aria-label="Key browser"
    >
      <div class="db">
        <RedisDbSelect
          :tab="tab"
          @keyspace="ui.keyspace = $event"
        />
        <span
          v-if="dbInfo"
          class="subtle"
        >{{ formatCount(dbInfo.keys) }} keys · {{ formatCount(dbInfo.expires) }} with TTL</span>
        <span class="spacer" />
        <button
          type="button"
          class="btn ghost compact"
          :disabled="!sessionId || readOnly || createEdit.state.planning"
          :title="readOnly ? 'This connection is read-only.' : undefined"
          @click="creating = true"
        >
          <Icon name="plus" />New key
        </button>
        <button
          type="button"
          class="btn ghost compact"
          :disabled="!sessionId || readOnly || !tab.scanned"
          :title="readOnly ? 'This connection is read-only.' : 'Set or remove the TTL of every key that matches the filter'"
          @click="bulkTtl = true"
        >
          Bulk TTL
        </button>
        <EnvBadge
          v-if="profile"
          :environment="profile.environment"
        />
      </div>

      <form
        class="filters"
        novalidate
        @submit.prevent="submitScan"
      >
        <div class="match-field">
          <label
            class="match"
            :class="{ invalid: !!errors.pattern }"
            :for="`${uid}-match`"
          >
            <span class="label">MATCH<span
              v-if="required.has('pattern')"
              class="required"
              aria-hidden="true"
            > *</span></span>
            <input
              :id="`${uid}-match`"
              v-model="pattern"
              v-bind="patternAttrs"
              class="mono"
              spellcheck="false"
              autocomplete="off"
              :aria-required="required.has('pattern')"
              :aria-invalid="!!errors.pattern"
              :aria-describedby="errors.pattern ? `${uid}-match-error` : undefined"
            >
          </label>
          <span
            v-if="errors.pattern"
            :id="`${uid}-match-error`"
            class="error"
            role="alert"
          >{{ errors.pattern }}</span>
        </div>
        <label
          class="visually-hidden"
          :for="`${uid}-type`"
        >Type filter</label>
        <select
          :id="`${uid}-type`"
          v-model="typeFilter"
          class="select compact"
        >
          <option value="">
            All types
          </option>
          <option
            v-for="t in typeFilters"
            :key="t.value"
            :value="t.value"
          >
            {{ t.label }}
          </option>
        </select>
        <div class="row2">
          <label :for="`${uid}-delim`">Delimiter</label>
          <input
            :id="`${uid}-delim`"
            v-model="delimiter"
            v-bind="delimiterAttrs"
            class="input compact mono delim"
            spellcheck="false"
            :aria-invalid="!!errors.delimiter"
            :aria-describedby="errors.delimiter ? `${uid}-delim-error` : undefined"
          >
          <label :for="`${uid}-count`">COUNT</label>
          <select
            :id="`${uid}-count`"
            v-model.number="count"
            class="select compact count"
          >
            <option
              v-for="c in SCAN_COUNTS"
              :key="c"
              :value="c"
            >
              {{ c }}
            </option>
          </select>
          <span class="spacer" />
          <button
            type="submit"
            class="btn compact"
            :disabled="!sessionId"
          >
            <Icon name="refresh" />Scan
          </button>
        </div>
        <span
          v-if="errors.delimiter"
          :id="`${uid}-delim-error`"
          class="error delim-error"
          role="alert"
        >{{ errors.delimiter }}</span>
      </form>

      <div
        class="progress"
        role="status"
      >
        <template v-if="tab.status === 'connecting'">
          Connecting…
        </template>
        <template v-else-if="ui.scanning">
          Scanning… {{ formatCount(tab.index.size) }} keys so far
        </template>
        <template v-else-if="tab.capped">
          Stopped at {{ formatCount(MAX_SCANNED_KEYS) }} keys. Narrow the MATCH pattern to see the rest.
        </template>
        <template v-else-if="done">
          SCAN complete · {{ formatCount(tab.index.size) }} keys
        </template>
        <template v-else-if="tab.scanned">
          {{ formatCount(tab.index.size) }} keys so far · cursor <span class="mono">{{ tab.cursor }}</span>
        </template>
      </div>

      <div
        v-if="ui.error"
        class="notice tone-danger state scan-error"
        role="alert"
      >
        <div class="error-body">
          <strong>{{ ui.error.title }}</strong>
          <span>{{ ui.error.message }}</span>
          <span v-if="ui.error.hint">{{ ui.error.hint }}</span>
        </div>
        <button
          v-if="ui.error.action"
          type="button"
          class="btn compact"
          :disabled="ui.scanning"
          @click="retryAfterError"
        >
          {{ ui.error.action === 'reconnect' ? 'Reconnect' : 'Try again' }}
        </button>
      </div>
      <div
        v-if="createEdit.state.error && !createEdit.state.planned"
        class="notice tone-danger state scan-error"
        role="alert"
      >
        <div class="error-body">
          <strong>Could not create the key: {{ createEdit.state.error.title }}</strong>
          <span>{{ createEdit.state.error.message }}</span>
          <span v-if="createEdit.state.error.hint">{{ createEdit.state.error.hint }}</span>
        </div>
      </div>
      <InitCommandNotice
        v-if="tab.connectError && initProblem"
        class="notice-block"
        :error="tab.connectError"
        :connection-id="tab.connectionId"
        @edited="tabs.reconnect(tab.id)"
      />
      <div
        v-else-if="connectError"
        class="notice tone-danger state"
        role="alert"
      >
        <span>{{ connectError.title }}: {{ connectError.message }}</span>
        <button
          type="button"
          class="btn compact"
          @click="tabs.reconnect(tab.id)"
        >
          Connect again
        </button>
      </div>

      <ul
        ref="list"
        class="tree"
        role="tree"
        aria-label="Keys by prefix"
        @scroll="onScroll"
      >
        <li
          class="spacer-row"
          role="none"
          aria-hidden="true"
          :style="{ height: `${totalHeight}px` }"
        />
        <li
          v-for="{ row, index } in windowRows"
          :id="rowId(row.node.key)"
          :key="row.node.key"
          role="treeitem"
          class="row"
          :class="{ oversized: row.node.kind === 'key' && !row.node.item.openable }"
          :aria-level="row.level"
          :aria-setsize="row.setSize"
          :aria-posinset="row.posInSet"
          :aria-expanded="row.node.kind === 'prefix' ? row.expanded : undefined"
          :aria-selected="row.node.kind === 'key' && row.node.item.id === tab.selected"
          :aria-disabled="row.node.kind === 'key' && !row.node.item.openable ? true : undefined"
          :title="row.node.kind === 'key' && !row.node.item.openable ? 'This key name is too large to read, so the key cannot be opened here.' : undefined"
          :tabindex="tabStop === row.node.key ? 0 : -1"
          :style="{ top: `${index * rowHeight}px`, paddingLeft: `${8 + (row.level - 1) * 16}px` }"
          @focus="focused = row.node.key"
          @click="focused = row.node.key; activate(row)"
          @keydown="onKeydown($event, row)"
        >
          <template v-if="row.node.kind === 'prefix'">
            <Icon
              :name="row.expanded ? 'chevronDown' : 'chevronRight'"
              :size="12"
              class="subtle"
            />
            <span class="name mono">{{ row.node.label }}</span>
            <span class="count mono">{{ formatCount(row.node.count) }}</span>
          </template>
          <template v-else>
            <span
              class="type mono"
              :title="row.node.item.entry.keyType === 'unknown' ? 'Type not read yet; it is read when you open the key.' : undefined"
            >{{ TYPE_LABELS[keyTypeName(row.node.item.entry.keyType)] ?? keyTypeName(row.node.item.entry.keyType) }}</span>
            <span class="name mono">{{ row.node.label }}</span>
          </template>
        </li>
      </ul>

      <div class="more">
        <button
          v-if="tab.scanned && !done"
          type="button"
          class="btn compact"
          :disabled="ui.scanning"
          @click="scan(false)"
        >
          Load more (next {{ formatCount(tab.count) }})
        </button>
        <span
          v-if="tab.scanned"
          class="subtle"
        >Counts are from keys scanned so far.</span>
      </div>
    </section>

    <div class="detail">
      <ValueViewer
        v-if="selectedEntry && sessionId"
        :tab-id="tab.id"
        :session-id="sessionId"
        :entry="selectedEntry"
        :read-only="readOnly"
        :connection-name="profile?.name ?? tab.title"
        :can-open-tab="!!profile"
        @renamed="onRenamed"
        @deleted="onDeleted"
        @open-tab="profile && tabs.openRedisKeyTab(profile, selectedEntry.display, currentDb)"
      />
      <p
        v-else
        class="empty subtle"
      >
        Select a key to see its type, TTL and value.
      </p>
    </div>
    <RedisBulkTtlDialog
      v-if="bulkTtl && sessionId"
      :session-id="sessionId"
      :filter="bulkFilter"
      :connection-name="profile?.name ?? 'this connection'"
      @close="onBulkClosed"
    />
    <RedisNewKeyDialog
      v-if="creating"
      :prefix="newKeyPrefix"
      :json-available="true"
      @done="onNewKey"
    />
    <RedisPlanDialog
      v-if="createEdit.state.planned"
      :plan="createEdit.state.planned.plan"
      :applying="createEdit.state.applying"
      :error="createEdit.state.error"
      @apply="createEdit.apply()"
      @cancel="createEdit.cancel()"
    />
  </div>
</template>

<style scoped>
.browser {
  display: flex;
  height: 100%;
  min-height: 0;
}

.keys {
  display: flex;
  flex-direction: column;
  width: 400px;
  min-height: 0;
  border-right: 1px solid var(--border);
  background: var(--surface);
}

.db {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border-inner);
  font-size: 12px;
}

.spacer {
  flex: 1;
}

.filters {
  display: grid;
  grid-template-columns: 1fr 130px;
  gap: 8px;
  padding: 8px 12px;
  border-bottom: 1px solid var(--border-inner);
  font-size: 12px;
}

.match-field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}

.match.invalid {
  border-color: var(--danger);
}

.required {
  color: var(--danger);
}

.error {
  font-size: 11px;
  color: var(--danger-text);
}

.match {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 28px;
  padding: 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
}

.match:focus-within {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--focus-ring);
}

.match input {
  flex: 1;
  min-width: 0;
  border: 0;
  background: transparent;
  outline: none;
}

.compact {
  height: 28px;
  font-size: 12px;
}

.row2 {
  display: flex;
  grid-column: 1 / -1;
  align-items: center;
  gap: 6px;
  color: var(--text-muted);
}

.delim-error {
  grid-column: 1 / -1;
}

.delim {
  width: 48px;
}

.count.select {
  width: 80px;
}

.progress {
  min-height: 28px;
  padding: 6px 12px;
  font-size: 12px;
  color: var(--text-muted);
}

.notice {
  margin: 0 12px 8px;
}

.state {
  align-items: center;
}

.scan-error {
  justify-content: space-between;
}

.error-body {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.notice-block {
  margin: 0 12px 8px;
}

.tree {
  position: relative;
  flex: 1;
  margin: 0;
  padding: 0;
  overflow: auto;
  list-style: none;
}

.spacer-row {
  pointer-events: none;
}

.row {
  position: absolute;
  right: 0;
  left: 0;
  display: flex;
  align-items: center;
  gap: 6px;
  height: var(--row-tree);
  padding-right: 12px;
  cursor: default;
}

.row.oversized .name {
  color: var(--text-subtle);
  font-family: var(--font-sans);
  font-style: italic;
}

.row:hover {
  background: var(--surface-raised);
}

.row:focus {
  outline: none;
}

.row:focus-visible,
.row[aria-selected='true'] {
  background: var(--accent-tint);
  box-shadow: inset 2px 0 0 var(--accent);
}

.name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  font-size: 12px;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.count {
  font-size: 11px;
  color: var(--text-subtle);
}

.type {
  flex-shrink: 0;
  min-width: 42px;
  padding: 1px 4px;
  border-radius: 3px;
  background: var(--surface-raised);
  color: var(--text-muted);
  font-size: 10px;
  text-align: center;
}

.more {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 4px;
  padding: 8px 12px;
  border-top: 1px solid var(--border-inner);
  font-size: 11px;
}

.detail {
  flex: 1;
  min-width: 0;
}

.empty {
  padding: 24px;
}
</style>
