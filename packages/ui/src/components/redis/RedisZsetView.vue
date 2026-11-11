<script setup lang="ts">
import { computed, reactive, shallowRef, useId } from 'vue';
import { useTabs } from '../../composables/useTabs';
import { redisZsetRange } from '../../api/redis';
import type { ScoredMember, ZsetRange } from '../../api/redis';
import { latestOnly } from '../../lib/async';
import { formatBound, parseBound } from '../../lib/redisEdit';
import type { EditDraft } from '../../lib/redisEdit';
import { redisErrorView } from '../../lib/redisErrors';
import { formatCount } from '../../lib/values';
import RedisZsetEditor from './RedisZsetEditor.vue';

/**
 * A sorted set by rank (the pages the key viewer reads), or by a score or lex
 * range (`ZRANGE ... BYSCORE|BYLEX [REV] LIMIT`), page by page. Every view
 * edits through the same draft.
 */
const props = defineProps<{
  tabId: string;
  sessionId: string;
  keyName: string;
  /** Members read by rank so far. */
  ranked: readonly ScoredMember[];
  draft: EditDraft;
  editable: boolean;
}>();

const uid = useId();
const PAGE_SIZES = [25, 50, 100, 500] as const;

const form = reactive<{ mode: 'rank' | 'score' | 'lex'; min: string; max: string; reverse: boolean; count: number }>({
  mode: 'rank',
  min: '-inf',
  max: '+inf',
  reverse: true,
  count: 50,
});
const formError = (): string | null => {
  if (form.mode === 'rank') return null;
  if (!parseBound(form.min, form.mode) || !parseBound(form.max, form.mode)) return 'Use a number, inf or -inf; prefix ( for an exclusive end.';
  return null;
};

/** The range shown, the members read and where the next page starts. */
const shown = reactive<{ range: ZsetRange | null; first: number; nextOffset: string | null; loading: boolean; error: string | null; missing: boolean }>({
  range: null,
  first: 0,
  nextOffset: null,
  loading: false,
  error: null,
  missing: false,
});
const members = shallowRef<ScoredMember[]>([]);
const requests = latestOnly();
const problem = computed(formError);

function rangeOf(offset: string): ZsetRange | null {
  if (form.mode === 'rank') return null;
  const min = parseBound(form.min, form.mode);
  const max = parseBound(form.max, form.mode);
  if (!min || !max) return null;
  return { by: form.mode, min, max, reverse: form.reverse, offset, count: form.count };
}

async function read(range: ZsetRange): Promise<void> {
  const token = requests.next();
  shown.loading = true;
  shown.error = null;
  const sessionId = props.sessionId;
  try {
    const page = await redisZsetRange(sessionId, props.keyName, range);
    if (!requests.isCurrent(token)) return;
    shown.range = range;
    shown.missing = page === null;
    shown.first = Number(range.offset);
    members.value = page?.members ?? [];
    shown.nextOffset = page?.nextOffset ?? null;
  } catch (e: unknown) {
    if (!requests.isCurrent(token)) return;
    const view = redisErrorView(useTabs().reportTabError(props.tabId, e, sessionId));
    shown.error = view.hint ? `${view.message} ${view.hint}` : view.message;
  } finally {
    if (requests.isCurrent(token)) shown.loading = false;
  }
}

function apply(): void {
  if (form.mode === 'rank') {
    shown.range = null;
    return;
  }
  const range = rangeOf('0');
  if (range) void read(range);
}

function setMode(mode: 'rank' | 'score' | 'lex'): void {
  if (form.mode === mode) return;
  form.mode = mode;
  // A range that was read (its rows, command and error) belongs to the mode it was read in: under
  // another mode it would show, for example, score rows and a BYSCORE command under "By lex".
  // A read still running is dropped for the same reason.
  requests.next();
  shown.loading = false;
  shown.error = null;
  shown.range = null;
  members.value = [];
  shown.nextOffset = null;
  shown.missing = false;
  if (mode === 'score') {
    form.min = '-inf';
    form.max = '+inf';
  } else if (mode === 'lex') {
    form.min = '-';
    form.max = '+';
  }
}

/**
 * The range part of the view: shown once a range was read, and also when the
 * first read of a range failed (its error with an empty table, never the rank
 * list under a "By score" or "By lex" toggle).
 */
const showRange = computed(() => shown.range !== null || (form.mode !== 'rank' && shown.error !== null));

