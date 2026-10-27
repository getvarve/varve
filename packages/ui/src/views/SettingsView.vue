<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, onBeforeUnmount, onMounted, reactive, useId } from 'vue';
import { fetchActivitySettings, saveActivitySettings } from '../api/activity';
import type { TextLevel } from '../api/activity';
import { presentError, toCmdError } from '../api/errors';
import { fetchSecretSettings, forgetCachedSecrets, saveSecretSettings } from '../api/secrets';
import ExtensionSlot from '../components/ExtensionSlot.vue';
import FormField from '../components/common/FormField.vue';
import Icon from '../components/common/Icon.vue';
import { useDialogs } from '../composables/useDialogs';
import { registerUnsavedCheck } from '../composables/useQuitGuard';
import { useToasts } from '../composables/useToasts';
import { activitySettingsForm, activitySettingsFrom, activitySettingsSchema, retentionShortened } from '../lib/forms';
import type { ActivitySettingsValues } from '../lib/forms';
import { requiredFields } from '../lib/schemaRequired';
import { formatCount } from '../lib/values';

/** Settings, starting with the stored activity log settings (design doc 9.3 and 9.5). */
const toasts = useToasts();
const dialogs = useDialogs();
const uid = useId();

const TEXT_LEVELS: { value: TextLevel; label: string; help: string }[] = [
  { value: 'none', label: 'None', help: 'Only the statement kind, duration and row counts. No statement text is stored.' },
  {
    value: 'normalized',
    label: 'Normalized (default)',
    help: 'Statement text with literals replaced by ?. Redis arguments are replaced too, except key names.',
  },
  { value: 'full', label: 'Full', help: 'The full statement text, still passed through the secret redactor.' },
];

const { values, errors, defineField, handleSubmit, resetForm, meta } = useForm({
  validationSchema: toTypedSchema(activitySettingsSchema),
  initialValues: activitySettingsForm({ textLevel: 'normalized', retentionDays: null }),
});
const [textLevel] = defineField('textLevel');
const [keep] = defineField('keep');
const [amount, amountAttrs] = defineField('amount', { validateOnModelUpdate: true });
const [unit] = defineField('unit');
const required = computed(() => requiredFields(activitySettingsSchema, values, ['amount']));

const ui = reactive<{ loading: boolean; saving: boolean; loadError: string | null; storedDays: number | null }>({
  loading: true,
  saving: false,
  loadError: null,
  storedDays: null,
});

async function load(): Promise<void> {
  ui.loading = true;
  try {
    const settings = await fetchActivitySettings();
    resetForm({ values: activitySettingsForm(settings) });
    ui.storedDays = settings.retentionDays;
    ui.loadError = null;
  } catch (error: unknown) {
    ui.loadError = presentError(toCmdError(error)).message;
  } finally {
    ui.loading = false;
  }
}

onMounted(load);

// Quitting with unsaved settings asks first.
const removeQuitCheck = registerUnsavedCheck(async () => {
  if (!meta.value.dirty) return true;
  return dialogs.askConfirm({
    title: 'Discard settings changes?',
    message: 'Your changes to the activity settings are not saved.',
    confirmLabel: 'Discard',
    danger: true,
  });
});
onBeforeUnmount(removeQuitCheck);

const save = handleSubmit(async (form) => {
  // Never store defaults over settings that could not be read.
  if (ui.saving || ui.loading || ui.loadError) return;
  const settings = activitySettingsFrom(form);
  if (settings.retentionDays !== null && retentionShortened(ui.storedDays, settings.retentionDays)) {
    const ok = await dialogs.askConfirm({
      title: `Delete events older than ${formatCount(settings.retentionDays)} days?`,
      message: `Events older than ${formatCount(settings.retentionDays)} days will be deleted when you save, including their query history. This cannot be undone.`,
      confirmLabel: 'Save and delete',
      danger: true,
    });
    if (!ok) return;
  }
  ui.saving = true;
  try {
    const removed = await saveActivitySettings(settings);
    ui.storedDays = settings.retentionDays;
    resetForm({ values: activitySettingsForm(settings) });
    toasts.push(
      'success',
      'Saved.',
      removed === 0 ? 'No old events needed to be removed.' : `${formatCount(removed)} old ${removed === 1 ? 'event' : 'events'} removed.`,
    );
  } catch (error: unknown) {
    toasts.pushError(error, 'Could not save the activity settings');
  } finally {
    ui.saving = false;
  }
});

