<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, useId } from 'vue';
import { hostKeyFormSchema } from '../../lib/forms';
import { requiredFields } from '../../lib/schemaRequired';
import Icon from '../common/Icon.vue';
import Modal from '../common/Modal.vue';

/**
 * Asks whether to trust the SSH server's host key. An unknown key is the
 * normal first connect; a changed key can mean an attack, so it gets a
 * strong warning and an explicit choice.
 */
const props = defineProps<{
  /** `host:port` of the SSH server. */
  server: string;
  code: 'host_key_unknown' | 'host_key_changed';
  fingerprints: string[];
}>();
const emit = defineEmits<{ done: [answer: boolean] }>();

const changed = computed(() => props.code === 'host_key_changed');
const recorded = computed(() => (changed.value ? (props.fingerprints[0] ?? '') : ''));
const presented = computed(() => (changed.value ? (props.fingerprints[1] ?? '') : (props.fingerprints[0] ?? '')));
const { values, errors, defineField, handleSubmit } = useForm({
  validationSchema: toTypedSchema(hostKeyFormSchema(props.code === 'host_key_changed')),
  initialValues: { acknowledged: false },
});
const [acknowledged] = defineField('acknowledged', { validateOnModelUpdate: true });
const ackRequired = computed(() => requiredFields(hostKeyFormSchema(changed.value), values, ['acknowledged']).has('acknowledged'));
/** The trust button stays disabled until the schema accepts the form. */
const canTrust = computed(() => hostKeyFormSchema(changed.value).safeParse(values).success);
const trust = handleSubmit(() => emit('done', true));
const descId = useId();
const ackId = useId();
</script>

<template>
  <Modal
    :title="changed ? 'The SSH host key has changed' : 'Trust this SSH server?'"
    :width="600"
    :role="changed ? 'alertdialog' : 'dialog'"
    :described-by="descId"
    @close="emit('done', false)"
  >
    <div class="content">
      <div
        v-if="changed"
        :id="descId"
        class="notice tone-danger warning"
        role="alert"
      >
        <Icon
          name="warning"
          :size="18"
        />
        <div>
          <strong>{{ server }} presented a different host key than the one you trusted before.</strong>
          <p>
            This happens when the server was reinstalled or its keys were rotated. It can also mean someone is
            intercepting the connection. Do not continue unless the server's administrator confirms the new fingerprint.
          </p>
        </div>
      </div>
      <p
        v-else
        :id="descId"
        class="lede"
      >
        This is the first connection to <span class="mono">{{ server }}</span> from this device. Compare the
        fingerprint with the one the server's administrator gave you before you trust it.
      </p>

      <dl class="keys">
        <template v-if="changed">
          <dt>Trusted before</dt>
          <dd class="mono">
            {{ recorded }}
          </dd>
        </template>
        <dt>{{ changed ? 'Presented now' : 'Fingerprint' }}</dt>
        <dd class="mono">
          {{ presented }}
        </dd>
      </dl>

      <div
        v-if="changed"
        class="ack"
      >
        <label
          class="check"
          :for="ackId"
        >
          <input
            :id="ackId"
            v-model="acknowledged"
            type="checkbox"
            :aria-required="ackRequired"
            :aria-invalid="!!errors.acknowledged"
            :aria-describedby="`${ackId}-msg`"
          >
          <span>I checked the new fingerprint with the server's administrator.<span
            v-if="ackRequired"
            class="required"
            aria-hidden="true"
          > *</span></span>
        </label>
        <span
          :id="`${ackId}-msg`"
          :class="errors.acknowledged ? 'error' : 'help'"
          :role="errors.acknowledged ? 'alert' : undefined"
        >{{ errors.acknowledged ?? 'Required before the new key can be trusted.' }}</span>
      </div>
    </div>

    <template #footer>
      <button
        type="button"
        class="btn"
        data-autofocus
        @click="emit('done', false)"
      >
        Cancel
      </button>
      <button
        v-if="changed"
        type="button"
        class="btn danger"
        :disabled="!canTrust"
        @click="trust"
      >
        Trust new key and connect
      </button>
      <button
        v-else
        type="button"
        class="btn primary"
        @click="trust"
      >
        Trust and connect
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
  align-items: flex-start;
  font-size: 13px;
}

.warning p {
  margin: 4px 0 0;
}

.keys {
  display: grid;
  grid-template-columns: 120px 1fr;
  gap: 8px 12px;
  margin: 0;
  padding: 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--ground);
}

.keys dt {
  color: var(--text-muted);
}

.keys dd {
  margin: 0;
  font-size: 12px;
  overflow-wrap: anywhere;
}

.ack {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.required {
  color: var(--danger);
}

.help {
  font-size: 12px;
  color: var(--text-subtle);
}

.error {
  font-size: 12px;
  color: var(--danger-text);
}

.check {
  display: flex;
  gap: 8px;
  align-items: center;
}

.check input {
  accent-color: var(--accent);
}
</style>
