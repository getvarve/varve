<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue';
import type { ScoredMember } from '../../api/redis';
import type { Value } from '../../api/sessions';
import { canReference, parseScore, scoreText, targetOf, textValue } from '../../lib/redisEdit';
import type { EditDraft } from '../../lib/redisEdit';
import { offReason } from '../../lib/tips';
import { cellClass, cellText, fullText, isPlaceholder, placeholderHelp, plainText } from '../../lib/values';
import { vFocus } from './vFocus';

/**
 * Sorted set members by rank with their scores: change a score (ZADD XX),
 * remove (ZREM) and add (ZADD NX), staged into the key's draft.
 */
const props = withDefaults(
  defineProps<{
    members: readonly ScoredMember[];
    draft: EditDraft;
    editable: boolean;
    /** Position of the first row (the range offset); rank for the whole set. */
    first?: number;
    positionLabel?: string;
    /** False for lex ranges: Redis returns no scores there. */
    scores?: boolean;
  }>(),
  { first: 0, positionLabel: 'rank', scores: true },
);

const uid = useId();
/**
 * The member whose score is being edited (its `targetOf.member` key, not a
 * row index, so paging never moves the edit to another member), and its text.
 */
const editing = ref<string | null>(null);
const editText = ref('');
const editError = ref<string | null>(null);
const newMember = ref('');
const newScore = ref('');
const addError = ref<string | null>(null);
/** Set when the member field was empty on submit; a second, explicit button adds the empty member. */
const emptyMember = ref(false);

// Another page or a new read replaces the rows: an edit left open is dropped.
watch(
  () => props.members,
  () => {
    editing.value = null;
    editError.value = null;
  },
);

function tip(value: Value): string {
  return isPlaceholder(value) ? placeholderHelp(value) : fullText(value);
}

function pendingOf(m: ScoredMember) {
  void props.draft.size;
  const edit = props.draft.pending(targetOf.member(m.member));
  return edit?.op === 'zsetScore' || edit?.op === 'zsetRemove' ? edit : null;
}

function shownScore(m: ScoredMember): Value {
  const pending = pendingOf(m);
  return pending?.op === 'zsetScore' ? pending.score : m.score;
}

function startEdit(m: ScoredMember): void {
  editing.value = targetOf.member(m.member);
  editError.value = null;
  editText.value = scoreText(shownScore(m));
}

function cancelEdit(): void {
  editing.value = null;
  editError.value = null;
}

function commitEdit(m: ScoredMember): void {
  if (editing.value !== targetOf.member(m.member)) return;
  const score = parseScore(editText.value);
  if (!score) {
    editError.value = 'Enter a number, inf or -inf.';
    return;
  }
  editing.value = null;
  editError.value = null;
  if (scoreText(score) === scoreText(m.score)) props.draft.unstage(targetOf.member(m.member));
  else props.draft.stage({ op: 'zsetScore', member: m.member, score, original: m.score });
}

const loaded = computed(() => new Set(props.members.map((m) => plainText(m.member))));

/** Stages the new member; an empty member name is only added through the explicit "Add empty member" button. */
function add(allowEmpty = false): void {
  const score = parseScore(newScore.value);
  if (!score) {
    addError.value = 'Enter a score: a number, inf or -inf.';
    emptyMember.value = false;
    return;
  }
  if (newMember.value === '' && !allowEmpty) {
    addError.value = 'Enter a member.';
    emptyMember.value = true;
    return;
  }
  if (loaded.value.has(newMember.value)) {
    addError.value = 'This member is in the set already: change its score in its row.';
    emptyMember.value = false;
    return;
  }
  addError.value = null;
  emptyMember.value = false;
  props.draft.stage({ op: 'zsetAdd', member: textValue(newMember.value), score });
  newMember.value = '';
  newScore.value = '';
}
</script>