const form = computed(() => values as ActivitySettingsValues);

// Saved secrets: the keychain is read once per secret per app run while
// remembering is on (default). Changes apply at once.
const secrets = reactive<{ loading: boolean; busy: boolean; loadError: string | null; remember: boolean }>({
  loading: true,
  busy: false,
  loadError: null,
  remember: true,
});

async function loadSecrets(): Promise<void> {
  secrets.loading = true;
  try {
    const settings = await fetchSecretSettings();
    secrets.remember = settings.rememberForSession;
    secrets.loadError = null;
  } catch (error: unknown) {
    secrets.loadError = presentError(toCmdError(error)).message;
  } finally {
    secrets.loading = false;
  }
}

onMounted(loadSecrets);

async function setRemember(remember: boolean): Promise<void> {
  if (secrets.busy || secrets.loading || secrets.loadError) return;
  const before = secrets.remember;
  secrets.remember = remember;
  secrets.busy = true;
  try {
    await saveSecretSettings({ rememberForSession: remember });
    toasts.push('success', remember ? 'Secrets are remembered for this session.' : 'Secrets are read from the keychain on every connection.');
  } catch (error: unknown) {
    secrets.remember = before;
    toasts.pushError(error, 'Could not save the secret settings');
  } finally {
    secrets.busy = false;
  }
}

async function forgetSecrets(): Promise<void> {
  if (secrets.busy) return;
  secrets.busy = true;
  try {
    const count = await forgetCachedSecrets();
    toasts.push(
      'success',
      'Cached secrets forgotten.',
      count === 0 ? 'Nothing was cached.' : `${formatCount(count)} ${count === 1 ? 'secret' : 'secrets'} will be read from the keychain again.`,
    );
  } catch (error: unknown) {
    toasts.pushError(error, 'Could not forget the cached secrets');
  } finally {
    secrets.busy = false;
  }
}
</script>

