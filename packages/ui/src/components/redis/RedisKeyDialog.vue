<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, useId } from 'vue';
import type { KeyEdit } from '../../api/redis';
import { ttlMsFromSeconds } from '../../lib/redisEdit';
import { keyRenameSchema, keyTtlSchema } from '../../lib/redisForms';
import { requiredFields } from '../../lib/schemaRequired';
import FormField from '../common/FormField.vue';
import Modal from '../common/Modal.vue';

/** Rename a key (RENAME / RENAMENX) or set or remove its TTL (PEXPIRE / PERSIST). Emits the edit to plan. */
const props = defineProps<{ mode: 'rename' | 'ttl'; keyName: string; hasTtl: boolean }>();
const emit = defineEmits<{ done: [edit: KeyEdit | null] }>();

const uid = useId();

const renameSchema = keyRenameSchema(props.keyName);
const rename = useForm({
  validationSchema: toTypedSchema(renameSchema),
  initialValues: { newKey: props.keyName, replace: false },
});
const [newKey, newKeyAttrs] = rename.defineField('newKey', { validateOnModelUpdate: false });
const [replace] = rename.defineField('replace');
const renameRequired = computed(() => requiredFields(renameSchema, rename.values, ['newKey']));

const ttl = useForm({
  validationSchema: toTypedSchema(keyTtlSchema),
  initialValues: { mode: 'expire' as const, seconds: '' },
});
const [ttlMode] = ttl.defineField('mode');
const [seconds, secondsAttrs] = ttl.defineField('seconds', { validateOnModelUpdate: false });
const ttlRequired = computed(() => requiredFields(keyTtlSchema, ttl.values, ['seconds']));

const submitRename = rename.handleSubmit((v) => emit('done', { op: 'rename', newKey: v.newKey, replace: v.replace }));
const submitTtl = ttl.handleSubmit((v) => {
  if (v.mode === 'persist') {
    emit('done', { op: 'persist' });
    return;
  }
  const ms = ttlMsFromSeconds(v.seconds);
  if (ms !== null) emit('done', { op: 'expire', ttlMs: ms });
});
</script>

<template>
  <Modal
    :title="mode === 'rename' ? `Rename ${keyName}` : `TTL of ${keyName}`"
    :width="480"
    @close="emit('done', null)"
  >
    <form
      v-if="mode === 'rename'"
      :id="`${uid}-form`"
      class="content"
      novalidate
      @submit.prevent="submitRename"
    >
      <FormField
        label="New name"
        :input-id="`${uid}-name`"
        :required="renameRequired.has('newKey')"
        :error="rename.errors.value.newKey ?? ''"
      >
        <input
          :id="`${uid}-name`"
          v-model="newKey"
          v-bind="newKeyAttrs"
          class="input mono"
          spellcheck="false"
          autocomplete="off"
          :aria-invalid="!!rename.errors.value.newKey"
          data-autofocus
        >
      </FormField>
      <label class="check"><input
        v-model="replace"
        type="checkbox"
      > Overwrite a key that already has this name (RENAME instead of RENAMENX)</label>
    </form>
    <form
      v-else
      :id="`${uid}-form`"
      class="content"
      novalidate
      @submit.prevent="submitTtl"
    >
      <div
        class="choices"
        role="radiogroup"
        aria-label="TTL"
      >
        <label class="check"><input
          v-model="ttlMode"
          type="radio"
          value="expire"
        > Expire after</label>
        <label class="check"><input
          v-model="ttlMode"
          type="radio"
          value="persist"
          :disabled="!hasTtl"
        > Remove the TTL (PERSIST)</label>
      </div>
      <FormField
        v-if="ttlMode === 'expire'"
        label="Seconds"
        :input-id="`${uid}-seconds`"
        :required="ttlRequired.has('seconds')"
        :error="ttl.errors.value.seconds ?? ''"
      >
        <input
          :id="`${uid}-seconds`"
          v-model="seconds"
          v-bind="secondsAttrs"
          class="input mono"
          inputmode="numeric"
          autocomplete="off"
          :aria-invalid="!!ttl.errors.value.seconds"
          data-autofocus
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
  gap: 14px;
}

.choices {
  display: flex;
  gap: 16px;
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
}
</style>
