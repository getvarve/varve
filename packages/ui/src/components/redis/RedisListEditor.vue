<script setup lang="ts">
import { ref, useId } from 'vue';
import type { ListItem } from '../../api/redis';
import type { Value } from '../../api/sessions';
import { canReference, editableText, targetOf, textValue } from '../../lib/redisEdit';
import type { EditDraft } from '../../lib/redisEdit';
import { cellClass, cellText, fullText, isPlaceholder, placeholderHelp, plainText } from '../../lib/values';

/**
 * List elements with inline LSET and remove (by the index as read), plus
 * LPUSH / RPUSH of a new element, staged into the key's draft.
 */
const props = defineProps<{ items: readonly ListItem[]; draft: EditDraft; editable: boolean }>();

const uid = useId();
const editing = ref<string | null>(null);
const editText = ref('');
const newValue = ref('');

function tip(value: Value): string {
  return isPlaceholder(value) ? placeholderHelp(value) : fullText(value);
}

function pendingOf(item: ListItem) {
  void props.draft.size;
  const edit = props.draft.pending(targetOf.index(item.index));
  return edit?.op === 'listSet' || edit?.op === 'listRemove' ? edit : null;
}

function shown(item: ListItem): Value {
  const pending = pendingOf(item);
  return pending?.op === 'listSet' ? pending.value : item.value;
}

function startEdit(item: ListItem): void {
  editing.value = item.index;
  editText.value = plainText(shown(item)) ?? '';
}

function commitEdit(item: ListItem): void {
  if (editing.value !== item.index) return;
  editing.value = null;
  if (editText.value === plainText(item.value)) props.draft.unstage(targetOf.index(item.index));
  else props.draft.stage({ op: 'listSet', index: item.index, value: textValue(editText.value), original: item.value });
}

function push(end: 'head' | 'tail'): void {
  props.draft.stage({ op: 'listPush', end, values: [textValue(newValue.value)] });
  newValue.value = '';
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
            index
          </th>
          <th scope="col">
            element
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
          v-for="item in items"
          :key="item.index"
          :class="{ changed: pendingOf(item)?.op === 'listSet', removed: pendingOf(item)?.op === 'listRemove' }"
        >
          <td class="mono num subtle">
            {{ item.index }}
          </td>
          <td
            v-if="editing === item.index"
            class="mono"
          >
            <label
              class="visually-hidden"
              :for="`${uid}-edit-${item.index}`"
            >New value at index {{ item.index }}</label>
            <input
              :id="`${uid}-edit-${item.index}`"
              v-model="editText"
              class="input inline mono"
              spellcheck="false"
              autofocus
              @keydown.enter.prevent="commitEdit(item)"
              @keydown.escape.prevent.stop="editing = null"
              @blur="commitEdit(item)"
            >
          </td>
          <td
            v-else
            class="mono"
            :class="cellClass(shown(item))"
            :title="tip(shown(item))"
          >
            {{ cellText(shown(item)) }}
          </td>
          <td
            v-if="editable"
            class="actions"
          >
            <button
              v-if="pendingOf(item)"
              type="button"
              class="btn ghost compact"
              @click="draft.unstage(targetOf.index(item.index))"
            >
              Undo
            </button>
            <template v-else>
              <button
                type="button"
                class="btn ghost compact"
                :aria-label="`Edit index ${item.index}`"
                :disabled="!editableText(item.value)"
                :title="editableText(item.value) ? undefined : 'Binary and very large values cannot be edited here.'"
                @click="startEdit(item)"
              >
                Edit
              </button>
              <button
                type="button"
                class="btn ghost compact"
                :aria-label="`Remove index ${item.index}`"
                :disabled="!canReference(item.value)"
                @click="draft.stage({ op: 'listRemove', index: item.index, original: item.value })"
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
      @submit.prevent="push('tail')"
    >
      <label
        class="visually-hidden"
        :for="`${uid}-new`"
      >New element</label>
      <input
        :id="`${uid}-new`"
        v-model="newValue"
        class="input compact mono grow"
        placeholder="New element"
        spellcheck="false"
      >
      <button
        type="button"
        class="btn compact"
        @click="push('head')"
      >
        Push left (LPUSH)
      </button>
      <button
        type="submit"
        class="btn compact"
      >
        Push right (RPUSH)
      </button>
    </form>
  </div>
</template>

<style scoped src="./editorTable.css"></style>
