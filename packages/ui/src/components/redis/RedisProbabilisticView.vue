<script setup lang="ts">
import { computed, ref, shallowRef, useId, watch } from 'vue';
import { MODULE_KIND_LABELS, redisModuleQuery } from '../../api/redis';
import type { InfoField, KeyEdit, ModuleKind, ModuleQuery, QueryResult, TopKItem } from '../../api/redis';
import { useTabs } from '../../composables/useTabs';
import { latestOnly } from '../../lib/async';
import { textValue } from '../../lib/redisEdit';
import { redisErrorView } from '../../lib/redisErrors';
import { addEdit, buildQuery, cmsIncrements, queriesFor } from '../../lib/redisModules';
import { cellText, formatCount } from '../../lib/values';

/**
 * A Bloom or Cuckoo filter, Count-Min sketch, Top-K or t-digest (design doc
 * 4.2, Bloom): its info fields, the Top-K list, the read-only queries
 * (also on read-only connections) and the adds Varve plans like every value
 * edit. A Cuckoo delete is checked first (the filter must still report the
 * item).
 */
const props = defineProps<{
  tabId: string;
  sessionId: string;
  keyName: string;
  kind: ModuleKind;
  fields: InfoField[];
  top: TopKItem[];
  editable: boolean;
  busy: boolean;
}>();
const emit = defineEmits<{ review: [edit: KeyEdit] }>();

const uid = useId();
const choices = computed(() => queriesFor(props.kind));
const op = ref<ModuleQuery['op'] | null>(choices.value[0]?.op ?? null);
watch(choices, (list) => {
  if (!list.some((c) => c.op === op.value)) op.value = list[0]?.op ?? null;
});
const choice = computed(() => choices.value.find((c) => c.op === op.value) ?? null);
const input = ref('');
const result = shallowRef<QueryResult | null>(null);
const queryError = ref<string | null>(null);
const querying = ref(false);
const requests = latestOnly();

const query = computed(() => (op.value ? buildQuery(op.value, input.value) : null));

async function run(): Promise<void> {
  const q = query.value;
  if (!q) return;
  const token = requests.next();
  querying.value = true;
  try {
    const out = await redisModuleQuery(props.sessionId, props.keyName, q);
    if (!requests.isCurrent(token)) return;
    queryError.value = out ? null : 'This key no longer exists.';
    result.value = out;
  } catch (e: unknown) {
    if (!requests.isCurrent(token)) return;
    const view = redisErrorView(useTabs().reportTabError(props.tabId, e));
    queryError.value = view.hint ? `${view.message} ${view.hint}` : view.message;
  } finally {
    if (requests.isCurrent(token)) querying.value = false;
  }
}

function outputText(row: QueryResult['rows'][number]): string {
  if (row.output.t === 'bool') {
    if (props.kind === 'topK') return row.output.v ? 'in the top list' : 'not in the top list';
    return row.output.v ? 'may be in the filter' : 'not in the filter';
  }
  return cellText(row.output);
}

// Adds

const addText = ref('');
const nx = ref(false);
const removeItem = ref('');
const add = computed(() => addEdit(props.kind, addText.value, nx.value));
const addProblem = computed(() => {
  if (addText.value.trim() === '') return null;
  if (props.kind === 'countMin') {
    const { problems } = cmsIncrements(addText.value);
    return problems[0] ?? null;
  }
  if (props.kind === 'tDigest' && add.value === null) return 'Each line is one finite number.';
  return null;
});
const addLabel = computed(() => {
  switch (props.kind) {
    case 'countMin':
      return 'Count items (one per line, optionally followed by an increment)';
    case 'tDigest':
      return 'Add observations (one number per line)';
    default:
      return 'Add items (one per line)';
  }
});

function reviewAdd(): void {
  if (add.value && !addProblem.value) emit('review', add.value);
}

function reviewRemove(): void {
  const item = removeItem.value.trim();
  if (item !== '') emit('review', { op: 'cuckooDelete', item: textValue(item) });
}
</script>

