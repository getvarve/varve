<script setup lang="ts">
import { computed, onBeforeUnmount, ref, useId, watch } from 'vue';
import type { KeyEdit } from '../../api/redis';
import type { Value } from '../../api/sessions';
import { jsonProblem, minifyJson, textValue } from '../../lib/redisEdit';
import { prettyJson } from '../../lib/values';
import { vFocus } from './vFocus';

/**
 * Edits a whole string value (SET ... KEEPTTL) or a JSON document
 * (JSON.SET key $). `text` is the whole value as read; editing is offered only
 * when it is complete text. Save emits the edit; the parent plans it.
 */
const props = defineProps<{
  /** The value as read (text), or `null` while it cannot be edited. */
  original: Value | null;
  /** `json` for a RedisJSON document, `string` for a string key. */
  mode: 'string' | 'json';
  /** Why the value cannot be edited, when it cannot. */
  locked: string | null;
  editable: boolean;
  busy: boolean;
}>();
const emit = defineEmits<{
  save: [edit: KeyEdit];
  /** True while an open edit holds text that differs from what it started with (unsaved work of the tab). */
  dirty: [dirty: boolean];
}>();

const uid = useId();
const editing = ref(false);
const draft = ref('');
const minify = ref(false);
/** The text and minify choice the edit started with, to tell whether anything changed. */
const started = ref<{ text: string; minify: boolean }>({ text: '', minify: false });

const originalText = computed(() => (props.original?.t === 'text' || props.original?.t === 'json' ? props.original.v : ''));
const problem = computed(() => (props.mode === 'json' || looksJson.value ? jsonProblem(draft.value) : null));
/** A string that held JSON when read keeps the JSON checks and the minify option. */
const looksJson = computed(() => props.mode === 'string' && prettyJson(originalText.value) !== null);

// The value was read again. An edit without typed changes closes and shows
// the new value. Typed changes stay: they are unsaved work, and only the
// parent drops them (`close`: after an apply, or when the user said so).
watch(
  () => props.original,
  () => {
    if (!dirty.value) editing.value = false;
  },
);

const asJson = computed(() => props.mode === 'json' || looksJson.value);

/**
 * The stored formatting cannot be written again: it is neither compact nor
 * the 2-space indentation the editor uses (for example `{"a": 1}` or 4 spaces).
 */
const formatLost = computed(() => {
  if (!asJson.value) return false;
  const text = originalText.value;
  return text !== minifyJson(text) && text !== prettyJson(text);
});

const dirty = computed(() => editing.value && (draft.value !== started.value.text || minify.value !== started.value.minify));
watch(dirty, (value) => emit('dirty', value));
onBeforeUnmount(() => {
  if (dirty.value) emit('dirty', false);
});

/** Closes an open edit and drops what was typed. */
function close(): void {
  editing.value = false;
}
defineExpose({ close });

function start(): void {
  draft.value = prettyJson(originalText.value) ?? originalText.value;
  // Only a document that really is stored compact is saved compact again (unless the user says otherwise).
  minify.value = asJson.value && originalText.value === minifyJson(originalText.value);
  started.value = { text: draft.value, minify: minify.value };
  editing.value = true;
}

const canSave = computed(() => {
  if (props.busy || !props.original) return false;
  if (props.mode === 'json' && problem.value !== null) return false;
  return true;
});

function save(): void {
  if (!canSave.value || !props.original) return;
  const valid = problem.value === null;
  const text = minify.value && valid ? minifyJson(draft.value) : draft.value;
  // Nothing changed (or the result is byte for byte what is stored): no write, so untouched bytes stay as they are.
  if (!dirty.value || text === originalText.value) {
    editing.value = false;
    return;
  }
  if (props.mode === 'json') emit('save', { op: 'jsonSet', document: text, original: originalText.value });
  else emit('save', { op: 'stringSet', value: textValue(text), original: props.original });
}
</script>

<template>
  <div class="string-editor">
    <div
      v-if="!editing && editable"
      class="bar"
    >
      <span
        v-if="locked"
        class="subtle"
      >{{ locked }}</span>
      <span class="spacer" />
      <button
        type="button"
        class="btn compact"
        :disabled="!!locked || !original"
        @click="start"
      >
        Edit
      </button>
    </div>
    <form
      v-if="editing"
      class="form"
      novalidate
      @submit.prevent="save"
    >
      <label
        class="visually-hidden"
        :for="`${uid}-text`"
      >{{ mode === 'json' ? 'JSON document' : 'Value' }}</label>
      <textarea
        :id="`${uid}-text`"
        v-model="draft"
        v-focus
        class="input mono text"
        spellcheck="false"
        rows="14"
        :aria-invalid="mode === 'json' && !!problem"
        :aria-describedby="`${uid}-state`"
      />
      <div class="bar">
        <span
          :id="`${uid}-state`"
          :class="problem ? (mode === 'json' ? 'error' : 'subtle') : 'subtle'"
          :role="problem && mode === 'json' ? 'alert' : undefined"
        >
          <template v-if="problem && mode === 'json'">Not valid JSON: {{ problem }}</template>
          <template v-else-if="problem">Not valid JSON any more; it is saved as plain text.</template>
          <template v-else-if="mode === 'json' || looksJson">Valid JSON</template>
          <template v-else>Text</template>
        </span>
        <label
          v-if="(mode === 'json' || looksJson) && !problem"
          class="check"
        ><input
          v-model="minify"
          type="checkbox"
        > Minify on save</label>
        <span
          v-if="formatLost && !problem"
          class="subtle"
        >The original formatting cannot be kept.</span>
        <span
          v-if="mode === 'string'"
          class="subtle"
        >The TTL is kept (KEEPTTL).</span>
        <span class="spacer" />
        <button
          type="button"
          class="btn compact"
          :disabled="busy"
          @click="editing = false"
        >
          Cancel
        </button>
        <button
          type="submit"
          class="btn primary compact"
          :disabled="!canSave"
        >
          {{ busy ? 'Planning…' : 'Review and save' }}
        </button>
      </div>
    </form>
    <slot v-else />
  </div>
</template>

<style scoped>
.string-editor {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.form {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.text {
  min-height: 240px;
  height: auto;
  padding: 10px 12px;
  font-size: 12px;
  line-height: 18px;
  resize: vertical;
}

.bar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 10px;
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

.compact {
  height: 28px;
  font-size: 12px;
}

.error {
  color: var(--danger-text);
}
</style>
