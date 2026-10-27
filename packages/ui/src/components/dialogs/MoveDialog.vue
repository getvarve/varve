<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, useId } from 'vue';
import { useWorkspace } from '../../composables/useWorkspace';
import { indentLabel } from '../../lib/display';
import { moveFormSchema } from '../../lib/forms';
import { requiredFields } from '../../lib/schemaRequired';
import { moveTargets } from '../../lib/tree';
import type { TreeNode } from '../../lib/tree';
import FormField from '../common/FormField.vue';
import Modal from '../common/Modal.vue';

/** "Move to…": the keyboard way to move a folder, connection or saved query to another folder. */
const props = defineProps<{ node: TreeNode }>();
const emit = defineEmits<{ done: [answer: { folderId: string | null } | null] }>();

const workspace = useWorkspace();
const uid = useId();
const targets = computed(() => moveTargets(workspace.tree.value, props.node));
const schema = moveFormSchema(targets.value.map((t) => t.id));

const { values, errors, defineField, handleSubmit } = useForm({
  validationSchema: toTypedSchema(schema),
  initialValues: { folderId: props.node.parentId ?? '' },
});
const [folderId] = defineField('folderId');
const required = computed(() => requiredFields(schema, values, ['folderId']));

const submit = handleSubmit((form) => {
  emit('done', { folderId: form.folderId === '' ? null : form.folderId });
});
</script>

<template>
  <Modal
    :title="`Move “${node.name}”`"
    :width="440"
    @close="emit('done', null)"
  >
    <form
      novalidate
      @submit.prevent="submit"
    >
      <FormField
        label="Folder"
        :input-id="`${uid}-folder`"
        :required="required.has('folderId')"
        :error="errors.folderId ?? ''"
      >
        <select
          :id="`${uid}-folder`"
          v-model="folderId"
          class="select"
          data-autofocus
        >
          <option value="">
            Top level
          </option>
          <option
            v-for="f in targets"
            :key="f.id"
            :value="f.id"
          >
            {{ indentLabel(f.name, f.depth) }}
          </option>
        </select>
      </FormField>
      <button
        type="submit"
        class="visually-hidden"
        tabindex="-1"
        aria-hidden="true"
      >
        Move
      </button>
    </form>
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
        class="btn primary"
        @click="submit"
      >
        Move
      </button>
    </template>
  </Modal>
</template>
