<script setup lang="ts">
import { computed, ref, useId } from 'vue';
import type { CellValue, ColumnInfo } from '../../api/grid';
import type { Value } from '../../api/sessions';
import { checkExpression } from '../../lib/gridSql';
import type { GridDialect } from '../../lib/gridSql';
import { columnAccess, editText, parseCellText } from '../../lib/gridValues';
import { charCount, prettyJson } from '../../lib/values';
import Modal from '../common/Modal.vue';

/**
 * Edits one cell in full: large text, JSON, arrays and bytes, or sets it
 * to NULL, DEFAULT or a raw SQL expression. Answers with the new
 * `CellValue`, or `null` when cancelled.
 */
const props = defineProps<{
  column: ColumnInfo;
  dialect: GridDialect;
  /** What the cell holds now: a pending change, or the stored value. */
  current: CellValue;
  /** True for a cell of a new row (DEFAULT is the natural empty state). */
  insert: boolean;
  /** True when the cell may only be read. */
  readOnly: boolean;
}>();
const emit = defineEmits<{ done: [answer: CellValue | null] }>();

type Mode = 'value' | 'null' | 'default' | 'expression';
const uid = useId();
const access = computed(() => columnAccess(props.column));
const writable = computed(() => !props.readOnly && (props.insert ? access.value.insert : access.value.update));

function initial(): { mode: Mode; text: string } {
  const c = props.current;
  if (c.kind === 'default') return { mode: 'default', text: '' };
  if (c.kind === 'expression') return { mode: 'expression', text: c.value };
  if (c.value.t === 'null') return { mode: 'null', text: '' };
  return { mode: 'value', text: editText(props.dialect, c.value) };
}
const start = initial();
const mode = ref<Mode>(start.mode);
const text = ref(start.text);

const isJson = computed(() => ['json', 'jsonb'].includes(props.column.typeName));
const problem = computed(() => {
  if (mode.value === 'expression') return checkExpression(props.dialect, text.value);
  if (mode.value === 'value') {
    const parsed = parseCellText(props.dialect, props.column, text.value);
    if (!parsed.ok) return parsed.message;
  }
  if (mode.value === 'null' && !props.column.nullable) return `${props.column.name} is NOT NULL.`;
  return null;
});

function format(): void {
  const pretty = prettyJson(text.value);
  if (pretty) text.value = pretty;
}

function save(): void {
  if (problem.value || !writable.value) return;
  let out: CellValue;
  if (mode.value === 'null') out = { kind: 'value', value: { t: 'null' } as Value };
  else if (mode.value === 'default') out = { kind: 'default' };
  else if (mode.value === 'expression') out = { kind: 'expression', value: text.value };
  else {
    const parsed = parseCellText(props.dialect, props.column, text.value);
    if (!parsed.ok) return;
    out = parsed.cell;
  }
  emit('done', out);
}
</script>

<template>
  <Modal
    :title="`${column.name}`"
    :width="680"
    @close="emit('done', null)"
  >
    <template #badges>
      <span class="mono subtle">{{ column.dataType }}</span>
    </template>
    <div class="body">
      <p
        v-if="!writable"
        class="notice tone-neutral"
      >
        {{ readOnly ? 'Read only: the value is shown in full.' : access.reason }}
      </p>
      <div
        role="radiogroup"
        aria-label="What the cell holds"
        class="segmented"
      >
        <button
          type="button"
          role="radio"
          :aria-checked="mode === 'value'"
          :disabled="!writable"
          @click="mode = 'value'"
        >
          Value
        </button>
        <button
          type="button"
          role="radio"
          :aria-checked="mode === 'null'"
          :disabled="!writable || !column.nullable"
          @click="mode = 'null'"
        >
          NULL
        </button>
        <button
          type="button"
          role="radio"
          :aria-checked="mode === 'default'"
          :disabled="!writable"
          @click="mode = 'default'"
        >
          DEFAULT
        </button>
        <button
          type="button"
          role="radio"
          :aria-checked="mode === 'expression'"
          :disabled="!writable"
          @click="mode = 'expression'"
        >
          SQL expression
        </button>
      </div>
      <template v-if="mode === 'value' || mode === 'expression'">
        <label
          class="visually-hidden"
          :for="`${uid}-text`"
        >{{ mode === 'expression' ? 'SQL expression' : 'Value' }}</label>
        <textarea
          :id="`${uid}-text`"
          v-model="text"
          class="textarea"
          rows="14"
          spellcheck="false"
          :readonly="!writable"
          :aria-invalid="!!problem"
          data-autofocus
        />
        <div class="meta">
          <span class="subtle">{{ charCount(text).toLocaleString('en-US') }} characters</span>
          <button
            v-if="isJson && mode === 'value'"
            type="button"
            class="btn compact ghost"
            @click="format"
          >
            Format JSON
          </button>
          <span
            v-if="mode === 'expression'"
            class="subtle"
          >Runs as SQL on the server, in parentheses. Check it in the review before applying.</span>
        </div>
      </template>
      <p
        v-else
        class="muted"
      >
        {{ mode === 'null' ? 'The cell is set to NULL.' : column.hasDefault ? 'The server fills in the column default.' : 'The column has no default, so it becomes NULL.' }}
      </p>
      <p
        v-if="problem"
        class="notice tone-danger"
        role="alert"
      >
        {{ problem }}
      </p>
    </div>
    <template #footer>
      <button
        type="button"
        class="btn"
        @click="emit('done', null)"
      >
        {{ writable ? 'Cancel' : 'Close' }}
      </button>
      <button
        v-if="writable"
        type="button"
        class="btn primary"
        :disabled="!!problem"
        @click="save"
      >
        Keep change
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.body {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.meta {
  display: flex;
  align-items: center;
  gap: 12px;
  font-size: 12px;
}

.notice {
  margin: 0;
}

.compact {
  height: 26px;
  font-size: 12px;
}
</style>
