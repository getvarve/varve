<script setup lang="ts">
import { computed, ref, useId } from 'vue';
import type { FieldValue } from '../../api/redis';
import type { Value } from '../../api/sessions';
import { canReference, editableText, targetOf, textValue } from '../../lib/redisEdit';
import type { EditDraft } from '../../lib/redisEdit';
import { cellClass, cellText, fullText, isPlaceholder, placeholderHelp, plainText } from '../../lib/values';
import Icon from '../common/Icon.vue';

/** Hash fields with inline edit, delete and add (HSET / HDEL), staged into the key's draft. */
const props = defineProps<{ entries: readonly FieldValue[]; draft: EditDraft; editable: boolean }>();

const uid = useId();
/** Index of the row being edited, and its text. */
const editing = ref<number | null>(null);
const editText = ref('');
const newField = ref('');
const newValue = ref('');
const addError = ref<string | null>(null);

function tip(value: Value): string {
  return isPlaceholder(value) ? placeholderHelp(value) : fullText(value);
}

function pendingOf(field: Value) {
  void props.draft.size;
  const edit = props.draft.pending(targetOf.field(field));
  return edit?.op === 'hashSet' || edit?.op === 'hashDelete' ? edit : null;
}

/** The value a row shows: the staged new value, or what was read. */
function shown(entry: FieldValue): Value {
  const pending = pendingOf(entry.field);
  return pending?.op === 'hashSet' ? pending.value : entry.value;
}

function startEdit(index: number, entry: FieldValue): void {
  const pending = pendingOf(entry.field);
  editing.value = index;
  editText.value = pending?.op === 'hashSet' ? (plainText(pending.value) ?? '') : (plainText(entry.value) ?? '');
}

function commitEdit(entry: FieldValue): void {
  if (editing.value === null) return;
  const text = editText.value;
  editing.value = null;
  if (text === plainText(entry.value)) props.draft.unstage(targetOf.field(entry.field));
  else props.draft.stage({ op: 'hashSet', field: entry.field, value: textValue(text), original: entry.value });
}

function remove(entry: FieldValue): void {
  props.draft.stage({ op: 'hashDelete', field: entry.field, original: entry.value });
}

const loadedFields = computed(() => new Set(props.entries.map((e) => plainText(e.field))));

function add(): void {
  const field = newField.value;
  if (field === '') {
    addError.value = 'Enter a field name.';
    return;
  }
  if (loadedFields.value.has(field)) {
    addError.value = 'This field exists already: edit it in its row.';
    return;
  }
  addError.value = null;
  props.draft.stage({ op: 'hashSet', field: textValue(field), value: textValue(newValue.value), original: null });
  newField.value = '';
  newValue.value = '';
}
</script>

<template>
  <div class="editor">
    <table class="table">
      <thead>
        <tr>
          <th scope="col">
            field
          </th>
          <th scope="col">
            value
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
          v-for="(f, i) in entries"
          :key="i"
          :class="{ changed: pendingOf(f.field)?.op === 'hashSet', removed: pendingOf(f.field)?.op === 'hashDelete' }"
        >
          <td class="mono">
            {{ cellText(f.field) }}
          </td>
          <td
            v-if="editing === i"
            class="mono"
          >
            <label
              class="visually-hidden"
              :for="`${uid}-edit-${i}`"
            >New value of {{ cellText(f.field) }}</label>
            <input
              :id="`${uid}-edit-${i}`"
              v-model="editText"
              class="input inline mono"
              spellcheck="false"
              autofocus
              @keydown.enter.prevent="commitEdit(f)"
              @keydown.escape.prevent.stop="editing = null"
              @blur="commitEdit(f)"
            >
          </td>
          <td
            v-else
            class="mono"
            :class="cellClass(shown(f))"
            :title="tip(shown(f))"
          >
            {{ cellText(shown(f)) }}
          </td>
          <td
            v-if="editable"
            class="actions"
          >
            <button
              v-if="pendingOf(f.field)"
              type="button"
              class="btn ghost compact"
              @click="draft.unstage(targetOf.field(f.field))"
            >
              Undo
            </button>
            <template v-else>
              <button
                type="button"
                class="btn ghost compact"
                :aria-label="`Edit ${cellText(f.field)}`"
                :disabled="!editableText(f.value) || !canReference(f.field)"
                :title="editableText(f.value) ? undefined : 'Binary and very large values cannot be edited here.'"
                @click="startEdit(i, f)"
              >
                Edit
              </button>
              <button
                type="button"
                class="btn ghost compact"
                :aria-label="`Delete ${cellText(f.field)}`"
                :disabled="!canReference(f.value) || !canReference(f.field)"
                @click="remove(f)"
              >
                Delete
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
      @submit.prevent="add"
    >
      <label
        class="visually-hidden"
        :for="`${uid}-field`"
      >New field</label>
      <input
        :id="`${uid}-field`"
        v-model="newField"
        class="input compact mono"
        placeholder="field"
        spellcheck="false"
        :aria-invalid="!!addError"
        :aria-describedby="addError ? `${uid}-add-error` : undefined"
      >
      <label
        class="visually-hidden"
        :for="`${uid}-value`"
      >Value</label>
      <input
        :id="`${uid}-value`"
        v-model="newValue"
        class="input compact mono grow"
        placeholder="value"
        spellcheck="false"
      >
      <button
        type="submit"
        class="btn compact"
      >
        <Icon name="plus" />Add field
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