function next(): void {
  if (shown.range && shown.nextOffset !== null) void read({ ...shown.range, offset: shown.nextOffset });
}

function previous(): void {
  if (!shown.range) return;
  const offset = Math.max(0, shown.first - shown.range.count);
  void read({ ...shown.range, offset: String(offset) });
}

/** Re-reads the current page, for example after an apply. */
function reload(): void {
  if (shown.range) void read(shown.range);
}
defineExpose({ reload });

/** The command of the range that is shown (not of form fields edited since). */
const command = computed(() => {
  const r = shown.range;
  if (!r) return null;
  const lo = formatBound(r.min, r.by, 'min');
  const hi = formatBound(r.max, r.by, 'max');
  const ends = r.reverse ? `${hi} ${lo}` : `${lo} ${hi}`;
  return `ZRANGE ${props.keyName} ${ends} BY${r.by.toUpperCase()}${r.reverse ? ' REV' : ''} LIMIT ${r.offset} ${r.count}`;
});
</script>

<template>
  <div class="zset">
    <form
      class="range"
      novalidate
      @submit.prevent="apply"
    >
      <div
        class="segmented"
        role="radiogroup"
        aria-label="Order"
      >
        <button
          v-for="m in (['rank', 'score', 'lex'] as const)"
          :key="m"
          type="button"
          role="radio"
          :aria-checked="form.mode === m"
          @click="setMode(m)"
        >
          {{ m === 'rank' ? 'By rank' : m === 'score' ? 'By score' : 'By lex' }}
        </button>
      </div>
      <template v-if="form.mode !== 'rank'">
        <label :for="`${uid}-min`">min</label>
        <input
          :id="`${uid}-min`"
          v-model="form.min"
          class="input compact mono bound"
          spellcheck="false"
          :aria-invalid="!!problem"
          :aria-describedby="problem ? `${uid}-error` : undefined"
        >
        <label :for="`${uid}-max`">max</label>
        <input
          :id="`${uid}-max`"
          v-model="form.max"
          class="input compact mono bound"
          spellcheck="false"
          :aria-invalid="!!problem"
        >
        <label class="check"><input
          v-model="form.reverse"
          type="checkbox"
        > Highest first</label>
        <label :for="`${uid}-count`">page</label>
        <select
          :id="`${uid}-count`"
          v-model.number="form.count"
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
          :disabled="!!problem || shown.loading"
        >
          Apply
        </button>
      </template>
    </form>
    <span
      v-if="problem && form.mode !== 'rank'"
      :id="`${uid}-error`"
      class="error"
      role="alert"
    >{{ problem }}</span>
    <p
      v-if="form.mode === 'lex' && shown.range?.by !== 'lex'"
      class="subtle hint"
    >
      Lex ranges only make sense when every member has the same score. Use [a or (a for the ends, - and + for open.
    </p>

    <template v-if="showRange">
      <div
        v-if="command"
        class="bar"
      >
        <span class="mono subtle">{{ command }}</span>
      </div>
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
      <RedisZsetEditor
        :members="members"
        :draft="draft"
        :editable="editable"
        :first="shown.first"
        position-label="#"
        :scores="(shown.range?.by ?? form.mode) === 'score'"
      />
      <div
        v-if="shown.range"
        class="bar"
      >
        <span class="subtle">
          {{ members.length === 0 ? 'No members in this range' : `${formatCount(String(shown.first + 1))} to ${formatCount(String(shown.first + members.length))} in range` }}
        </span>
        <span class="spacer" />
        <button
          type="button"
          class="btn compact"
          :disabled="shown.loading || shown.first === 0"
          @click="previous"
        >
          Previous
        </button>
        <button
          type="button"
          class="btn compact"
          :disabled="shown.loading || shown.nextOffset === null"
          @click="next"
        >
          Next
        </button>
      </div>
    </template>
    <RedisZsetEditor
      v-else
      :members="ranked"
      :draft="draft"
      :editable="editable"
    />
  </div>
</template>

<style scoped>
.zset {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.range,
.bar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  font-size: 12px;
  color: var(--text-muted);
}

.segmented button {
  height: 28px;
  font-size: 12px;
}

.bound {
  width: 96px;
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

.hint,
.notice {
  margin: 0;
  font-size: 12px;
}

.error {
  font-size: 11px;
  color: var(--danger-text);
}
</style>
