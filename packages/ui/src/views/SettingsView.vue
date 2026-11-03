<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, onBeforeUnmount, onMounted, reactive, ref, useId, watch } from 'vue';
import { fetchActivitySettings, saveActivitySettings } from '../api/activity';
import type { TextLevel } from '../api/activity';
import { presentError, toCmdError } from '../api/errors';
import { claudeHttpCommand, claudeStdioCommand, fetchMcpStatus, rotateMcpToken, saveMcpSettings } from '../api/mcp';
import type { McpSettings, McpStatus } from '../api/mcp';
import { fetchSecretSettings, forgetCachedSecrets, saveSecretSettings } from '../api/secrets';
import ExtensionSlot from '../components/ExtensionSlot.vue';
import FormField from '../components/common/FormField.vue';
import Icon from '../components/common/Icon.vue';
import { useDialogs } from '../composables/useDialogs';
import { useExternalChanges } from '../composables/useExternalChanges';
import { registerUnsavedCheck } from '../composables/useQuitGuard';
import { useToasts } from '../composables/useToasts';
import { useUpdates } from '../composables/useUpdates';
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

const ui = reactive<{
  loading: boolean;
  saving: boolean;
  loadError: string | null;
  storedDays: number | null;
  changedElsewhere: boolean;
}>({
  loading: true,
  saving: false,
  loadError: null,
  storedDays: null,
  changedElsewhere: false,
});

async function load(): Promise<void> {
  // A read of outside changes started before this one is older than it.
  externalReload += 1;
  ui.loading = true;
  try {
    const settings = await fetchActivitySettings();
    resetForm({ values: activitySettingsForm(settings) });
    ui.storedDays = settings.retentionDays;
    ui.loadError = null;
    ui.changedElsewhere = false;
  } catch (error: unknown) {
    ui.loadError = presentError(toCmdError(error)).message;
  } finally {
    ui.loading = false;
    runQueuedReload();
  }
}

onMounted(load);

/**
 * The stored activity settings changed outside this page (an MCP tool). The
 * stored retention is always taken over, so saving still asks before it
 * deletes events; the form follows only while it holds no edits, checked
 * again once the values are in.
 */
let externalReload = 0;
let externalReadsInFlight = 0;
/** The read of outside changes running now, for a save to wait on. */
let externalRead: Promise<void> | null = null;
let reloadQueued = false;

/** A save starts: a read of outside changes still running may return what the save replaces, so it is dropped and done again after the save. */
function invalidateExternalReads(): void {
  externalReload += 1;
  if (externalReadsInFlight > 0) reloadQueued = true;
}

function reloadChangedElsewhere(): Promise<void> {
  if (ui.loadError) return Promise.resolve();
  // A load or save in flight would race the read: read again once it is done.
  if (ui.loading || ui.saving) {
    reloadQueued = true;
    return Promise.resolve();
  }
  const read = readChangedElsewhere().finally(() => {
    if (externalRead === read) externalRead = null;
  });
  externalRead = read;
  return read;
}

/** Runs a reload that was asked for while a load or save ran. */
function runQueuedReload(): void {
  if (!reloadQueued) return;
  reloadQueued = false;
  void reloadChangedElsewhere();
}

