<script setup lang="ts">
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { writeFile, writeTextFile } from '@tauri-apps/plugin-fs';
import { computed, onBeforeUnmount, reactive, ref, shallowRef, triggerRef, watch } from 'vue';
import type { ShallowRef } from 'vue';
import { toCmdError } from '../../api/errors';
import { keyTypeName, MODULE_KIND_LABELS, moduleKindOf, redisKeyDigestCancel, redisKeyInfo, redisKeyVersion, redisReadValue, redisStreamGroupPlan } from '../../api/redis';
import type {
  FieldValue,
  KeyEdit,
  KeyEntry,
  KeyExpect,
  KeyInfo,
  KeyVersion,
  ListItem,
  ScoredMember,
  StreamEntry,
  StreamGroupChange,
  TsInfo,
  ValueData,
} from '../../api/redis';
import type { Value } from '../../api/sessions';
import { useTabs } from '../../composables/useTabs';
import { useDialogs } from '../../composables/useDialogs';
import { useToasts } from '../../composables/useToasts';
import { latestOnly } from '../../lib/async';
import { describeEdit, digestNote, EditDraft, hexBytes, wholeStringPolicy } from '../../lib/redisEdit';
import type { OutcomeView } from '../../lib/redisEdit';
import { redisErrorView } from '../../lib/redisErrors';
import { chunkEnd, isDeadCursor, joinStringChunks, MAX_VIEW_ENTRIES } from '../../lib/redisValue';
import { cellText, formatBytes, formatCount, formatTtl, fullText, ttlSeconds, isOversized, plainText, prettyJson } from '../../lib/values';
import Icon from '../common/Icon.vue';
import RedisHashEditor from './RedisHashEditor.vue';
import RedisKeyDialog from './RedisKeyDialog.vue';
import RedisListEditor from './RedisListEditor.vue';
import RedisPlanDialog from './RedisPlanDialog.vue';
import RedisProbabilisticView from './RedisProbabilisticView.vue';
import RedisSetEditor from './RedisSetEditor.vue';
import RedisStreamEditor from './RedisStreamEditor.vue';
import RedisStreamGroups from './RedisStreamGroups.vue';
import RedisStringEditor from './RedisStringEditor.vue';
import RedisTimeSeriesView from './RedisTimeSeriesView.vue';
import RedisZsetView from './RedisZsetView.vue';
import { useKeyEdit } from './useKeyEdit';

/**
 * One Redis key: type, TTL, size, encoding and its value, page by page, with
 * the type's editor. Edits are staged, planned (the plan is shown first) and
 * applied with checks that the key still holds what was read.
 */
const props = withDefaults(
  defineProps<{
    tabId: string;
    sessionId: string;
    entry: KeyEntry;
    readOnly?: boolean;
    connectionName?: string;
    /** Offer "Open in tab" (the key browser does; a key tab does not). */
    canOpenTab?: boolean;
  }>(),
  { readOnly: false, connectionName: 'this connection', canOpenTab: false },
);
const emit = defineEmits<{ renamed: [newKey: string]; deleted: []; openTab: [] }>();

const PAGE = 200;
const toasts = useToasts();

const info = shallowRef<KeyInfo | null>(null);
const missing = ref(false);
const error = ref<string | null>(null);
const loading = ref(false);
const nextCursor = ref<string | null>(null);
const total = ref<string | null>(null);

// Accumulated data per type.
const text = shallowRef<{ value: Value; truncated: boolean } | null>(null);
/** Bytes of the string loaded so far, from the backend's byte offsets. */
const loadedBytes = ref<bigint>(0n);
const hash = shallowRef<FieldValue[]>([]);
const list = shallowRef<ListItem[]>([]);
const members = shallowRef<Value[]>([]);
const zset = shallowRef<ScoredMember[]>([]);
const stream = shallowRef<StreamEntry[]>([]);
const json = shallowRef<Value | null>(null);
/** A time series' info; its samples are read by the series view (by time range). */
const series = shallowRef<TsInfo | null>(null);
/** A probabilistic key's info. */
const probabilistic = shallowRef<Extract<ValueData, { kind: 'probabilistic' }> | null>(null);
const kind = ref<ValueData['kind'] | null>(null);
const view = ref<'raw' | 'json'>('json');

