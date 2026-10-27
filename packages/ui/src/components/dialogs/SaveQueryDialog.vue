<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, reactive, useId } from 'vue';
import { presentError, toCmdError } from '../../api/errors';
import { createQuery } from '../../api/records';
import type { SavedQuery } from '../../api/records';
import type { SaveQueryRequest } from '../../composables/useDialogs';
import { useWorkspace } from '../../composables/useWorkspace';
import { singleFlight } from '../../lib/async';
import { savedQueryFormSchema, toSavedQueryInput } from '../../lib/queryForm';
import { requiredFields } from '../../lib/schemaRequired';
import { indentLabel } from '../../lib/display';
import { folderOptions } from '../../lib/tree';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import Modal from '../common/Modal.vue';

/** Saves the editor text as a new saved query ("Save as"). */
const props = defineProps<{ request: SaveQueryRequest }>();
const emit = defineEmits<{ done: [answer: SavedQuery | null] }>();

const workspace = useWorkspace();
const uid = useId();
const folders = computed(() => folderOptions(workspace.records.value.folders));

const { values, errors, handleSubmit, defineField, setFieldError } = useForm({
  validationSchema: toTypedSchema(savedQueryFormSchema),
  initialValues: {
    name: props.request.name,
    folderId: props.request.folderId ?? props.request.connection.folderId ?? '',
    tags: props.request.tags.join(', '),
    sql: props.request.sql,
  },
});
const [name, nameAttrs] = defineField('name', { validateOnModelUpdate: false });
const [folderId] = defineField('folderId');
const [tags] = defineField('tags');

const required = computed(() => requiredFields(savedQueryFormSchema, values, ['name', 'sql']));
const ui = reactive<{ saving: boolean; error: string | null }>({ saving: false, error: null });

const save = handleSubmit(async (form) => {
  ui.saving = true;
  ui.error = null;
  try {
    const saved = await createQuery(toSavedQueryInput(form, props.request.connection.id, props.request.database));
    emit('done', saved);
  } catch (error: unknown) {
    const cmd = toCmdError(error);
    if (cmd.kind === 'invalid' && /name/.test(cmd.message)) setFieldError('name', cmd.message);
    else ui.error = presentError(cmd).message;
  } finally {
    ui.saving = false;
  }
});

/** One save at a time: Enter in a field and a click on Save cannot create two saved queries. */
const saveFlight = singleFlight(async () => {
  await save();
});
const submit = (): Promise<void> => saveFlight.run();
</script>

<template>
  <Modal
    title="Save query"
    :width="520"
    @close="emit('done', null)"
  >
    <form
      class="form"
      novalidate
      @submit.prevent="submit"
    >
      <FormField
        label="Name"
        :input-id="`${uid}-name`"
        :required="required.has('name')"
        :error="errors.name ?? ''"
      >
        <input
          :id="`${uid}-name`"
          v-model="name"
          v-bind="nameAttrs"
          class="input"
          :aria-invalid="!!errors.name"
          data-autofocus
        >
      </FormField>
      <FormField
        label="Folder"
        :input-id="`${uid}-folder`"
      >
        <select
          :id="`${uid}-folder`"
          v-model="folderId"
          class="select"
        >
          <option value="">
            Top level
          </option>
          <option
            v-for="f in folders"
            :key="f.id"
            :value="f.id"
          >
            {{ indentLabel(f.name, f.depth) }}
          </option>
        </select>
      </FormField>
      <FormField
        label="Tags"
        :input-id="`${uid}-tags`"
        help="Separate tags with commas."
      >
        <input
          :id="`${uid}-tags`"
          v-model="tags"
          class="input"
        >
      </FormField>
      <p class="subtle meta">
        Attached to {{ request.connection.name }}. The query also appears under that connection in the sidebar.
      </p>
      <p
        v-if="errors.sql || ui.error"
        class="notice tone-danger"
        role="alert"
      >
        <Icon name="warning" />
        <span>{{ errors.sql ?? ui.error }}</span>
      </p>
      <button
        type="submit"
        class="visually-hidden"
        tabindex="-1"
        aria-hidden="true"
      >
        Save
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
        :disabled="ui.saving"
        @click="submit"
      >
        Save
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.form {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.meta {
  margin: 0;
  font-size: 12px;
}

.notice {
  margin: 0;
}
</style>
