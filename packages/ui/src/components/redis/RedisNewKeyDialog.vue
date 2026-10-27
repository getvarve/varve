<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, useId } from 'vue';
import type { RedisEditRequest } from '../../api/redis';
import { newKeyEdits, newKeySchema, NEW_KEY_TYPES } from '../../lib/redisForms';
import type { NewKeyType } from '../../lib/redisForms';
import { requiredFields } from '../../lib/schemaRequired';
import FormField from '../common/FormField.vue';
import Modal from '../common/Modal.vue';

/**
 * Creates a key: name, type, its first element (Redis has no empty keys) and
 * an optional TTL. Emits the request to plan; the key must not exist yet.
 */
const props = defineProps<{ prefix: string; jsonAvailable: boolean }>();
const emit = defineEmits<{ done: [request: RedisEditRequest | null] }>();

const uid = useId();
const { values, errors, defineField, handleSubmit } = useForm({
  validationSchema: toTypedSchema(newKeySchema),
  initialValues: { key: props.prefix, type: 'string' as NewKeyType, value: '', second: '', field: '', ttlSeconds: '' },
});
const [key, keyAttrs] = defineField('key', { validateOnModelUpdate: false });
const [type] = defineField('type');
const [value, valueAttrs] = defineField('value', { validateOnModelUpdate: false });
const [second, secondAttrs] = defineField('second', { validateOnModelUpdate: false });
const [field, fieldAttrs] = defineField('field', { validateOnModelUpdate: false });
const [ttlSeconds, ttlAttrs] = defineField('ttlSeconds', { validateOnModelUpdate: false });
const required = computed(() => requiredFields(newKeySchema, values, ['key', 'field', 'second', 'value', 'ttlSeconds']));

const LABELS: Record<NewKeyType, string> = {
  string: 'String',
  hash: 'Hash',
  list: 'List',
  set: 'Set',
  zset: 'Sorted set',
  stream: 'Stream',
  json: 'JSON (RedisJSON)',
};
const types = computed(() => NEW_KEY_TYPES.filter((t) => t !== 'json' || props.jsonAvailable));

const valueLabel = computed(() => {
  switch (type.value) {
    case 'json':
      return 'JSON document';
    case 'list':
      return 'First element';
    case 'set':
    case 'zset':
      return 'First member';
    default:
      return 'Value';
  }
});

const submit = handleSubmit((v) => {
  emit('done', { key: v.key, expect: { state: 'absent' }, edits: newKeyEdits(v) });
});
</script>

<template>
  <Modal
    title="New key"
    :width="520"
    @close="emit('done', null)"
  >
    <form
      :id="`${uid}-form`"
      class="content"
      novalidate
      @submit.prevent="submit"
    >
      <FormField
        label="Key"
        :input-id="`${uid}-key`"
        :required="required.has('key')"
        :error="errors.key ?? ''"
      >
        <input
          :id="`${uid}-key`"
          v-model="key"
          v-bind="keyAttrs"
          class="input mono"
          spellcheck="false"
          autocomplete="off"
          :aria-invalid="!!errors.key"
          data-autofocus
        >
      </FormField>
      <FormField
        label="Type"
        :input-id="`${uid}-type`"
      >
        <select
          :id="`${uid}-type`"
          v-model="type"
          class="select"
        >
          <option
            v-for="t in types"
            :key="t"
            :value="t"
          >
            {{ LABELS[t] }}
          </option>
        </select>
      </FormField>
      <FormField
        v-if="type === 'hash' || type === 'stream'"
        label="Field"
        :input-id="`${uid}-field`"
        :required="required.has('field')"
        :error="errors.field ?? ''"
      >
        <input
          :id="`${uid}-field`"
          v-model="field"
          v-bind="fieldAttrs"
          class="input mono"
          spellcheck="false"
          :aria-invalid="!!errors.field"
        >
      </FormField>
      <FormField
        v-if="type === 'hash' || type === 'stream'"
        label="Value"
        :input-id="`${uid}-second`"
      >
        <input
          :id="`${uid}-second`"
          v-model="second"
          v-bind="secondAttrs"
          class="input mono"
          spellcheck="false"
        >
      </FormField>
      <FormField
        v-else
        :label="valueLabel"
        :input-id="`${uid}-value`"
        :required="required.has('value')"
        :error="errors.value ?? ''"
      >
        <textarea
          v-if="type === 'string' || type === 'json'"
          :id="`${uid}-value`"
          v-model="value"
          v-bind="valueAttrs"
          class="input mono area"
          rows="5"
          spellcheck="false"
          :aria-invalid="!!errors.value"
        />
        <input
          v-else
          :id="`${uid}-value`"
          v-model="value"
          v-bind="valueAttrs"
          class="input mono"
          spellcheck="false"
        >
      </FormField>
      <FormField
        v-if="type === 'zset'"
        label="Score"
        :input-id="`${uid}-score`"
        :required="required.has('second')"
        :error="errors.second ?? ''"
      >
        <input
          :id="`${uid}-score`"
          v-model="second"
          v-bind="secondAttrs"
          class="input mono"
          spellcheck="false"
          :aria-invalid="!!errors.second"
        >
      </FormField>
      <FormField
        label="TTL in seconds"
        help="Leave empty for a key that does not expire."
        :input-id="`${uid}-ttl`"
        :error="errors.ttlSeconds ?? ''"
      >
        <input
          :id="`${uid}-ttl`"
          v-model="ttlSeconds"
          v-bind="ttlAttrs"
          class="input mono"
          inputmode="numeric"
          :aria-invalid="!!errors.ttlSeconds"
        >
      </FormField>
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
        type="submit"
        class="btn primary"
        :form="`${uid}-form`"
      >
        Review
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.content {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.area {
  height: auto;
  min-height: 96px;
  padding: 8px 10px;
  resize: vertical;
}
</style>
