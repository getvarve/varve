<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, ref, useId } from 'vue';
import { secretLabel } from '../../api/errors';
import type { ConnectionProfile, SecretKindName } from '../../api/records';
import type { SecretAnswer } from '../../composables/useDialogs';
import { hostPort, TLS_MODE_INFO } from '../../lib/display';
import { secretFormSchema } from '../../lib/forms';
import { requiredFields } from '../../lib/schemaRequired';
import FormField from '../common/FormField.vue';
import EngineTile from '../common/EngineTile.vue';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';
import Modal from '../common/Modal.vue';

/** Asks for a database password, SSH password or passphrase, or TLS key passphrase that is not saved (or was wrong). */
const props = defineProps<{ profile: ConnectionProfile; secret: SecretKindName; reason: string | null }>();
const emit = defineEmits<{ done: [answer: SecretAnswer | null] }>();

const schema = secretFormSchema(props.secret);
const { values, errors, defineField, handleSubmit } = useForm({
  validationSchema: toTypedSchema(schema),
  initialValues: { value: '', save: true },
});
const [value, valueAttrs] = defineField('value', { validateOnModelUpdate: false });
const [save] = defineField('save');
const required = computed(() => requiredFields(schema, values, ['value']));
const reveal = ref(false);
const inputId = useId();
const descId = useId();

const label = computed(() => {
  const text = secretLabel(props.secret);
  return text.charAt(0).toUpperCase() + text.slice(1);
});
const server = computed(() => {
  const p = props.profile;
  const db = p.engine === 'redis' ? `db${p.options.redisDb}` : (p.database ?? '');
  return `${hostPort(p.host, p.port)}${db ? ` / ${db}` : ''}`;
});
const user = computed(() => {
  if (props.secret === 'db_password') return props.profile.username ?? '';
  if (props.secret === 'tls_key_passphrase') return '';
  return props.profile.ssh.username;
});
/** The encrypted client key the TLS passphrase unlocks. */
const keyFile = computed(() => (props.secret === 'tls_key_passphrase' ? (props.profile.tls.clientKeyPath ?? '') : ''));

const submit = handleSubmit((form) => {
  emit('done', { value: form.value, save: form.save });
});
</script>

<template>
  <Modal
    :title="`Connect to ${profile.name}`"
    :width="520"
    :described-by="descId"
    @close="emit('done', null)"
  >
    <template #lead>
      <EngineTile :engine="profile.engine" />
    </template>
    <template #badges>
      <EnvBadge :environment="profile.environment" />
    </template>

    <form
      class="form"
      @submit.prevent="submit"
    >
      <p
        :id="descId"
        class="lede"
      >
        Enter the {{ secretLabel(secret) }} for this connection. Secrets are never stored in the connection profile.
      </p>

      <p
        v-if="reason"
        class="notice tone-danger"
        role="alert"
      >
        <Icon name="warning" />
        <span>{{ reason }}</span>
      </p>

      <dl class="facts">
        <dt>Server</dt>
        <dd class="mono">
          {{ server }}
        </dd>
        <template v-if="profile.ssh.enabled">
          <dt>Through</dt>
          <dd class="mono">
            {{ profile.ssh.username }}@{{ hostPort(profile.ssh.host, profile.ssh.port) }}
          </dd>
        </template>
        <template v-if="profile.engine !== 'redis' || profile.tls.mode !== 'prefer'">
          <dt>TLS</dt>
          <dd class="mono">
            {{ TLS_MODE_INFO[profile.tls.mode].label }}
          </dd>
        </template>
        <template v-if="keyFile">
          <dt>Key file</dt>
          <dd class="mono">
            {{ keyFile }}
          </dd>
        </template>
        <template v-if="user">
          <dt>User</dt>
          <dd class="mono">
            {{ user }}
          </dd>
        </template>
      </dl>

      <FormField
        :label="label"
        :input-id="inputId"
        :required="required.has('value')"
        :error="errors.value ?? ''"
      >
        <div class="secret">
          <input
            :id="inputId"
            v-model="value"
            v-bind="valueAttrs"
            class="input"
            :type="reveal ? 'text' : 'password'"
            autocomplete="current-password"
            spellcheck="false"
            :aria-invalid="!!errors.value"
            data-autofocus
          >
          <button
            type="button"
            class="icon-btn"
            :aria-label="reveal ? `Hide ${secretLabel(secret)}` : `Show ${secretLabel(secret)}`"
            :aria-pressed="reveal"
            @click="reveal = !reveal"
          >
            <Icon :name="reveal ? 'eyeOff' : 'eye'" />
          </button>
        </div>
      </FormField>

      <label class="check">
        <input
          v-model="save"
          type="checkbox"
        >
        <span>
          Save in keychain on this device
          <span class="help">Stored in the system keychain only, never in Varve's data or exports. Unchecked, it is kept in memory until you quit.</span>
        </span>
      </label>
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
        Connect
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

.lede {
  margin: 0;
  color: var(--text-muted);
}

.notice {
  margin: 0;
  align-items: flex-start;
}

.facts {
  display: grid;
  grid-template-columns: 80px 1fr;
  gap: 6px 12px;
  margin: 0;
  padding: 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--ground);
  font-size: 12px;
}

.facts dt {
  color: var(--text-muted);
}

.facts dd {
  margin: 0;
  overflow-wrap: anywhere;
}

.secret {
  display: flex;
  gap: 4px;
  align-items: center;
}

.check {
  display: flex;
  gap: 8px;
  align-items: flex-start;
  cursor: pointer;
}

.check input {
  margin-top: 3px;
  accent-color: var(--accent);
}

.check > span {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.help {
  font-size: 12px;
  color: var(--text-muted);
}
</style>