<template>
  <section class="settings">
    <h1>Settings</h1>

    <form
      class="card"
      :aria-labelledby="`${uid}-activity`"
      :aria-busy="ui.loading"
      novalidate
      @submit.prevent="save"
    >
      <h2 :id="`${uid}-activity`">
        Activity log
      </h2>
      <p class="muted">
        The activity log stays on this device. The secret redactor always runs, whatever the text level.
      </p>
      <p
        v-if="ui.loadError"
        class="notice tone-danger"
        role="alert"
      >
        <Icon name="warning" />
        <span>Could not load the stored settings: {{ ui.loadError }} Saving is off, so the stored values are not
          overwritten.</span>
        <button
          type="button"
          class="btn compact"
          @click="load"
        >
          Try again
        </button>
      </p>

      <fieldset class="group">
        <legend class="label">
          Statement text
        </legend>
        <label
          v-for="level in TEXT_LEVELS"
          :key="level.value"
          class="choice"
        >
          <input
            v-model="textLevel"
            type="radio"
            :name="`${uid}-text-level`"
            :value="level.value"
          >
          <span>
            <span class="choice-title">{{ level.label }}</span>
            <span class="choice-help">{{ level.help }}</span>
          </span>
        </label>
        <p
          v-if="form.textLevel === 'full'"
          class="notice tone-warning"
          role="note"
        >
          <Icon name="warning" />
          <span>Statement text is stored as written, apart from detected secrets. Personal data in queries ends up in the
            log.</span>
        </p>
      </fieldset>

      <fieldset class="group">
        <legend class="label">
          Keep events
        </legend>
        <label class="choice inline">
          <input
            v-model="keep"
            type="radio"
            :name="`${uid}-keep`"
            value="forever"
          >
          <span class="choice-title">Forever (default)</span>
        </label>
        <label class="choice inline">
          <input
            v-model="keep"
            type="radio"
            :name="`${uid}-keep`"
            value="period"
          >
          <span class="choice-title">For a period</span>
        </label>
        <div
          v-if="form.keep === 'period'"
          class="period"
        >
          <FormField
            label="Keep for"
            :input-id="`${uid}-amount`"
            :required="required.has('amount')"
            :error="errors.amount ?? ''"
          >
            <div class="period-row">
              <input
                :id="`${uid}-amount`"
                v-model="amount"
                v-bind="amountAttrs"
                class="input amount"
                inputmode="numeric"
                :aria-invalid="!!errors.amount"
                :aria-describedby="errors.amount ? `${uid}-amount-error` : undefined"
              >
              <label
                class="visually-hidden"
                :for="`${uid}-unit`"
              >Unit</label>
              <select
                :id="`${uid}-unit`"
                v-model="unit"
                class="select unit"
              >
                <option value="days">
                  days
                </option>
                <option value="months">
                  months (30 days)
                </option>
                <option value="years">
                  years (365 days)
                </option>
              </select>
            </div>
          </FormField>
          <p class="subtle hint">
            Older events, including query history, are removed when you save, at startup and once a day.
          </p>
        </div>
      </fieldset>

      <div class="actions">
        <button
          type="submit"
          class="btn primary"
          :disabled="ui.saving || ui.loading || !!ui.loadError || !meta.dirty"
        >
          {{ ui.saving ? 'Saving…' : 'Save' }}
        </button>
      </div>
    </form>

    <section
      class="card"
      :aria-labelledby="`${uid}-secrets`"
      :aria-busy="secrets.loading"
    >
      <h2 :id="`${uid}-secrets`">
        Saved secrets
      </h2>
      <p class="muted">
        Passwords and passphrases live in the system keychain. Every keychain read can ask for permission, so Varve
        keeps a secret in memory after its first read and shares it with every session of that connection.
      </p>
      <p
        v-if="secrets.loadError"
        class="notice tone-danger"
        role="alert"
      >
        <Icon name="warning" />
        <span>Could not load the stored settings: {{ secrets.loadError }}</span>
        <button
          type="button"
          class="btn compact"
          @click="loadSecrets"
        >
          Try again
        </button>
      </p>
      <label class="choice">
        <input
          type="checkbox"
          :checked="secrets.remember"
          :disabled="secrets.busy || secrets.loading || !!secrets.loadError"
          @change="setRemember(($event.target as HTMLInputElement).checked)"
        >
        <span>
          <span class="choice-title">Remember secrets for this session (default)</span>
          <span class="choice-help">Each secret is read from the keychain once per app run. They are kept only in memory, never
            written anywhere, and wiped when Varve quits.</span>
        </span>
      </label>
      <div class="actions">
        <button
          type="button"
          class="btn"
          :disabled="secrets.busy"
          @click="forgetSecrets"
        >
          Forget cached secrets
        </button>
      </div>
    </section>

    <ExtensionSlot name="settings-sections" />
  </section>
</template>

<style scoped>
.settings {
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 720px;
  padding: 40px 48px;
}

h1 {
  margin: 0;
  font-size: 24px;
  font-weight: 600;
}

.card {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

h2 {
  margin: 0;
  font-size: 15px;
  font-weight: 500;
}

p {
  margin: 0;
}

.group {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 0;
  padding: 0;
  border: 0;
}

.group legend {
  margin-bottom: 4px;
  padding: 0;
}

.choice {
  display: flex;
  gap: 10px;
  align-items: flex-start;
  cursor: pointer;
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

.choice.inline {
  align-items: center;
}

.choice.inline input {
  margin-top: 0;
}

.choice-title {
  font-weight: 500;
}

.choice-help {
  font-size: 12px;
  color: var(--text-muted);
}

.period {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding-left: 24px;
}

.period-row {
  display: flex;
  gap: 8px;
}

.amount {
  width: 96px;
}

.unit {
  width: 180px;
}

.hint {
  font-size: 12px;
}

.notice {
  align-items: flex-start;
}

.actions {
  display: flex;
  justify-content: flex-end;
}
</style>
