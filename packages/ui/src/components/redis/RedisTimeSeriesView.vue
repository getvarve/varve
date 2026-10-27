<script setup lang="ts">
import { computed, reactive, shallowRef, useId, watch } from 'vue';
import { redisTsRange } from '../../api/redis';
import type { KeyEdit, TsInfo, TsRangeQuery, TsSample } from '../../api/redis';
import { useTabs } from '../../composables/useTabs';
import { latestOnly } from '../../lib/async';
import { textValue } from '../../lib/redisEdit';
import { redisErrorView } from '../../lib/redisErrors';
import { DUPLICATE_POLICIES, formatRetention, formatTimestamp, parseTimestamp, retentionShrinks, settingsForm, tsAlterEdit } from '../../lib/redisModules';
import { cellText, formatCount } from '../../lib/values';

/**
 * A time series (design doc 4.2, TimeSeries): `TS.INFO` facts, samples by
 * time range page by page (`TS.RANGE` / `TS.REVRANGE`), and the changes
 * Varve plans like every value edit: add a sample, delete a time range
 * (checked against the series' version), and the retention, labels and
 * duplicate policy (checked against what was read).
 */
const props = defineProps<{
  tabId: string;
  sessionId: string;
  keyName: string;
  info: TsInfo;
  editable: boolean;
  busy: boolean;
}>();
const emit = defineEmits<{ review: [edit: KeyEdit] }>();

const uid = useId();
const PAGE_SIZES = [50, 200, 1000] as const;

const range = reactive({ from: '', to: '', newestFirst: true, count: 200 as number });
const shown = reactive<{ loading: boolean; error: string | null; missing: boolean; next: string | null; query: TsRangeQuery | null }>({
  loading: false,
  error: null,
  missing: false,
  next: null,
  query: null,
});
const samples = shallowRef<TsSample[]>([]);
const requests = latestOnly();

const rangeProblem = computed(() => {
  const from = parseTimestamp(range.from);
  const to = parseTimestamp(range.to);
  if (from === null || to === null) return 'Use milliseconds or a date and time, for example 2026-10-21T13:45:00Z.';
  if (from !== undefined && to !== undefined && BigInt(from) > BigInt(to)) return 'The range starts after it ends.';
  return null;
});

function queryOf(cursor: string | null): TsRangeQuery | null {
  const from = parseTimestamp(range.from);
  const to = parseTimestamp(range.to);
  if (from === null || to === null) return null;
  return { from: from ?? null, to: to ?? null, reverse: range.newestFirst, count: range.count, cursor };
}

async function load(cursor: string | null): Promise<void> {
  const query = queryOf(cursor);
  if (!query || rangeProblem.value) return;
  const token = requests.next();
  shown.loading = true;
  try {
    const page = await redisTsRange(props.sessionId, props.keyName, query);
    if (!requests.isCurrent(token)) return;
    shown.error = null;
    shown.query = query;
    if (!page) {
      shown.missing = true;
      samples.value = [];
      shown.next = null;
      return;
    }
    shown.missing = false;
    samples.value = cursor === null ? page.samples : [...samples.value, ...page.samples];
    shown.next = page.next;
  } catch (e: unknown) {
    if (!requests.isCurrent(token)) return;
    const view = redisErrorView(useTabs().reportTabError(props.tabId, e));
    shown.error = view.hint ? `${view.message} ${view.hint}` : view.message;
  } finally {
    if (requests.isCurrent(token)) shown.loading = false;
  }
}

// Read again whenever the key is read again (after an edit, a reload).
watch(
  () => props.info,
  () => void load(null),
  { immediate: true },
);

const command = computed(() => {
  const q = shown.query;
  if (!q) return '';
  const lo = q.from ?? '-';
  const hi = q.to ?? '+';
  return q.reverse ? `TS.REVRANGE ${hi} ${lo} COUNT ${q.count}` : `TS.RANGE ${lo} ${hi} COUNT ${q.count}`;
});

