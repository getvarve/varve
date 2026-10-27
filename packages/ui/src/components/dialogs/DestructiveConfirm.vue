<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, useId } from 'vue';
import type { Environment } from '../../api/records';
import { describeStatements, needsTypedName } from '../../lib/destructive';
import { destructiveFormSchema } from '../../lib/forms';
import { requiredFields } from '../../lib/schemaRequired';
import FormField from '../common/FormField.vue';
import EnvBadge from '../common/EnvBadge.vue';
import Modal from '../common/Modal.vue';

/** Confirms a script with destructive statements (design doc 3.2). Production needs the connection name typed. */
const props = withDefaults(defineProps<{ connectionName: string; environment: Environment; statements: string[]; warnings?: string[] }>(), {
  warnings: () => [],
});
const emit = defineEmits<{ done: [answer: boolean] }>();

const schema = destructiveFormSchema(props.environment, props.connectionName);
const { values, errors, defineField, handleSubmit } = useForm({
  validationSchema: toTypedSchema(schema),
  initialValues: { typed: '' },
});
const [typed, typedAttrs] = defineField('typed', { validateOnModelUpdate: false });
const required = computed(() => requiredFields(schema, values, ['typed']));
const inputId = useId();
const descId = useId();
const kinds = computed(() => describeStatements(props.statements));
const typing = computed(() => needsTypedName(props.environment));
/** Run stays disabled until the schema accepts the form (the typed name on production). */
const enabled = computed(() => schema.safeParse(values).success);

const run = handleSubmit(() => {
  emit('done', true);
});
</script>

<template>
  <Modal
    :title="`Run destructive statements on ${connectionName}?`"
    :width="560"
    role="alertdialog"
    :described-by="descId"
    @close="emit('done', false)"
  >
    <template #badges>
      <EnvBadge :environment="environment" />
    </template>

    <form
      class="content"
      @submit.prevent="run"
    >
      <p
        :id="descId"
        class="lede"
      >
        This script contains statements that remove or overwrite data. They run on
        <strong>{{ connectionName }}</strong> as soon as you confirm.
      </p>
      <p
        v-for="warning in warnings"
        :key="warning"
        class="warning"
        role="note"
      >
        {{ warning }}
      </p>
      <ul
        class="kinds"
        aria-label="Destructive statements"
      >
        <li
          v-for="kind in kinds"
          :key="kind.label"
        >
          <span class="mono">{{ kind.label }}</span>
          <span
            v-if="kind.count > 1"
            class="subtle"
          >× {{ kind.count }}</span>
        </li>
      </ul>
      <FormField
        v-if="typing"
        label="Type the connection name to confirm"
        :input-id="inputId"
        :required="required.has('typed')"
        :error="errors.typed ?? ''"
      >
        <input
          :id="inputId"
          v-model="typed"
          v-bind="typedAttrs"
          class="input mono"
          autocomplete="off"
          spellcheck="false"
          :placeholder="connectionName"
          :aria-invalid="!!errors.typed"
          data-autofocus
        >
      </FormField>
    </form>

    <template #footer>
      <button
        type="button"
        class="btn"
        :data-autofocus="typing ? undefined : true"
        @click="emit('done', false)"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn danger"
        :disabled="!enabled"
        @click="run"
      >
        Run
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.content {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.lede {
  margin: 0;
  color: var(--text-muted);
}

.warning {
  margin: 0;
  padding-left: 10px;
  border-left: 3px solid var(--danger);
  color: var(--text);
}

.kinds {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.kinds li {
  display: inline-flex;
  gap: 6px;
  align-items: center;
  height: 24px;
  padding: 0 8px;
  border-radius: var(--radius-control);
  background: var(--danger-tint);
  color: var(--danger-text);
  font-size: 12px;
}
</style>
