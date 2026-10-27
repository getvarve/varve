<script setup lang="ts">
import { computed, ref, useId } from 'vue';
import type { Value } from '../../api/sessions';
import { canReference, targetOf, textValue } from '../../lib/redisEdit';
import type { EditDraft } from '../../lib/redisEdit';
import { cellClass, cellText, fullText, isPlaceholder, placeholderHelp, plainText } from '../../lib/values';

/** Set members with remove (SREM) and add (SADD), staged into the key's draft. */
const props = defineProps<{ members: readonly Value[]; draft: EditDraft; editable: boolean }>();

const uid = useId();
const newMember = ref('');
const addError = ref<string | null>(null);

function tip(value: Value): string {
  return isPlaceholder(value) ? placeholderHelp(value) : fullText(value);
}

function removed(member: Value): boolean {
  void props.draft.size;
  return props.draft.pending(targetOf.member(member))?.op === 'setRemove';
}

const loaded = computed(() => new Set(props.members.map((m) => plainText(m))));

function add(): void {
  if (loaded.value.has(newMember.value)) {
    addError.value = 'This member is in the set already.';
    return;
  }
  addError.value = null;
  props.draft.stage({ op: 'setAdd', members: [textValue(newMember.value)] });
  newMember.value = '';
}
</script>

<template>
  <div class="editor">
    <table class="table">
      <thead>
        <tr>
          <th scope="col">
            member
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
          :class="{ removed: removed(m) }"
        >
          <td
            class="mono"
            :class="cellClass(m)"
            :title="tip(m)"
          >
            {{ cellText(m) }}
          </td>
          <td
            v-if="editable"
            class="actions"
          >
            <button
              v-if="removed(m)"
              type="button"
              class="btn ghost compact"
              @click="draft.unstage(targetOf.member(m))"
            >
              Undo
            </button>
            <button
              v-else
              type="button"
              class="btn ghost compact"
              :aria-label="`Remove ${cellText(m)}`"
              :disabled="!canReference(m)"
              @click="draft.stage({ op: 'setRemove', member: m })"
            >
              Remove
            </button>
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
        :for="`${uid}-new`"
      >New member</label>
      <input
        :id="`${uid}-new`"
        v-model="newMember"
        class="input compact mono grow"
        placeholder="New member"
        spellcheck="false"
        :aria-invalid="!!addError"
        :aria-describedby="addError ? `${uid}-error` : undefined"
      >
      <button
        type="submit"
        class="btn compact"
      >
        Add member (SADD)
      </button>
      <span
        v-if="addError"
        :id="`${uid}-error`"
        class="error"
        role="alert"
      >{{ addError }}</span>
    </form>
  </div>
</template>

<style scoped src="./editorTable.css"></style>
