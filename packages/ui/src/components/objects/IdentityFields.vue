<script setup lang="ts">
import { onMounted, ref, useId } from 'vue';
import { catalogSchemas } from '../../api/catalog';
import type { SessionInfo } from '../../api/sessions';
import FormField from '../common/FormField.vue';

/**
 * Name, schema, owner and comment of a PostgreSQL object. The schema list
 * comes from the catalog; any name can still be typed.
 */
const props = withDefaults(
  defineProps<{
    session: SessionInfo;
    errors: Record<string, string>;
    /** Hide the owner (objects without one). */
    noOwner?: boolean;
    /** Hide the schema (database-wide objects such as event triggers). */
    noSchema?: boolean;
    disabled?: boolean;
  }>(),
  { noOwner: false, noSchema: false, disabled: false },
);
const name = defineModel<string>('name', { required: true });
const schema = defineModel<string>('schema', { default: '' });
const owner = defineModel<string>('owner', { default: '' });
const comment = defineModel<string>('comment', { required: true });

const ids = { name: useId(), schema: useId(), owner: useId(), comment: useId(), list: useId() };
const schemas = ref<string[]>([]);

onMounted(async () => {
  if (props.noSchema || props.session.engine !== 'postgres' || !props.session.database) return;
  try {
    const list = await catalogSchemas(props.session.id, props.session.database);
    schemas.value = list.filter((s) => !s.system).map((s) => s.object.name);
  } catch {
    // The datalist only suggests names; typing one works without it.
    schemas.value = [];
  }
});
</script>

<template>
  <div class="identity">
    <FormField
      label="Name"
      :input-id="ids.name"
      required
      :error="errors.name ?? ''"
    >
      <input
        :id="ids.name"
        v-model="name"
        class="input mono"
        autocomplete="off"
        spellcheck="false"
        :disabled="disabled"
        :aria-invalid="!!errors.name"
      >
    </FormField>
    <FormField
      v-if="!noSchema"
      label="Schema"
      :input-id="ids.schema"
      required
      :error="errors.schema ?? ''"
    >
      <input
        :id="ids.schema"
        v-model="schema"
        class="input mono"
        :list="ids.list"
        autocomplete="off"
        spellcheck="false"
        :disabled="disabled"
        :aria-invalid="!!errors.schema"
      >
      <datalist :id="ids.list">
        <option
          v-for="s in schemas"
          :key="s"
          :value="s"
        />
      </datalist>
    </FormField>
    <FormField
      v-if="!noOwner"
      label="Owner"
      :input-id="ids.owner"
      help="A role name; blank keeps the current owner."
    >
      <input
        :id="ids.owner"
        v-model="owner"
        class="input mono"
        autocomplete="off"
        spellcheck="false"
        :disabled="disabled"
      >
    </FormField>
    <FormField
      label="Comment"
      :input-id="ids.comment"
      class="wide"
    >
      <textarea
        :id="ids.comment"
        v-model="comment"
        class="textarea"
        rows="2"
        :disabled="disabled"
      />
    </FormField>
  </div>
</template>

<style scoped>
.identity {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
  gap: 12px;
}

.wide {
  grid-column: 1 / -1;
}

.textarea {
  font-family: var(--font-sans);
  font-size: 13px;
}
</style>