<template>
  <div class="editor">
    <table class="table">
      <thead>
        <tr>
          <th
            scope="col"
            class="num"
          >
            {{ positionLabel }}
          </th>
          <th scope="col">
            member
          </th>
          <th
            v-if="scores"
            scope="col"
            class="num"
          >
            score
          </th>
          <th
            v-if="editable"
            scope="col"
            class="actions"
          >
            <span class="visually-hidden">Actions</span>
          </th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="(m, i) in members"
          :key="i"
          :class="{ changed: pendingOf(m)?.op === 'zsetScore', removed: pendingOf(m)?.op === 'zsetRemove' }"
        >
          <td class="mono num subtle">
            {{ first + i }}
          </td>
          <td
            class="mono"
            :class="cellClass(m.member)"
            :title="tip(m.member)"
          >
            {{ cellText(m.member) }}
          </td>
          <td
            v-if="scores && editing === targetOf.member(m.member)"
            class="mono num"
          >
            <label
              class="visually-hidden"
              :for="`${uid}-score-${i}`"
            >New score of {{ cellText(m.member) }}</label>
            <input
              :id="`${uid}-score-${i}`"
              v-model="editText"
              v-focus
              class="input inline mono num"
              spellcheck="false"
              :aria-invalid="!!editError"
              :aria-describedby="editError ? `${uid}-score-error` : undefined"
              @keydown.enter.prevent="commitEdit(m)"
              @keydown.escape.prevent.stop="cancelEdit"
              @blur="commitEdit(m)"
            >
            <span
              v-if="editError"
              :id="`${uid}-score-error`"
              class="error"
              role="alert"
            >{{ editError }}</span>
          </td>
          <td
            v-else-if="scores"
            class="mono num"
          >
            {{ cellText(shownScore(m)) }}
          </td>
          <td
            v-if="editable"
            class="actions"
          >
            <button
              v-if="pendingOf(m)"
              type="button"
              class="btn ghost compact"
              @click="draft.unstage(targetOf.member(m.member))"
            >
              Undo
            </button>
            <template v-else>
              <button
                v-if="scores"
                type="button"
                class="btn ghost compact"
                :aria-label="`Change the score of ${cellText(m.member)}`"
                :disabled="!canReference(m.member)"
                :title="canReference(m.member) ? undefined : 'This member is too large to read, so it cannot be changed here'"
                @click="startEdit(m)"
              >
                Score
              </button>
              <button
                type="button"
                class="btn ghost compact"
                :aria-label="`Remove ${cellText(m.member)}`"
                :disabled="!canReference(m.member) || m.score.t === 'null'"
                :title="offReason([!canReference(m.member), 'This member is too large to read, so it cannot be changed here'], [m.score.t === 'null', 'A lex range has no scores; remove it from the rank or score view'])"
                @click="draft.stage({ op: 'zsetRemove', member: m.member, original: m.score })"
              >
                Remove
              </button>
            </template>
          </td>
        </tr>
      </tbody>
    </table>
    <form
      v-if="editable"
      class="add"
      novalidate
      @submit.prevent="add()"
    >
      <label
        class="visually-hidden"
        :for="`${uid}-member`"
      >New member</label>
      <input
        :id="`${uid}-member`"
        v-model="newMember"
        class="input compact mono grow"
        placeholder="member"
        spellcheck="false"
        :aria-invalid="emptyMember"
        :aria-describedby="emptyMember && addError ? `${uid}-add-error` : undefined"
      >
      <label
        class="visually-hidden"
        :for="`${uid}-score`"
      >Score</label>
      <input
        :id="`${uid}-score`"
        v-model="newScore"
        class="input compact mono score"
        placeholder="score"
        spellcheck="false"
        :aria-invalid="!!addError"
        :aria-describedby="addError ? `${uid}-add-error` : undefined"
      >
      <button
        type="submit"
        class="btn compact"
      >
        Add member
      </button>
      <button
        v-if="emptyMember && newMember === ''"
        type="button"
        class="btn ghost compact"
        @click="add(true)"
      >
        Add empty member
      </button>
      <span
        v-if="addError"
        :id="`${uid}-add-error`"
        class="error"
        role="alert"
      >{{ addError }}</span>
    </form>
  </div>
</template>

<style scoped src="./editorTable.css"></style>
<style scoped>
.score {
  width: 120px;
}
</style>
