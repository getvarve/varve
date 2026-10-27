<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed } from 'vue';
import type { Folder, FolderContents, FolderDeleteMode } from '../../api/records';
import { useWorkspace } from '../../composables/useWorkspace';
import { deleteFolderFormSchema } from '../../lib/forms';
import Modal from '../common/Modal.vue';

const props = defineProps<{ folder: Folder; contents: FolderContents }>();
const emit = defineEmits<{ done: [answer: FolderDeleteMode | null] }>();

const workspace = useWorkspace();
const itemCount = props.contents.folders + props.contents.connections + props.contents.queries;
const { defineField, handleSubmit, errors } = useForm({
  validationSchema: toTypedSchema(deleteFolderFormSchema(itemCount)),
  initialValues: { mode: 'move_contents_to_parent' as FolderDeleteMode },
});
const [mode] = defineField('mode');
const confirm = handleSubmit((form) => emit('done', form.mode));

const parentName = computed(() => {
  const parent = workspace.records.value.folders.find((f) => f.id === props.folder.parentId);
  return parent ? parent.name : null;
});
const total = computed(() => props.contents.folders + props.contents.connections + props.contents.queries);

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

const summary = computed(() =>
  [
    plural(props.contents.folders, 'subfolder', 'subfolders'),
    plural(props.contents.connections, 'connection', 'connections'),
    plural(props.contents.queries, 'saved query', 'saved queries'),
  ].join(', '),
);
</script>

<template>
  <Modal
    :title="`Delete folder “${folder.name}”?`"
    :width="600"
    @close="emit('done', null)"
  >
    <p class="lede">
      {{ folder.name }} is {{ parentName ? `inside ${parentName}` : 'a top-level folder' }} in your Personal workspace.
    </p>

    <div class="contents">
      <span class="label">Contents</span>
      <span class="counts">{{ total === 0 ? 'The folder is empty.' : `${summary}, counted through every subfolder.` }}</span>
    </div>

    <fieldset class="choices">
      <legend class="label">
        What happens to the contents
      </legend>
      <label class="choice">
        <input
          v-model="mode"
          type="radio"
          name="delete-folder-mode"
          value="move_contents_to_parent"
        >
        <span>
          <span class="choice-title">Move contents to the parent folder <span class="subtle">(default)</span></span>
          <span class="choice-help">
            Everything inside moves to {{ parentName ?? 'the top level' }}. Nothing is deleted, only the empty folder.
          </span>
        </span>
      </label>
      <label class="choice">
        <input
          v-model="mode"
          type="radio"
          name="delete-folder-mode"
          value="delete_contents"
          :disabled="total === 0"
        >
        <span>
          <span class="choice-title">Delete contents too</span>
          <span class="choice-help">
            Removes {{ total === 0 ? 'nothing else' : summary }} together with the folder. This cannot be undone.
          </span>
        </span>
      </label>
    </fieldset>
    <p
      v-if="errors.mode"
      class="notice tone-danger"
      role="alert"
    >
      {{ errors.mode }}
    </p>

    <p class="notice tone-neutral">
      Keychain entries are removed only when a connection is deleted. With Move, saved passwords stay in your keychain.
    </p>

    <template #footer>
      <button
        type="button"
        class="btn"
        @click="emit('done', null)"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn danger"
        @click="confirm"
      >
        {{ mode === 'delete_contents' ? 'Delete folder and contents' : 'Delete folder' }}
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.lede {
  margin: 0 0 16px;
  color: var(--text-muted);
}

.contents {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 12px;
  margin-bottom: 16px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
}

.counts {
  font-variant-numeric: tabular-nums;
}

.choices {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 0 0 16px;
  padding: 0;
  border: 0;
}

.choices legend {
  margin-bottom: 8px;
  padding: 0;
}

.choice {
  display: flex;
  gap: 10px;
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  cursor: pointer;
}

.choice:has(input:checked) {
  border-color: var(--accent);
  background: var(--accent-tint);
}

.choice input {
  margin-top: 3px;
  accent-color: var(--accent);
}

.choice > span {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.choice-title {
  font-weight: 500;
}

.choice-help {
  font-size: 12px;
  color: var(--text-muted);
}

.notice {
  margin: 0;
}
</style>