async function readChangedElsewhere(): Promise<void> {
  const request = ++externalReload;
  externalReadsInFlight += 1;
  try {
    const settings = await fetchActivitySettings();
    // Only the newest read counts, and none that a save overtook.
    if (request !== externalReload) return;
    ui.storedDays = settings.retentionDays;
    if (meta.value.dirty) {
      ui.changedElsewhere = true;
    } else {
      resetForm({ values: activitySettingsForm(settings) });
      ui.changedElsewhere = false;
    }
  } catch (error: unknown) {
    // Saving stays possible: it still asks before deleting events, against
    // the retention read last.
    if (request === externalReload) ui.changedElsewhere = true;
    toasts.pushError(error, 'Could not read the settings that changed outside this page');
  } finally {
    externalReadsInFlight -= 1;
  }
}

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
  // A read of outside changes may be about to bring a newer stored
  // retention: the delete warning below must compare against that one.
  if (externalRead) await externalRead;
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
  invalidateExternalReads();
  ui.saving = true;
  try {
    const removed = await saveActivitySettings(settings);
    ui.storedDays = settings.retentionDays;
    ui.changedElsewhere = false;
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
    runQueuedReload();
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

// Updates: hidden when this build has no update feed (dev builds), or before
// the backend answered. On macOS Sparkle shows its own windows.
const updates = useUpdates();
const updateInfo = computed(() => {
  const info = updates.state.info;
  return info && info.mechanism !== 'none' ? info : null;
});
const autoCheckBusy = ref(false);

async function toggleAutoCheck(): Promise<void> {
  if (!updateInfo.value || autoCheckBusy.value) return;
  autoCheckBusy.value = true;
  try {
    await updates.setAutoCheck(!updateInfo.value.autoCheck);
  } finally {
    autoCheckBusy.value = false;
  }
}

// Local MCP server (app/src-tauri/src/mcp): off by default; while on, MCP
// clients on this computer that can read the token file change records and
// settings through the app.
const mcp = reactive<{ loading: boolean; busy: boolean; loadError: string | null; status: McpStatus | null; port: string }>({
  loading: true,
  busy: false,
  loadError: null,
  status: null,
  port: '',
});

async function loadMcp(): Promise<void> {
  mcp.loading = true;
  try {
    mcp.status = await fetchMcpStatus();
    mcp.port = String(mcp.status.settings.port);
    mcp.loadError = null;
  } catch (error: unknown) {
    mcp.loadError = presentError(toCmdError(error)).message;
  } finally {
    mcp.loading = false;
  }
}

onMounted(loadMcp);

const mcpPortError = computed(() => {
  const text = mcp.port.trim();
  if (!/^\d+$/.test(text)) return 'Enter a port number.';
  const port = Number(text);
  return port < 1024 || port > 65535 ? 'Pick a port from 1024 to 65535.' : null;
});
const mcpPortChanged = computed(() => mcp.status !== null && mcp.port.trim() !== String(mcp.status.settings.port));
const mcpStdioCommand = computed(() => (mcp.status ? claudeStdioCommand(mcp.status) : null));
const mcpHttpCommand = computed(() => (mcp.status ? claudeHttpCommand(mcp.status) : null));

/** Saves the MCP settings; `fromPortField` when the port comes from the field, which then shows the stored one. */
async function applyMcp(settings: McpSettings, fromPortField: boolean): Promise<void> {
  if (mcp.busy) return;
  mcp.busy = true;
  try {
    mcp.status = await saveMcpSettings(settings);
    if (fromPortField) mcp.port = String(mcp.status.settings.port);
    if (!mcp.status.error) {
      toasts.push('success', mcp.status.url ? 'The local MCP server is on.' : 'The local MCP server is off.');
    }
  } catch (error: unknown) {
    toasts.pushError(error, 'Could not save the MCP settings');
  } finally {
    mcp.busy = false;
  }
}

function toggleMcp(): void {
  if (!mcp.status) return;
  // The stored port: a port typed but not applied stays in the field.
  void applyMcp({ enabled: !mcp.status.settings.enabled, port: mcp.status.settings.port }, false);
}

function saveMcpPort(): void {
  if (!mcp.status || mcpPortError.value) return;
  void applyMcp({ enabled: mcp.status.settings.enabled, port: Number(mcp.port.trim()) }, true);
}

async function rotateMcp(): Promise<void> {
  if (mcp.busy) return;
  const ok = await dialogs.askConfirm({
    title: 'Create a new MCP token?',
    message: 'Clients that send the current token are refused from their next request on. Add them again with the new token.',
    confirmLabel: 'New token',
    danger: true,
  });
  if (!ok) return;
  mcp.busy = true;
  try {
    mcp.status = await rotateMcpToken();
    toasts.push('success', 'New MCP token created.', 'The stdio command reads it by itself; HTTP clients need it again.');
  } catch (error: unknown) {
    toasts.pushError(error, 'Could not create a new MCP token');
  } finally {
    mcp.busy = false;
  }
}

async function copyCommand(command: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(command);
    toasts.push('success', 'Command copied', 'Paste it into a terminal.');
  } catch (error: unknown) {
    toasts.pushError(error, 'Could not copy the command');
  }
}