/** The background digest read of the shown version, stopped when the key is read again or the viewer closes. */
let digestRead: { sessionId: string; id: string } | null = null;

function stopDigestRead(): void {
  if (digestRead) void redisKeyDigestCancel(digestRead.sessionId, digestRead.id).catch(() => undefined);
  digestRead = null;
}

onBeforeUnmount(stopDigestRead);

function reset(): void {
  info.value = null;
  stopDigestRead();
  version.value = null;
  versionError.value = null;
  missing.value = false;
  error.value = null;
  cursorExpired.value = false;
  nextCursor.value = null;
  total.value = null;
  text.value = null;
  loadedBytes.value = 0n;
  hash.value = [];
  list.value = [];
  members.value = [];
  zset.value = [];
  stream.value = [];
  json.value = null;
  series.value = null;
  probabilistic.value = null;
  kind.value = null;
}

/** Appends a page to a shallow list in place and tells Vue once, instead of copying the whole list. */
function appendInPlace<T>(target: ShallowRef<T[]>, page: readonly T[]): void {
  for (const item of page) target.value.push(item);
  triggerRef(target);
}

function append(data: ValueData): void {
  kind.value = data.kind;
  switch (data.kind) {
    case 'string':
      // Text and byte chunks never mix: once a chunk is bytes, the whole value is bytes.
      text.value = { value: joinStringChunks(text.value?.value ?? null, data.value), truncated: data.truncated };
      loadedBytes.value = chunkEnd(data.offset, data.value);
      break;
    case 'hash':
      appendInPlace(hash, data.entries);
      break;
    case 'list':
      appendInPlace(list, data.items);
      break;
    case 'set':
      appendInPlace(members, data.members);
      break;
    case 'zset':
      appendInPlace(zset, data.members);
      break;
    case 'stream':
      appendInPlace(stream, data.entries);
      break;
    case 'json':
      json.value = data.document;
      break;
    case 'timeSeries':
      // The series view pages by time range itself.
      series.value = data.info;
      nextCursor.value = null;
      break;
    case 'probabilistic':
      probabilistic.value = data;
      break;
  }
}

// Every load of a key gets a token; a slow answer for a previous key (or an
// older page request) is dropped instead of landing in the current view.
const requests = latestOnly();

const cursorExpired = ref(false);

function fail(e: unknown): void {
  // Marks a lost session, or a tab whose profile now points elsewhere (reconnect needed).
  const cmd = useTabs().reportTabError(props.tabId, e);
  if (isDeadCursor(cmd)) {
    // Page cursors are single use and expire; a dead one is never retried.
    nextCursor.value = null;
    cursorExpired.value = true;
    error.value = null;
    return;
  }
  // MULTI_OPEN (console transaction), reconnect needed and read-only
  // refusals carry a hint on what to do next.
  const view = redisErrorView(cmd);
  error.value = view.hint ? `${view.message} ${view.hint}` : view.message;
}

async function readPage(cursor: string, token = requests.next()): Promise<void> {
  loading.value = true;
  try {
    const chunk = await redisReadValue(props.sessionId, props.entry.display, { cursor, count: PAGE });
    if (!requests.isCurrent(token)) return;
    error.value = null;
    if (!chunk) {
      missing.value = true;
      return;
    }
    total.value = chunk.total;
    // The backend may stop a page early on its byte budget; a cursor means there is more.
    nextCursor.value = chunk.nextCursor;
    append(chunk.data);
  } catch (e: unknown) {
    if (requests.isCurrent(token)) fail(e);
  } finally {
    if (requests.isCurrent(token)) loading.value = false;
  }
}