<template>
  <div class="prob">
    <p class="subtle">
      {{ MODULE_KIND_LABELS[kind] }}. The structure keeps estimates, not the items themselves, so the browser shows its settings and answers queries.
    </p>
    <table class="table info">
      <tbody>
        <tr
          v-for="f in fields"
          :key="f.name"
        >
          <th>{{ f.name }}</th>
          <td class="mono">
            {{ cellText(f.value) }}
          </td>
        </tr>
      </tbody>
    </table>

    <template v-if="kind === 'topK'">
      <h3>Top items</h3>
      <table
        v-if="top.length > 0"
        class="table"
      >
        <thead>
          <tr>
            <th>#</th>
            <th>Item</th>
            <th class="num">
              Count
            </th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="(t, i) in top"
            :key="i"
          >
            <td class="subtle">
              {{ i + 1 }}
            </td>
            <td class="mono">
              {{ cellText(t.item) }}
            </td>
            <td class="mono num">
              {{ formatCount(t.count) }}
            </td>
          </tr>
        </tbody>
      </table>
      <p
        v-else
        class="subtle"
      >
        The list is empty.
      </p>
    </template>

    <form
      v-if="choices.length > 0"
      class="query"
      aria-label="Query"
      @submit.prevent="run"
    >
      <h3>Query</h3>
      <label :for="`${uid}-op`">Question</label>
      <select
        :id="`${uid}-op`"
        v-model="op"
        class="select compact"
      >
        <option
          v-for="c in choices"
          :key="c.op"
          :value="c.op"
        >
          {{ c.label }}
        </option>
      </select>
      <label :for="`${uid}-input`">{{ choice?.op === 'tdigestTrimmedMean' ? 'Low and high fraction' : `One ${choice?.input ?? 'input'} per line` }}</label>
      <textarea
        :id="`${uid}-input`"
        v-model="input"
        class="input mono"
        rows="3"
        spellcheck="false"
      />
      <div>
        <button
          type="submit"
          class="btn compact"
          :disabled="querying || query === null"
        >
          {{ querying ? 'Asking…' : 'Ask' }}
        </button>
      </div>
      <p
        v-if="queryError"
        class="notice tone-danger"
        role="alert"
      >
        {{ queryError }}
      </p>
      <template v-if="result">
        <span class="mono subtle">{{ result.command }}</span>
        <table class="table">
          <thead>
            <tr>
              <th>Input</th>
              <th>Answer</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="(r, i) in result.rows"
              :key="i"
            >
              <td class="mono">
                {{ cellText(r.input) }}
              </td>
              <td class="mono">
                {{ outputText(r) }}
              </td>
            </tr>
          </tbody>
        </table>
      </template>
    </form>

    <template v-if="editable">
      <form
        class="change"
        aria-label="Add"
        @submit.prevent="reviewAdd"
      >
        <h3>Change</h3>
        <label :for="`${uid}-add`">{{ addLabel }}</label>
        <textarea
          :id="`${uid}-add`"
          v-model="addText"
          class="input mono"
          rows="3"
          spellcheck="false"
        />
        <label
          v-if="kind === 'cuckoo'"
          class="check"
        ><input
          v-model="nx"
          type="checkbox"
        > Only items the filter does not report yet (CF.ADDNX)</label>
        <p
          v-if="kind === 'topK'"
          class="subtle"
        >
          Adding can push other items out of the top list.
        </p>
        <span
          v-if="addProblem"
          class="error"
          role="alert"
        >{{ addProblem }}</span>
        <div>
          <button
            type="submit"
            class="btn compact"
            :disabled="busy || add === null || !!addProblem"
          >
            Review
          </button>
        </div>
      </form>
      <form
        v-if="kind === 'cuckoo'"
        class="change"
        aria-label="Delete an item"
        @submit.prevent="reviewRemove"
      >
        <label :for="`${uid}-remove`">Delete one copy of an item (CF.DEL)</label>
        <input
          :id="`${uid}-remove`"
          v-model="removeItem"
          class="input compact mono"
          spellcheck="false"
        >
        <p class="subtle">
          Delete only items that were added: a Cuckoo filter cannot tell a false positive apart, and deleting one removes another item's entry.
        </p>
        <div>
          <button
            type="submit"
            class="btn compact danger-text"
            :disabled="busy || removeItem.trim() === ''"
          >
            Review delete
          </button>
        </div>
      </form>
    </template>
  </div>
</template>

<style scoped>
.prob,
.query,
.change {
  display: flex;
  flex-direction: column;
  gap: 6px;
  font-size: 12px;
}

.query,
.change {
  max-width: 520px;
}

h3 {
  margin: 8px 0 0;
  font-size: 12px;
  font-weight: 600;
}

.table {
  border-collapse: collapse;
  font-size: 12px;
}

.table th {
  text-align: left;
  font-weight: 500;
  color: var(--text-muted);
  padding: 3px 8px;
}

.table td {
  padding: 3px 8px;
  border-bottom: 1px solid var(--border);
}

.info th {
  border-bottom: 1px solid var(--border);
}

.num {
  text-align: right;
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

.subtle,
.notice {
  margin: 0;
}

.error {
  font-size: 11px;
  color: var(--danger-text);
}

.danger-text {
  color: var(--danger-text);
}
</style>