// An MCP client changed settings: show the stored values, but never throw
// away edits the user has not saved yet.
watch(useExternalChanges().settingsRevision, () => {
  void reloadChangedElsewhere();
  void loadSecrets();
});

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
        v-if="ui.changedElsewhere && !ui.loadError"
        class="notice"
        role="status"
      >
        <Icon name="info" />
        <span>These settings were changed outside this page while you were editing them. Saving stores what you see
          here.</span>
        <button
          type="button"
          class="btn compact"
          @click="load"
        >
          Show stored values
        </button>
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

    <section
      v-if="updateInfo"
      class="card"
      :aria-labelledby="`${uid}-updates`"
    >
      <h2 :id="`${uid}-updates`">
        Updates
      </h2>
      <p class="muted">
        You have Varve <span class="mono">{{ updateInfo.currentVersion }}</span>.
      </p>
      <div class="switch-row">
        <button
          type="button"
          role="switch"
          class="switch"
          :aria-checked="updateInfo.autoCheck"
          :aria-labelledby="`${uid}-auto-check`"
          :disabled="autoCheckBusy"
          @click="toggleAutoCheck"
        />
        <span
          :id="`${uid}-auto-check`"
          class="choice-title"
        >Automatically check for updates</span>
      </div>
      <div class="actions">
        <button
          type="button"
          class="btn"
          @click="updates.checkNow()"
        >
          Check for Updates…
        </button>
      </div>
    </section>

    <section
      class="card"
      :aria-labelledby="`${uid}-mcp`"
      :aria-busy="mcp.loading"
    >
      <h2 :id="`${uid}-mcp`">
        Local MCP server
      </h2>
      <p class="muted">
        Lets AI tools on this computer, such as Claude Code, read and change your folders, connections, saved queries
        and settings through the Model Context Protocol, while Varve runs. Passwords are never shared. Only programs
        that can read the token file get in, and web pages are refused.
      </p>
      <p
        v-if="mcp.loadError"
        class="notice tone-danger"
        role="alert"
      >
        <Icon name="warning" />
        <span>Could not load the MCP settings: {{ mcp.loadError }}</span>
        <button
          type="button"
          class="btn compact"
          @click="loadMcp"
        >
          Try again
        </button>
      </p>
      <template v-if="mcp.status">
        <div class="switch-row">
          <button
            type="button"
            role="switch"
            class="switch"
            :aria-checked="mcp.status.settings.enabled"
            :aria-labelledby="`${uid}-mcp-enabled`"
            :disabled="mcp.busy"
            @click="toggleMcp"
          />
          <span
            :id="`${uid}-mcp-enabled`"
            class="choice-title"
          >Serve MCP on this computer</span>
        </div>
        <p
          v-if="mcp.status.error"
          class="notice tone-danger"
          role="alert"
        >
          <Icon name="warning" />
          <span>{{ mcp.status.error }}</span>
        </p>
        <p
          v-else-if="mcp.status.url"
          class="muted"
        >
          Listening on <span class="mono">{{ mcp.status.url }}</span>.
        </p>
        <FormField
          label="Port"
          :input-id="`${uid}-mcp-port`"
          :error="mcpPortError ?? ''"
          help="On 127.0.0.1 only. The default is 7465."
        >
          <div class="period-row">
            <input
              :id="`${uid}-mcp-port`"
              v-model="mcp.port"
              class="input amount"
              inputmode="numeric"
              :aria-invalid="!!mcpPortError"
              :aria-describedby="mcpPortError ? `${uid}-mcp-port-error` : `${uid}-mcp-port-help`"
              :disabled="mcp.busy"
              @keydown.enter.prevent="saveMcpPort"
            >
            <button
              v-if="mcpPortChanged"
              type="button"
              class="btn"
              :disabled="mcp.busy || !!mcpPortError"
              @click="saveMcpPort"
            >
              Use port
            </button>
          </div>
        </FormField>
        <div
          v-if="mcpStdioCommand"
          class="command"
        >
          <span class="choice-title">Add to Claude Code (stdio)</span>
          <span class="choice-help">Starts Varve's bridge, which reads the token by itself.</span>
          <div class="command-row">
            <code class="mono">{{ mcpStdioCommand }}</code>
            <button
              type="button"
              class="btn compact"
              @click="copyCommand(mcpStdioCommand)"
            >
              Copy
            </button>
          </div>
        </div>
        <div
          v-if="mcpHttpCommand"
          class="command"
        >
          <span class="choice-title">Add to Claude Code (HTTP)</span>
          <span class="choice-help">Sends the token from <span class="mono">{{ mcp.status.tokenFile }}</span> as a bearer token.</span>
          <div class="command-row">
            <code class="mono">{{ mcpHttpCommand }}</code>
            <button
              type="button"
              class="btn compact"
              @click="copyCommand(mcpHttpCommand)"
            >
              Copy
            </button>
          </div>
        </div>
        <div class="actions">
          <button
            type="button"
            class="btn"
            :disabled="mcp.busy"
            @click="rotateMcp"
          >
            New token…
          </button>
        </div>
      </template>
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

.switch-row {
  display: flex;
  gap: 10px;
  align-items: center;
}

.actions {
  display: flex;
  justify-content: flex-end;
}

.command {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.command-row {
  display: flex;
  gap: 8px;
  align-items: flex-start;
}

.command-row code {
  flex: 1;
  min-width: 0;
  padding: 6px 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  font-size: 12px;
  overflow-wrap: anywhere;
  user-select: text;
}
</style>