async function load(): Promise<void> {
  const token = requests.next();
  reset();
  loading.value = true;
  try {
    const [keyInfo, keyVersion] = await Promise.all([
      redisKeyInfo(props.sessionId, props.entry.display),
      redisKeyVersion(props.sessionId, props.entry.display).catch((e: unknown) => {
        // Without a version the value is still shown; only editing is off.
        versionError.value = redisErrorView(toCmdError(e)).message;
        return null;
      }),
    ]);
    if (!requests.isCurrent(token)) {
      if (keyVersion?.digestRead) void redisKeyDigestCancel(props.sessionId, keyVersion.digestRead).catch(() => undefined);
      return;
    }
    version.value = keyVersion;
    if (keyVersion?.digestRead) digestRead = { sessionId: props.sessionId, id: keyVersion.digestRead };
    if (!keyInfo) {
      missing.value = true;
      loading.value = false;
      return;
    }
    info.value = keyInfo;
  } catch (e: unknown) {
    if (requests.isCurrent(token)) {
      fail(e);
      loading.value = false;
    }
    return;
  }
  await readPage('', token);
}


const loadedCount = computed(() => {
  switch (kind.value) {
    case 'hash':
      return hash.value.length;
    case 'list':
      return list.value.length;
    case 'set':
      return members.value.length;
    case 'zset':
      return zset.value.length;
    case 'stream':
      return stream.value.length;
    default:
      return null;
  }
});


/** Lists stop growing at a fixed number of entries, so the view stays responsive. */
const atCap = computed(() => (loadedCount.value ?? 0) >= MAX_VIEW_ENTRIES);

const stringOversized = computed(() => (text.value ? isOversized(text.value.value) : false));
const stringText = computed(() => (text.value ? (plainText(text.value.value) ?? '') : ''));
const stringJson = computed(() => (text.value?.value.t === 'text' && !text.value.truncated ? prettyJson(stringText.value) : null));

async function copyKey(): Promise<void> {
  try {
    await navigator.clipboard.writeText(props.entry.display);
    toasts.push('success', 'Key copied', '', 2000);
  } catch {
    toasts.push('warning', 'Could not copy to the clipboard');
  }
}

function sizeLabel(keyInfo: KeyInfo): string | null {
  if (keyInfo.length === null) return null;
  const type = keyTypeName(keyInfo.keyType);
  if (type === 'string') return `Length ${formatBytes(keyInfo.length)}`;
  return `${type === 'hash' ? 'Fields' : 'Entries'} ${formatCount(keyInfo.length)}`;
}

// Editing

const version = shallowRef<KeyVersion | null>(null);
const versionError = ref<string | null>(null);
const draft = reactive(new EditDraft()) as EditDraft;
const outcome = shallowRef<OutcomeView | null>(null);
/** Bumped after every apply so the stream groups panel reads again. */
const groupsRefresh = ref(0);
const zsetView = ref<InstanceType<typeof RedisZsetView> | null>(null);
/** A whole-key change in flight (rename, delete), to follow it in the browser once applied. */
let wholeKey: KeyEdit | null = null;

const keyEdit = useKeyEdit({
  tabId: () => props.tabId,
  sessionId: () => props.sessionId,
  connectionName: () => props.connectionName,
  done: (view) => {
    outcome.value = view;
    draft.clear();
    const change = wholeKey;
    wholeKey = null;
    if (view.tone === 'success' && change?.op === 'rename') {
      emit('renamed', change.newKey);
      return;
    }
    if (view.tone === 'success' && change?.op === 'delete') {
      emit('deleted');
      return;
    }
    groupsRefresh.value += 1;
    zsetView.value?.reload();
    void load();
  },
});

/** Why editing is off, or `null` when the key can be edited here. */
const editBlock = computed<string | null>(() => {
  if (props.readOnly) return 'This connection is read-only, so editing is off.';
  if (versionError.value !== null) return `Editing is off: ${versionError.value}`;
  if (!info.value || !version.value) return 'Editing is off until the key is read.';
  return null;
});
const editable = computed(() => editBlock.value === null && !missing.value);

// Staged edits count as unsaved work of the tab (closing it asks first).
watch(
  () => draft.size > 0,
  (unsaved) => useTabs().setUnsaved(props.tabId, unsaved),
  { immediate: true },
);

function expect(): KeyExpect | null {
  if (!info.value) return null;
  return { state: 'present', keyType: info.value.keyType, version: version.value };
}

function review(edits: KeyEdit[], whole: KeyEdit | null = null): void {
  const expected = expect();
  if (!expected) return;
  outcome.value = null;
  wholeKey = whole;
  void keyEdit.review({ key: props.entry.display, expect: expected, edits });
}

