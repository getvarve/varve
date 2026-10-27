<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue';
import type { KeyEdit } from '../../api/redis';
import type { Value } from '../../api/sessions';
import { jsonProblem, minifyJson, textValue } from '../../lib/redisEdit';
import { prettyJson } from '../../lib/values';

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
const emit = defineEmits<{ save: [edit: KeyEdit] }>();

const uid = useId();
const editing = ref(false);
const draft = ref('');
const minify = ref(false);

const originalText = computed(() => (props.original?.t === 'text' || props.original?.t === 'json' ? props.original.v : ''));
const problem = computed(() => (props.mode === 'json' || looksJson.value ? jsonProblem(draft.value) : null));
/** A string that held JSON when read keeps the JSON checks and the minify option. */
const looksJson = computed(() => props.mode === 'string' && prettyJson(originalText.value) !== null);

watch(
  () => props.original,
  () => {
    editing.value = false;
  },
);

function start(): void {
  draft.value = prettyJson(originalText.value) ?? originalText.value;
  // A document stored compact is saved compact again unless the user says otherwise.
  minify.value = (props.mode === 'json' || looksJson.value) && !/\n/.test(originalText.value);
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
        class="input mono text"
        spellcheck="false"
        rows="14"
        :aria-invalid="mode === 'json' && !!problem"
        :aria-describedby="`${uid}-state`"
        autofocus
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