// Changes

const add = reactive({ timestamp: '', value: '' });
const addProblem = computed(() => {
  if (parseTimestamp(add.timestamp) === null) return 'Leave the time empty for now, or use milliseconds or a date and time.';
  const v = add.value.trim().toLowerCase();
  if (v === '') return null;
  return v === 'nan' || Number.isFinite(Number(v)) ? null : 'The value is a number (or nan).';
});

function addSample(): void {
  if (addProblem.value || add.value.trim() === '') return;
  emit('review', { op: 'tsAdd', timestamp: parseTimestamp(add.timestamp) ?? null, value: textValue(add.value.trim()) });
}

const remove = reactive({ from: '', to: '' });
const removeProblem = computed(() => {
  const from = parseTimestamp(remove.from);
  const to = parseTimestamp(remove.to);
  if (from === null || to === null) return 'Use milliseconds or a date and time; leave a side empty for the first or last sample.';
  if (from !== undefined && to !== undefined && BigInt(from) > BigInt(to)) return 'The range starts after it ends.';
  return null;
});

function deleteRange(): void {
  if (removeProblem.value) return;
  emit('review', { op: 'tsDeleteRange', from: parseTimestamp(remove.from) ?? '-', to: parseTimestamp(remove.to) ?? '+' });
}

const settings = reactive(settingsForm(props.info.meta));
watch(
  () => props.info.meta,
  (meta) => Object.assign(settings, settingsForm(meta)),
);
const alter = computed(() => tsAlterEdit(props.info.meta, settings));
const shrinkWarning = computed(() =>
  retentionShrinks(props.info.meta.retentionMs, settings.retentionMs.trim())
    ? `Lowering the retention to ${formatRetention(settings.retentionMs.trim())} removes the samples older than that (counted back from the newest one).`
    : null,
);

function saveSettings(): void {
  const edit = alter.value.edit;
  if (edit) emit('review', edit);
}
</script>