const draftProblem = computed(() => {
  void draft.size;
  return draft.size === 0 ? null : draft.problem();
});

function saveDraft(): void {
  if (draftProblem.value !== null) return;
  review([...draft.edits]);
}

function planGroupChange(change: StreamGroupChange): void {
  outcome.value = null;
  wholeKey = null;
  void keyEdit.reviewWith(() => redisStreamGroupPlan(props.sessionId, props.entry.display, change));
}

const keyDialog = ref<'rename' | 'ttl' | null>(null);
function onKeyDialog(edit: KeyEdit | null): void {
  keyDialog.value = null;
  if (edit) review([edit], edit.op === 'rename' ? edit : null);
}

function deleteKey(): void {
  const edit: KeyEdit = { op: 'delete' };
  review([edit], edit);
}

/** Why the string or JSON value cannot be edited as a whole here. */
const stringLock = computed<string | null>(() => {
  if (kind.value === 'string') {
    if (!text.value) return null;
    if (stringOversized.value) return 'This value cannot be read here; edit it in the console (SETRANGE, APPEND).';
    if (text.value.truncated) {
      return wholeStringPolicy(total.value ?? info.value?.length ?? null) === 'refuse'
        ? 'This value is too large to edit here; save it to a file, or use SETRANGE or APPEND in the console.'
        : 'Only the first part of this value is loaded. Load the whole value to edit it.';
    }
    if (text.value.value.t !== 'text') return 'Binary values cannot be edited here; use the console.';
    return null;
  }
  if (kind.value === 'json' && json.value && isOversized(json.value)) return 'This document is too large to edit as a whole; use JSON.SET with a path in the console.';
  return null;
});

watch(
  () => [props.sessionId, props.entry.name],
  () => {
    // Staged edits belong to the key (and session) they were made on.
    draft.clear();
    outcome.value = null;
    void load();
  },
  { immediate: true },
);

/** Reads the rest of a string chunk by chunk, so the whole value can be edited. */
const loadingWhole = ref(false);
/** What loading the whole string for editing may do, from its size (`STRING_*_BYTES`). */
const wholePolicy = computed(() => wholeStringPolicy(total.value ?? info.value?.length ?? null));

async function loadWholeString(): Promise<void> {
  if (loadingWhole.value || wholePolicy.value === 'refuse') return;
  if (wholePolicy.value === 'confirm') {
    const size = total.value ?? info.value?.length ?? null;
    const ok = await useDialogs().askConfirm({
      title: 'Load the whole value?',
      message: `This value is ${size === null ? 'of unknown size' : formatBytes(size)}. Loading it into the editor can make Varve slow while it is open.`,
      confirmLabel: 'Load',
      danger: false,
    });
    if (!ok) return;
  }
  loadingWhole.value = true;
  try {
    const key = props.entry.name;
    while (kind.value === 'string' && nextCursor.value !== null && error.value === null && !cursorExpired.value && props.entry.name === key) {
      await readPage(nextCursor.value);
    }
  } finally {
    loadingWhole.value = false;
  }
}

/**
 * Saves a string value to a file chunk by chunk (appending), so a value too
 * large for the editor never sits in memory whole. Text chunks are written
 * with `writeTextFile` (UTF-8), byte chunks (not valid UTF-8, sent as hex)
 * with `writeFile` as raw bytes, so the file holds exactly the value's bytes.
 */
const saving = ref(false);
async function saveValueToFile(): Promise<void> {
  if (saving.value) return;
  const path = await saveFileDialog({ defaultPath: 'value.txt' }).catch(() => null);
  if (!path) return;
  saving.value = true;
  try {
    let cursor = '';
    let first = true;
    for (;;) {
      const chunk = await redisReadValue(props.sessionId, props.entry.display, { cursor, count: PAGE });
      if (!chunk || chunk.data.kind !== 'string') {
        toasts.push('warning', 'The key changed while saving', 'It no longer exists or is no longer a string. The file is incomplete.');
        return;
      }
      const value = chunk.data.value;
      if (value.t === 'text') await writeTextFile(path, value.v, { append: !first });
      else if (value.t === 'bytes') await writeFile(path, hexBytes(value.v), { append: !first });
      else {
        // A placeholder (a slice too large to read) or another shape: never written as if it were data.
        toasts.push('warning', 'Part of the value could not be read', 'The file is incomplete. Save it with redis-cli --raw GET instead.', 12_000);
        return;
      }
      first = false;
      if (chunk.nextCursor === null) break;
      cursor = chunk.nextCursor;
    }
    toasts.push('success', 'Value saved', path, 3000);
  } catch (e: unknown) {
    toasts.pushError(e, 'Could not save the value');
  } finally {
    saving.value = false;
  }
}

/** The type pill: module types by their name, not the module's internal type name. */
const typeLabel = computed(() => {
  const type = info.value?.keyType ?? props.entry.keyType;
  const module = moduleKindOf(type);
  return module ? MODULE_KIND_LABELS[module].toUpperCase() : keyTypeName(type).toUpperCase();
});

/** Why rename, delete and trim cannot check this key for changes by others, if they cannot. */
const versionNote = computed(() => digestNote(version.value?.digestMissing ?? null));

const TYPE_TONE: Record<string, string> = {
  hash: 'tone-info',
  zset: 'tone-warning',
  list: 'tone-success',
  string: 'tone-neutral',
  stream: 'tone-info',
  set: 'tone-neutral',
  'ReJSON-RL': 'tone-info',
};
</script>

<template>
  <section
    class="viewer"
    :aria-label="`Key ${entry.display}`"
  >
    <header class="head">
      <span
        class="pill"
        :class="TYPE_TONE[keyTypeName(info?.keyType ?? entry.keyType)] ?? 'tone-neutral'"
      >{{ typeLabel }}</span>
      <h2 class="mono key">
        {{ entry.display }}
      </h2>
      <span class="spacer" />
      <template v-if="editable">
        <button
          type="button"
          class="btn ghost compact"
          :disabled="keyEdit.state.planning"
          @click="keyDialog = 'rename'"
        >
          Rename
        </button>
        <button
          type="button"
          class="btn ghost compact"
          :disabled="keyEdit.state.planning"
          @click="keyDialog = 'ttl'"
        >
          TTL
        </button>
        <button
          type="button"
          class="btn ghost compact danger-text"
          :disabled="keyEdit.state.planning"
          @click="deleteKey"
        >
          Delete
        </button>
      </template>
      <button
        v-if="canOpenTab"
        type="button"
        class="btn ghost compact"
        @click="emit('openTab')"
      >
        Open in tab
      </button>
      <button
        type="button"
        class="btn compact"
        @click="copyKey"
      >
        <Icon name="copy" />Copy key
      </button>
      <button
        type="button"
        class="icon-btn"
        aria-label="Reload key"
        :disabled="loading"
        @click="load"
      >
        <Icon name="refresh" />
      </button>
    </header>

    <dl
      v-if="info"
      class="facts"
    >
      <div>
        <dt>TTL</dt>
        <dd class="mono">
          {{ formatTtl(info.ttlMs) }}<template v-if="ttlSeconds(info.ttlMs) !== null">
            ({{ formatCount(String(ttlSeconds(info.ttlMs))) }} s)
          </template>
        </dd>
      </div>
      <div v-if="info.memoryBytes !== null">
        <dt>Memory</dt>
        <dd class="mono">
          {{ formatBytes(info.memoryBytes) }}
        </dd>
      </div>
      <div v-if="info.encoding">
        <dt>Encoding</dt>
        <dd class="mono">
          {{ info.encoding }}
        </dd>
      </div>
      <div v-if="sizeLabel(info)">
        <dt>Size</dt>
        <dd class="mono">
          {{ sizeLabel(info) }}
        </dd>
      </div>
    </dl>

    <div class="content">
      <p
        v-if="missing"
        class="notice tone-warning"
      >
        This key no longer exists. It may have expired or been deleted since the scan.
      </p>
      <div
        v-if="cursorExpired"
        class="notice tone-warning expired"
        role="alert"
      >
        <span>The page position expired, so the rest of this value cannot be read from here. What was loaded stays below.</span>
        <button
          type="button"
          class="btn compact"
          @click="load"
        >
          Reload from the start
        </button>
      </div>
      <!-- An error on a later page keeps what was loaded so far visible below it. -->
      <p
        v-if="error"
        class="notice tone-danger"
        role="alert"
      >
        {{ error }}
      </p>

      <div
        v-if="outcome"
        class="notice outcome"
        :class="`tone-${outcome.tone}`"
        role="status"
      >
        <div class="stack">
          <strong>{{ outcome.title }}</strong>
          <span>{{ outcome.message }}</span>
        </div>
        <button
          type="button"
          class="icon-btn"
          aria-label="Dismiss"
          @click="outcome = null"
        >
          <Icon name="close" />
        </button>
      </div>
      <div
        v-if="keyEdit.state.error && !keyEdit.state.planned"
        class="notice tone-danger outcome"
        role="alert"
      >
        <div class="stack">
          <strong>{{ keyEdit.state.error.title }}</strong>
          <span>{{ keyEdit.state.error.message }}</span>
          <span v-if="keyEdit.state.error.hint">{{ keyEdit.state.error.hint }}</span>
        </div>
      </div>
      <p
        v-if="editBlock && info && !missing && !readOnly"
        class="subtle edit-off"
      >
        {{ editBlock }}
      </p>
      <p
        v-if="editable && versionNote"
        class="notice tone-warning edit-off"
      >
        {{ versionNote }}
      </p>
      <div
        v-if="editable && kind === 'string' && text?.truncated && !stringOversized && nextCursor !== null"
        class="notice tone-neutral outcome"
      >
        <span v-if="wholePolicy === 'refuse'">
          This value is {{ formatBytes(total ?? info?.length ?? '0') }}, too large to edit here (the limit is 64 MiB). Save it to a file,
          or change it in the console with SETRANGE or APPEND.
        </span>
        <span v-else>Only the first {{ formatBytes(String(loadedBytes)) }}<template v-if="total !== null"> of {{ formatBytes(total) }}</template> are loaded. Load the whole value to edit it here.</span>
        <button
          v-if="wholePolicy === 'refuse'"
          type="button"
          class="btn compact"
          :disabled="saving"
          @click="saveValueToFile"
        >
          {{ saving ? 'Saving…' : 'Save value to file' }}
        </button>
        <button
          v-else
          type="button"
          class="btn compact"
          :disabled="loadingWhole || loading"
          @click="loadWholeString"
        >
          {{ loadingWhole ? 'Loading…' : 'Load the whole value' }}
        </button>
      </div>

      <template v-if="missing" />
      <template v-else-if="kind === 'string' && text">
        <div class="string-bar">
          <span class="subtle">{{ text.value.t === 'bytes' ? 'Binary value, shown as hex' : stringJson ? 'Valid JSON' : 'Text' }}</span>
          <span class="spacer" />
          <div
            v-if="stringJson"
            class="segmented"
            role="radiogroup"
            aria-label="View as"
          >
            <button
              type="button"
              role="radio"
              :aria-checked="view === 'json'"
              @click="view = 'json'"
            >
              JSON
            </button>
            <button
              type="button"
              role="radio"
              :aria-checked="view === 'raw'"
              @click="view = 'raw'"
            >
              Raw
            </button>
          </div>
        </div>
        <RedisStringEditor
          mode="string"
          :original="stringLock ? null : text.value"
          :locked="stringLock"
          :editable="editable"
          :busy="keyEdit.state.planning"
          @save="review([$event])"
        >
          <p
            v-if="stringOversized"
            class="notice tone-neutral placeholder"
          >
            {{ stringText }}. Read it in pieces in the console, for example with GETRANGE.
          </p>
          <pre
            v-else
            class="mono value"
          >{{ stringJson && view === 'json' ? stringJson : stringText }}</pre>
        </RedisStringEditor>
      </template>

      <p
        v-else-if="kind === 'json' && json && isOversized(json)"
        class="notice tone-neutral placeholder"
      >
        {{ cellText(json) }}. Read it in pieces in the console, for example with JSON.GET and a path.
      </p>
      <RedisStringEditor
        v-else-if="kind === 'json' && json"
        mode="json"
        :original="stringLock ? null : json"
        :locked="stringLock"
        :editable="editable"
        :busy="keyEdit.state.planning"
        @save="review([$event])"
      >
        <pre class="mono value">{{ json.t === 'text' ? (prettyJson(json.v) ?? json.v) : fullText(json) }}</pre>
      </RedisStringEditor>

      <RedisHashEditor
        v-else-if="kind === 'hash'"
        :entries="hash"
        :draft="draft"
        :editable="editable"
      />
      <RedisListEditor
        v-else-if="kind === 'list'"
        :items="list"
        :draft="draft"
        :editable="editable"
      />
      <RedisSetEditor
        v-else-if="kind === 'set'"
        :members="members"
        :draft="draft"
        :editable="editable"
      />
      <RedisZsetView
        v-else-if="kind === 'zset'"
        ref="zsetView"
        :tab-id="tabId"
        :session-id="sessionId"
        :key-name="entry.display"
        :ranked="zset"
        :draft="draft"
        :editable="editable"
      />
      <RedisTimeSeriesView
        v-else-if="kind === 'timeSeries' && series"
        :tab-id="tabId"
        :session-id="sessionId"
        :key-name="entry.display"
        :info="series"
        :editable="editable"
        :busy="keyEdit.state.planning"
        @review="review([$event])"
      />
      <RedisProbabilisticView
        v-else-if="kind === 'probabilistic' && probabilistic"
        :tab-id="tabId"
        :session-id="sessionId"
        :key-name="entry.display"
        :kind="probabilistic.moduleKind"
        :fields="probabilistic.fields"
        :top="probabilistic.top"
        :editable="editable"
        :busy="keyEdit.state.planning"
        @review="review([$event])"
      />
      <template v-else-if="kind === 'stream'">
        <RedisStreamEditor
          :entries="stream"
          :draft="draft"
          :editable="editable"
        />
        <RedisStreamGroups
          :tab-id="tabId"
          :session-id="sessionId"
          :key-name="entry.display"
          :editable="editable"
          :refresh="groupsRefresh"
          @plan="planGroupChange"
        />
      </template>

      <p
        v-if="loading"
        class="subtle"
        role="status"
      >
        Loading…
      </p>
    </div>

    <div
      v-if="draft.size > 0"
      class="savebar"
      role="region"
      aria-label="Unsaved changes"
    >
      <span class="count">{{ draft.size }} change{{ draft.size === 1 ? '' : 's' }}</span>
      <ul class="pending">
        <li
          v-for="(e, i) in draft.edits.slice(0, 6)"
          :key="i"
        >
          {{ describeEdit(e) }}<button
            type="button"
            class="icon-btn small"
            :aria-label="`Undo: ${describeEdit(e)}`"
            @click="draft.removeAt(i)"
          >
            <Icon name="close" />
          </button>
        </li>
        <li
          v-if="draft.size > 6"
          class="subtle"
        >
          and {{ draft.size - 6 }} more
        </li>
      </ul>
      <span
        v-if="draftProblem"
        class="error"
        role="alert"
      >{{ draftProblem }}</span>
      <span class="spacer" />
      <button
        type="button"
        class="btn compact"
        :disabled="keyEdit.state.planning"
        @click="draft.clear()"
      >
        Discard
      </button>
      <button
        type="button"
        class="btn primary compact"
        :disabled="keyEdit.state.planning || !!draftProblem"
        @click="saveDraft"
      >
        {{ keyEdit.state.planning ? 'Planning…' : 'Review and save' }}
      </button>
    </div>

    <RedisPlanDialog
      v-if="keyEdit.state.planned"
      :plan="keyEdit.state.planned.plan"
      :applying="keyEdit.state.applying"
      :error="keyEdit.state.error"
      @apply="keyEdit.apply()"
      @cancel="keyEdit.cancel()"
    />
    <RedisKeyDialog
      v-if="keyDialog"
      :mode="keyDialog"
      :key-name="entry.display"
      :has-ttl="info?.ttlMs !== null && info?.ttlMs !== undefined"
      @done="onKeyDialog"
    />

    <footer
      v-if="kind && !missing"
      class="foot"
    >
      <span
        v-if="kind === 'string' && text"
        class="subtle"
      >{{ text.truncated ? `First ${formatBytes(String(loadedBytes))} loaded` : 'Whole value loaded' }}<template v-if="total !== null"> of {{ formatBytes(total) }}</template></span>
      <span
        v-else-if="loadedCount !== null"
        class="subtle"
      >{{ formatCount(loadedCount) }}<template v-if="total !== null"> of {{ formatCount(total) }}</template> loaded</span>
      <span class="spacer" />
      <span
        v-if="nextCursor !== null && atCap"
        class="subtle"
      >Showing the first {{ formatCount(MAX_VIEW_ENTRIES) }} entries. Read the rest in the console.</span>
      <button
        v-else-if="nextCursor !== null"
        type="button"
        class="btn compact"
        :disabled="loading"
        @click="readPage(nextCursor)"
      >
        Load more
      </button>
    </footer>
  </section>