<template>
  <div class="ts">
    <dl class="facts">
      <div>
        <dt>Samples</dt>
        <dd class="mono">
          {{ formatCount(info.totalSamples) }}
        </dd>
      </div>
      <div>
        <dt>First</dt>
        <dd class="mono">
          {{ info.firstTimestamp === null ? 'empty' : formatTimestamp(info.firstTimestamp) }}
        </dd>
      </div>
      <div>
        <dt>Last</dt>
        <dd class="mono">
          {{ info.lastTimestamp === null ? 'empty' : formatTimestamp(info.lastTimestamp) }}
        </dd>
      </div>
      <div>
        <dt>Retention</dt>
        <dd class="mono">
          {{ formatRetention(info.meta.retentionMs) }}
        </dd>
      </div>
      <div>
        <dt>Duplicates</dt>
        <dd class="mono">
          {{ info.meta.duplicatePolicy ?? 'server default' }}
        </dd>
      </div>
      <div v-if="info.sourceKey">
        <dt>Compacted from</dt>
        <dd class="mono">
          {{ info.sourceKey }}
        </dd>
      </div>
    </dl>
    <p
      v-if="info.meta.labels.length > 0"
      class="labels"
    >
      <span
        v-for="l in info.meta.labels"
        :key="l.name"
        class="pill tone-neutral mono"
      >{{ l.name }}={{ l.value }}</span>
    </p>
    <p
      v-if="info.rules.length > 0"
      class="subtle rules"
    >
      Compaction rules:
      <span
        v-for="r in info.rules"
        :key="r.destKey"
        class="mono"
      >{{ r.aggregation }} every {{ formatRetention(r.bucketMs) }} into {{ r.destKey }}; </span>
    </p>

    <p class="subtle note">
      Reading the settings (TS.INFO) counts as a change of this key for Redis: it ends a WATCH on it, also one of another client.
    </p>
    <form
      class="range"
      aria-label="Sample range"
      @submit.prevent="load(null)"
    >
      <label :for="`${uid}-from`">From</label>
      <input
        :id="`${uid}-from`"
        v-model="range.from"
        class="input compact mono stamp"
        placeholder="first sample"
        spellcheck="false"
        :aria-invalid="!!rangeProblem"
      >
      <label :for="`${uid}-to`">to</label>
      <input
        :id="`${uid}-to`"
        v-model="range.to"
        class="input compact mono stamp"
        placeholder="last sample"
        spellcheck="false"
        :aria-invalid="!!rangeProblem"
      >
      <label class="check"><input
        v-model="range.newestFirst"
        type="checkbox"
      > Newest first</label>
      <label :for="`${uid}-count`">page</label>
      <select
        :id="`${uid}-count`"
        v-model.number="range.count"
        class="select compact"
      >
        <option
          v-for="n in PAGE_SIZES"
          :key="n"
          :value="n"
        >
          {{ n }}
        </option>
      </select>
      <button
        type="submit"
        class="btn compact"
        :disabled="!!rangeProblem || shown.loading"
      >
        Show
      </button>
    </form>
    <span
      v-if="rangeProblem"
      class="error"
      role="alert"
    >{{ rangeProblem }}</span>
    <span class="mono subtle">{{ command }}</span>
    <p
      v-if="shown.error"
      class="notice tone-danger"
      role="alert"
    >
      {{ shown.error }}
    </p>
    <p
      v-else-if="shown.missing"
      class="notice tone-warning"
    >
      This key no longer exists.
    </p>
    <table
      v-if="samples.length > 0"
      class="table"
    >
      <thead>
        <tr>
          <th>Time (UTC)</th>
          <th>Milliseconds</th>
          <th class="num">
            Value
          </th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="s in samples"
          :key="s.timestamp"
        >
          <td class="mono">
            {{ formatTimestamp(s.timestamp) }}
          </td>
          <td class="mono subtle">
            {{ s.timestamp }}
          </td>
          <td class="mono num">
            {{ cellText(s.value) }}
          </td>
        </tr>
      </tbody>
    </table>
    <p
      v-else-if="!shown.loading && !shown.error && !shown.missing"
      class="subtle"
    >
      No samples in this range.
    </p>
    <div class="bar">
      <span class="subtle">{{ formatCount(String(samples.length)) }} shown</span>
      <span class="spacer" />
      <button
        type="button"
        class="btn compact"
        :disabled="shown.loading || shown.next === null"
        @click="load(shown.next)"
      >
        Load more
      </button>
    </div>

    <template v-if="editable">
      <form
        class="change"
        aria-label="Add a sample"
        @submit.prevent="addSample"
      >
        <strong>Add a sample</strong>
        <label :for="`${uid}-at`">at</label>
        <input
          :id="`${uid}-at`"
          v-model="add.timestamp"
          class="input compact mono stamp"
          placeholder="now (server time)"
          spellcheck="false"
        >
        <label :for="`${uid}-value`">value</label>
        <input
          :id="`${uid}-value`"
          v-model="add.value"
          class="input compact mono"
          inputmode="decimal"
          spellcheck="false"
        >
        <button
          type="submit"
          class="btn compact"
          :disabled="busy || !!addProblem || add.value.trim() === ''"
        >
          Review
        </button>
        <span
          v-if="addProblem"
          class="error"
          role="alert"
        >{{ addProblem }}</span>
      </form>
      <form
        class="change"
        aria-label="Delete samples"
        @submit.prevent="deleteRange"
      >
        <strong>Delete samples</strong>
        <label :for="`${uid}-dfrom`">from</label>
        <input
          :id="`${uid}-dfrom`"
          v-model="remove.from"
          class="input compact mono stamp"
          placeholder="first sample"
          spellcheck="false"
        >
        <label :for="`${uid}-dto`">to</label>
        <input
          :id="`${uid}-dto`"
          v-model="remove.to"
          class="input compact mono stamp"
          placeholder="last sample"
          spellcheck="false"
        >
        <button
          type="submit"
          class="btn compact danger-text"
          :disabled="busy || !!removeProblem"
        >
          Review delete
        </button>
        <span
          v-if="removeProblem"
          class="error"
          role="alert"
        >{{ removeProblem }}</span>
      </form>
      <form
        class="settings"
        aria-label="Series settings"
        @submit.prevent="saveSettings"
      >
        <strong>Settings</strong>
        <label>Retention (ms, 0 keeps forever)
          <input
            v-model="settings.retentionMs"
            class="input compact mono"
            inputmode="numeric"
            spellcheck="false"
          >
        </label>
        <label>Duplicate policy
          <select
            v-model="settings.duplicatePolicy"
            class="select compact"
          >
            <option value="">server default</option>
            <option
              v-for="p in DUPLICATE_POLICIES"
              :key="p"
              :value="p"
            >{{ p }}</option>
          </select>
        </label>
        <fieldset class="labels-edit">
          <legend>Labels (they replace all labels; names and values are kept exactly, spaces included)</legend>
          <div
            v-for="(l, i) in settings.labels"
            :key="i"
            class="label-row"
          >
            <input
              v-model="l.name"
              class="input compact mono"
              :aria-label="`Label ${i + 1} name`"
              spellcheck="false"
            >
            <input
              v-model="l.value"
              class="input compact mono"
              :aria-label="`Label ${i + 1} value`"
              spellcheck="false"
            >
            <button
              type="button"
              class="btn compact"
              :aria-label="`Remove label ${i + 1}`"
              @click="settings.labels.splice(i, 1)"
            >
              Remove
            </button>
          </div>
          <div>
            <button
              type="button"
              class="btn compact"
              @click="settings.labels.push({ name: '', value: '' })"
            >
              Add label
            </button>
          </div>
        </fieldset>
        <p
          v-if="shrinkWarning"
          class="notice tone-warning"
        >
          {{ shrinkWarning }}
        </p>
        <ul
          v-if="alter.problems.length > 0"
          class="problems"
          role="alert"
        >
          <li
            v-for="p in alter.problems"
            :key="p"
          >
            {{ p }}
          </li>
        </ul>
        <div>
          <button
            type="submit"
            class="btn compact"
            :disabled="busy || alter.edit === null"
          >
            Review settings
          </button>
        </div>
      </form>
    </template>
  </div>