</template>

<style scoped>
.viewer {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-width: 0;
  background: var(--surface);
}

.head {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 10px;
  height: 48px;
  padding: 0 12px 0 16px;
  border-bottom: 1px solid var(--border-inner);
}

.key {
  margin: 0;
  overflow: hidden;
  font-size: 15px;
  font-weight: 500;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.spacer {
  flex: 1;
}

.facts {
  display: flex;
  flex-shrink: 0;
  flex-wrap: wrap;
  gap: 4px 24px;
  margin: 0;
  padding: 8px 16px;
  border-bottom: 1px solid var(--border-inner);
  font-size: 12px;
}

.facts div {
  display: flex;
  gap: 6px;
}

.facts dt {
  color: var(--text-muted);
}

.facts dd {
  margin: 0;
}

.content {
  flex: 1;
  min-height: 0;
  padding: 12px 16px;
  overflow: auto;
}

.string-bar {
  display: flex;
  align-items: center;
  margin-bottom: 8px;
  font-size: 12px;
}

.value {
  margin: 0;
  padding: 12px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-panel);
  background: var(--ground);
  font-size: 12px;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.table {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.table th {
  position: sticky;
  top: 0;
  height: 28px;
  padding: 0 10px;
  background: var(--surface-raised);
  color: var(--text-muted);
  font-weight: 500;
  text-align: left;
}

.table td {
  height: 24px;
  max-width: 480px;
  padding: 2px 10px;
  overflow: hidden;
  border-bottom: 1px solid var(--border-inner);
  text-overflow: ellipsis;
  white-space: nowrap;
}

.table tbody tr:nth-child(even) {
  background: var(--zebra);
}

.num,
.table td.v-num {
  text-align: right;
  font-variant-numeric: tabular-nums;
}

.table td.v-placeholder,
.pair .v-placeholder {
  color: var(--text-subtle);
  font-family: var(--font-sans);
  font-style: italic;
}

.expired {
  align-items: center;
  justify-content: space-between;
  margin-bottom: 8px;
}

.placeholder {
  margin: 0;
  font-style: italic;
}

.table td.v-null {
  color: var(--text-subtle);
  font-style: italic;
}

.nowrap {
  white-space: nowrap;
}

.pair {
  margin-right: 12px;
}

.foot {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 16px;
  border-top: 1px solid var(--border-inner);
  font-size: 12px;
}

.notice {
  margin: 0;
}

.outcome {
  align-items: flex-start;
  justify-content: space-between;
  margin-bottom: 8px;
}

.stack {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.edit-off {
  margin: 0 0 8px;
  font-size: 12px;
}

.danger-text {
  color: var(--danger-text);
}

.savebar {
  display: flex;
  flex-shrink: 0;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  padding: 8px 16px;
  border-top: 1px solid var(--border-inner);
  background: var(--warning-tint);
  font-size: 12px;
}

.savebar .count {
  font-weight: 500;
}

.pending {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 12px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.pending li {
  display: inline-flex;
  align-items: center;
  gap: 2px;
}

.icon-btn.small {
  width: 20px;
  height: 20px;
}

.error {
  color: var(--danger-text);
}
</style>