</template>

<style scoped>
.ts {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.facts {
  display: flex;
  flex-wrap: wrap;
  gap: 16px;
  margin: 0;
}

.facts dt {
  font-size: 11px;
  color: var(--text-muted);
}

.facts dd {
  margin: 0;
}

.labels {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  margin: 0;
}

.rules {
  margin: 0;
  font-size: 12px;
}

.range,
.bar,
.change {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  font-size: 12px;
  color: var(--text-muted);
}

.change strong,
.settings strong {
  color: var(--text);
}

.settings {
  display: flex;
  flex-direction: column;
  gap: 6px;
  font-size: 12px;
  max-width: 520px;
}

.settings label {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.stamp {
  width: 200px;
}

.labels-edit {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0;
  padding: 0;
  border: 0;
}

.label-row {
  display: flex;
  gap: 4px;
}

.note {
  margin: 0;
  font-size: 12px;
}

.compact {
  height: 28px;
  font-size: 12px;
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.spacer {
  flex: 1;
}

.table {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.table th {
  text-align: left;
  font-weight: 500;
  color: var(--text-muted);
  border-bottom: 1px solid var(--border);
  padding: 4px 8px;
}

.table td {
  padding: 3px 8px;
  border-bottom: 1px solid var(--border-subtle, var(--border));
}

.num {
  text-align: right;
}

.notice,
.problems {
  margin: 0;
  font-size: 12px;
}

.error {
  font-size: 11px;
  color: var(--danger-text);
}

.danger-text {
  color: var(--danger-text);
}
</style>
