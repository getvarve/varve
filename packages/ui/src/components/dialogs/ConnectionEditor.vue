<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { open as openFileDialog } from '@tauri-apps/plugin-dialog';
import { useForm } from 'vee-validate';
import type { Path } from 'vee-validate';
import { computed, onBeforeUnmount, reactive, ref, useId } from 'vue';
import { presentError, secretLabel, toCmdError } from '../../api/errors';
import { createConnection, defaultPort, ENGINES, ENVIRONMENTS, forgetSecret, saveSecret, trustSshHostKey, updateConnection } from '../../api/records';
import type { ConnectionInput, ConnectionProfile, Engine, SecretKindName, SshAuthMethod, TlsMode } from '../../api/records';
import { testConnection } from '../../api/sessions';
import type { ServerInfo, TestResult, TestTarget, TypedSecrets } from '../../api/sessions';
import { forgetTypedSecrets, rememberTypedSecrets, retainTypedSecretsAfterEdit, withConnectPrompts } from '../../composables/useConnect';
import { useDialogs } from '../../composables/useDialogs';
import { registerUnsavedCheck } from '../../composables/useQuitGuard';
import { useWorkspace } from '../../composables/useWorkspace';
import {
  COLOR_TOKENS,
  connectionFormFromProfile,
  connectionFormSchema,
  emptyConnectionForm,
  fieldForServerMessage,
  toConnectionSubmit,
} from '../../lib/connectionSchema';
import type { ConnectionFormValues } from '../../lib/connectionSchema';
import { nextConnectStep, sshSecretKind } from '../../lib/connectFlow';
import { colorVar, ENGINE_INFO, hostPort, indentLabel, ENVIRONMENT_HELP, ENVIRONMENT_INFO, TLS_MODE_INFO } from '../../lib/display';
import { singleFlight } from '../../lib/async';
import { requiredFields } from '../../lib/schemaRequired';
import { folderOptions } from '../../lib/tree';
import { formatDuration } from '../../lib/values';
import EngineTile from '../common/EngineTile.vue';
import EnvBadge from '../common/EnvBadge.vue';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import Modal from '../common/Modal.vue';

const props = defineProps<{ profile: ConnectionProfile | null; engine: Engine; folderId: string | null }>();
const emit = defineEmits<{ done: [answer: ConnectionProfile | null] }>();

const workspace = useWorkspace();
const dialogs = useDialogs();
const uid = useId();
const id = (name: string): string => `${uid}-${name}`;

/** The saved profile, once there is one (after the first save of a new connection too). */
const current = ref<ConnectionProfile | null>(props.profile);

const { values, errors, meta, handleSubmit, defineField, setFieldValue, setFieldError, validate, resetForm } = useForm({
  validationSchema: toTypedSchema(connectionFormSchema),
  initialValues: props.profile ? connectionFormFromProfile(props.profile) : emptyConnectionForm(props.engine, props.folderId),
});

/** The form values; always complete, because the form starts from complete initial values. */
const form = values as ConnectionFormValues;

const fieldOptions = { validateOnModelUpdate: false };
const [name, nameAttrs] = defineField('name', fieldOptions);
const [folderModel, folderAttrs] = defineField('folderId', fieldOptions);
const [host, hostAttrs] = defineField('host', fieldOptions);
const [port, portAttrs] = defineField('port', fieldOptions);
const [database, databaseAttrs] = defineField('database', fieldOptions);
const [username, usernameAttrs] = defineField('username', fieldOptions);
const [password] = defineField('password', fieldOptions);
const [savePassword] = defineField('savePassword', fieldOptions);
const [redisDb, redisDbAttrs] = defineField('options.redisDb', fieldOptions);
const [caPath, caPathAttrs] = defineField('tls.caPath', fieldOptions);
const [certPath, certPathAttrs] = defineField('tls.clientCertPath', fieldOptions);
const [keyPath, keyPathAttrs] = defineField('tls.clientKeyPath', fieldOptions);
const [tlsPassphrase] = defineField('tls.passphrase', fieldOptions);
const [saveTlsPassphrase] = defineField('tls.savePassphrase', fieldOptions);
const [sshHost, sshHostAttrs] = defineField('ssh.host', fieldOptions);
const [sshPort, sshPortAttrs] = defineField('ssh.port', fieldOptions);
const [sshUser, sshUserAttrs] = defineField('ssh.username', fieldOptions);
const [sshKeyPath, sshKeyPathAttrs] = defineField('ssh.keyPath', fieldOptions);
const [sshSecret] = defineField('ssh.secret', fieldOptions);
const [saveSshSecret] = defineField('ssh.saveSecret', fieldOptions);
const [timeout, timeoutAttrs] = defineField('options.statementTimeoutMs', fieldOptions);
const [keepalive, keepaliveAttrs] = defineField('options.keepaliveS', fieldOptions);
const [initSql, initSqlAttrs] = defineField('options.initSql', fieldOptions);

/** Fields that can show a required marker; which ones do is read from the schema. */
const MARKED_FIELDS = [
  'name',
  'host',
  'port',
  'database',
  'username',
  'options.redisDb',
  'tls.caPath',
  'tls.clientCertPath',
  'tls.clientKeyPath',
  'ssh.host',
  'ssh.port',
  'ssh.username',
  'ssh.keyPath',
] as const;
const required = computed(() => requiredFields(connectionFormSchema, values, MARKED_FIELDS));
const req = (path: (typeof MARKED_FIELDS)[number]): boolean => required.value.has(path);
const err = (path: string): string => (errors.value as Record<string, string | undefined>)[path] ?? '';
const describedBy = (path: string, field: string): string | undefined => (err(path) ? `${id(field)}-error` : undefined);

// Tabs

type TabName = 'general' | 'ssh' | 'tls' | 'advanced';
const TABS: { name: TabName; label: string }[] = [
  { name: 'general', label: 'General' },
  { name: 'ssh', label: 'SSH' },
  { name: 'tls', label: 'TLS' },
  { name: 'advanced', label: 'Advanced' },
];
const activeTab = ref<TabName>('general');

function tabOf(path: string): TabName {
  if (path.startsWith('ssh.')) return 'ssh';
  if (path.startsWith('tls.')) return 'tls';
  if (path === 'environment' || (path.startsWith('options.') && path !== 'options.redisDb')) return 'advanced';
  return 'general';
}

const tabErrors = computed(() => {
  const out = new Set<TabName>();
  for (const [path, message] of Object.entries(errors.value)) if (message) out.add(tabOf(path));
  return out;
});

function onTabKey(event: KeyboardEvent): void {
  const at = TABS.findIndex((t) => t.name === activeTab.value);
  let next: number;
  if (event.key === 'ArrowRight') next = (at + 1) % TABS.length;
  else if (event.key === 'ArrowLeft') next = (at - 1 + TABS.length) % TABS.length;
  else if (event.key === 'Home') next = 0;
  else if (event.key === 'End') next = TABS.length - 1;
  else return;
  event.preventDefault();
  const tab = TABS[next];
  if (!tab) return;
  activeTab.value = tab.name;
  document.getElementById(id(`tab-${tab.name}`))?.focus();
}

function showFirstError(paths: string[]): void {
  const first = paths.find((p) => err(p));
  if (first) activeTab.value = tabOf(first);
}

// Engine, environment, choices

const isRedis = computed(() => form.engine === 'redis');
const folders = computed(() => folderOptions(workspace.records.value.folders));

function chooseEngine(engine: Engine): void {
  const previous = form.engine;
  if (previous === engine) return;
  if (form.port === String(defaultPort(previous))) setFieldValue('port', '');
  if (engine === 'redis') setFieldValue('database', '');
  else setFieldValue('options.redisDb', '0');
  setFieldValue('engine', engine);
}

function onRadioKey<T>(event: KeyboardEvent, list: readonly T[], currentValue: T, choose: (value: T) => void): void {
  const at = list.indexOf(currentValue);
  let next: number;
  if (event.key === 'ArrowRight' || event.key === 'ArrowDown') next = (at + 1) % list.length;
  else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') next = (at - 1 + list.length) % list.length;
  else return;
  event.preventDefault();
  const value = list[next];
  if (value === undefined) return;
  choose(value);
  const group = (event.currentTarget as HTMLElement).closest('[role="radiogroup"]');
  group?.querySelectorAll<HTMLElement>('[role="radio"]')[next]?.focus();
}

const TLS_MODES: readonly TlsMode[] = ['disable', 'prefer', 'require', 'verify_ca', 'verify_full'];
const SSH_AUTHS: readonly { value: SshAuthMethod; label: string }[] = [
  { value: 'password', label: 'Password' },
  { value: 'key_file', label: 'Key file' },
  { value: 'agent', label: 'SSH agent' },
];
const COLORS = [...COLOR_TOKENS, ''] as const;

async function browse(field: 'tls.caPath' | 'tls.clientCertPath' | 'tls.clientKeyPath' | 'ssh.keyPath', title: string): Promise<void> {
  try {
    const picked = await openFileDialog({ title, multiple: false, directory: false });
    if (typeof picked === 'string') setFieldValue(field, picked);
  } catch (error: unknown) {
    ui.formError = presentError(toCmdError(error)).message;
  }
}

// Secrets

const ui = reactive<{
  saving: boolean;
  formError: string | null;
  changePassword: boolean;
  changeSshSecret: boolean;
  changeTlsPassphrase: boolean;
  test:
    | { state: 'idle' }
    | { state: 'running' }
    | { state: 'ok'; server: ServerInfo; latencyMs: number; at: number }
    | { state: 'error'; title: string; message: string };
}>({
  saving: false,
  formError: null,
  changePassword: false,
  changeSshSecret: false,
  changeTlsPassphrase: false,
  test: { state: 'idle' },
});

const savedPassword = computed(() => current.value?.savedSecrets.includes('db_password') ?? false);
const sshKind = computed<SecretKindName | null>(() => sshSecretKind({ ssh: { ...form.ssh, port: 22, keyPath: null } }));
const savedSshSecret = computed(() => (sshKind.value ? (current.value?.savedSecrets.includes(sshKind.value) ?? false) : false));
/** The client key passphrase field applies while TLS is on and a key file is set. */
const usesClientKey = computed(() => form.tls.mode !== 'disable' && form.tls.clientKeyPath.trim() !== '');
/** A saved passphrase only belongs to the key file it was saved for (the backend drops it when the path changes). */
const savedTlsPassphrase = computed(
  () =>
    (current.value?.savedSecrets.includes('tls_key_passphrase') ?? false) &&
    current.value?.tls.clientKeyPath === form.tls.clientKeyPath.trim(),
);

async function forget(kind: SecretKindName): Promise<void> {
  if (!current.value) return;
  try {
    current.value = await forgetSecret(current.value.id, kind);
    void workspace.refresh();
  } catch (error: unknown) {
    ui.formError = presentError(toCmdError(error)).message;
  }
}

// Save

/** True once this dialog stored the profile (create or update), even when a later step failed. */
const profileStored = ref(false);

const save = handleSubmit(
  async (formValues) => {
    ui.saving = true;
    ui.formError = null;
    const out = toConnectionSubmit(formValues);
    // Set when the edit was stored but old keychain entries could not be
    // removed: shown once the rest of the save is done.
    let keychainWarning: string | null = null;
    // Whether this attempt stored the profile (the dialog-wide flag only
    // remembers that some attempt did, for closing the dialog).
    let storedNow = false;
    try {
      const before = current.value;
      let saved: ConnectionProfile;
      if (before) {
        try {
          saved = await updateConnection(before.id, out.input);
        } catch (error: unknown) {
          const cmd = toCmdError(error);
          // `keychain` from update_connection means the edit is stored and
          // only removing the old server's keychain entries failed: carry on
          // with the stored profile as on success.
          if (cmd.kind !== 'keychain') throw error;
          // The error carries the profile exactly as this update stored it
          // (its version is what the typed secrets were entered for). Without
          // it, typed secrets cannot be checked, so none of the old server's
          // are kept.
          if (!cmd.profile || cmd.profile.id !== before.id) {
            forgetTypedSecrets(before.id);
            profileStored.value = true;
            storedNow = true;
            void workspace.refresh();
            ui.formError = `The connection was saved, but keychain entries of the previous server could not be removed (Varve retries later): ${presentError(cmd).message}`;
            return;
          }
          saved = cmd.profile;
          keychainWarning = presentError(cmd).message;
        }
      } else saved = await createConnection(out.input);
      // Secrets typed earlier for this connection must not follow it to a new server or tunnel.
      if (before) retainTypedSecretsAfterEdit(before, saved);
      current.value = saved;
      // The profile is stored now: make it the form's baseline (keeping the typed
      // secrets), so a later keychain failure never asks to "discard" stored changes.
      profileStored.value = true;
      storedNow = true;
      const baseline = connectionFormFromProfile(saved);
      resetForm({
        values: {
          ...baseline,
          password: formValues.password,
          savePassword: formValues.savePassword,
          tls: { ...baseline.tls, passphrase: formValues.tls.passphrase, savePassphrase: formValues.tls.savePassphrase },
          ssh: { ...baseline.ssh, secret: formValues.ssh.secret, saveSecret: formValues.ssh.saveSecret },
        },
      });
      void workspace.refresh();
      const remember: TypedSecrets = {};
      if (out.password !== null) {
        if (out.savePassword) {
          saved = await saveSecret(saved.id, 'db_password', out.password, saved.version);
          // Shown at once: a failing SSH secret save below must not hide it.
          current.value = saved;
        }
        else remember.password = out.password;
      }
      const kind = sshSecretKind(saved);
      if (out.sshSecret !== null && kind) {
        if (out.saveSshSecret) {
          saved = await saveSecret(saved.id, kind, out.sshSecret, saved.version);
          current.value = saved;
        }
        else remember.sshSecret = out.sshSecret;
      }
      if (out.tlsPassphrase !== null && saved.tls.clientKeyPath !== null) {
        if (out.saveTlsPassphrase) {
          saved = await saveSecret(saved.id, 'tls_key_passphrase', out.tlsPassphrase, saved.version);
          current.value = saved;
        } else remember.tlsPassphrase = out.tlsPassphrase;
      }
      if (remember.password !== undefined || remember.sshSecret !== undefined || remember.tlsPassphrase !== undefined) {
        rememberTypedSecrets(saved.id, remember);
      }
      current.value = saved;
      if (keychainWarning !== null) {
        // Stay open so the warning is read; the connection is saved.
        ui.formError = `The connection was saved, but keychain entries of the previous server could not be removed (Varve retries later): ${keychainWarning}`;
        return;
      }
      emit('done', saved);
    } catch (error: unknown) {
      const cmd = toCmdError(error);
      if (cmd.kind === 'invalid' && storedNow) {
        // A refused save_secret: the connection itself is saved.
        ui.formError = `The connection was saved, but the secret was not: ${cmd.message}`;
        void workspace.refresh();
      } else if (cmd.kind === 'invalid') {
        const field = fieldForServerMessage(cmd.message);
        if (field) {
          setFieldError(field as Path<ConnectionFormValues>, cmd.message);
          activeTab.value = tabOf(field);
        } else ui.formError = cmd.message;
      } else {
        const presented = presentError(cmd);
        ui.formError = storedNow
          ? `The connection was saved. The secret could not be stored in the keychain: ${presented.message}`
          : presented.message;
        if (storedNow) void workspace.refresh();
      }
    } finally {
      ui.saving = false;
    }
  },
  ({ errors: invalid }) => showFirstError(Object.keys(invalid)),
);

/** Save runs once at a time: Enter in a field and a click on Save cannot create two connections. */
const saveFlight = singleFlight(async () => {
  await save();
});
const submit = (): Promise<void> => saveFlight.run();

// Test

function secretHint(secret: SecretKindName): string {
  if (secret === 'db_password') return 'Enter the database password on the General tab to test.';
  if (secret === 'tls_key_passphrase') return 'The TLS client key is encrypted. Enter its passphrase on the TLS tab to test.';
  return `Enter the ${secretLabel(secret)} on the SSH tab to test.`;
}

async function test(): Promise<void> {
  const result = await validate();
  if (!result.valid) {
    showFirstError(Object.keys(result.errors));
    return;
  }
  const out = toConnectionSubmit(form);
  const typed: TypedSecrets = {};
  if (out.password !== null) typed.password = out.password;
  if (out.sshSecret !== null) typed.sshSecret = out.sshSecret;
  if (out.tlsPassphrase !== null) typed.tlsPassphrase = out.tlsPassphrase;
  ui.test = { state: 'running' };
  try {
    const saved = current.value;
    const unchanged = saved && JSON.stringify(toConnectionSubmit(connectionFormFromProfile(saved)).input) === JSON.stringify(out.input);
    let res: TestResult | null;
    if (saved && unchanged) {
      // The form's secrets go first; a prompt's answer replaces them (and its
      // "Save in keychain" choice applies), and the outdated form field is cleared.
      res = await withConnectPrompts(saved, (secrets) => testConnection({ kind: 'saved', id: saved.id }, secrets), {
        initial: typed,
        remember: false,
        onAnswered: (secret) => {
          if (secret === 'db_password') setFieldValue('password', '');
          else if (secret === 'tls_key_passphrase') setFieldValue('tls.passphrase', '');
          else setFieldValue('ssh.secret', '');
        },
      });
      const refreshed = workspace.connection(saved.id);
      if (refreshed) current.value = refreshed;
    } else {
      res = await testUnsaved(out.input, saved?.id ?? null, typed);
    }
    ui.test = res ? { state: 'ok', server: res.server, latencyMs: res.latencyMs, at: Date.now() } : { state: 'idle' };
  } catch (error: unknown) {
    const cmd = toCmdError(error);
    if (cmd.kind === 'secretRequired') {
      ui.test = { state: 'error', title: 'Secret needed', message: secretHint(cmd.secret) };
    } else {
      const presented = presentError(cmd);
      const extra = [presented.code, presented.detail, presented.hint].filter(Boolean).join(' · ');
      ui.test = { state: 'error', title: presented.title, message: extra ? `${presented.message} (${extra})` : presented.message };
    }
  }
}

/**
 * Tests editor input that is not saved. `basedOn` lets the backend reuse the
 * saved profile's keychain secrets while the server and account are the
 * same. An unknown or changed SSH host key is offered for trust and the test
 * runs again. Resolves `null` when the user declines.
 */
async function testUnsaved(input: ConnectionInput, basedOn: string | null, typed: TypedSecrets): Promise<TestResult | null> {
  const target: TestTarget = basedOn ? { kind: 'unsaved', input, basedOn } : { kind: 'unsaved', input };
  for (let round = 0; round < 3; round += 1) {
    try {
      return await testConnection(target, typed);
    } catch (error: unknown) {
      const cmd = toCmdError(error);
      const step = nextConnectStep(cmd, { ssh: input.ssh });
      if (step.step !== 'trustHostKey') throw error;
      const trusted = await dialogs.askHostKey({
        server: hostPort(step.host, step.port),
        code: step.code,
        fingerprints: step.fingerprints,
      });
      if (!trusted) return null;
      // The host and port come from the error: the server that presented the key.
      await trustSshHostKey(step.host, step.port, step.presentedKey);
    }
  }
  return null;
}

/** The editor's own discard question; `true` when nothing is unsaved or the user discards. */
async function mayDiscard(): Promise<boolean> {
  if (!meta.value.dirty) return true;
  return dialogs.askConfirm({
    title: 'Discard changes?',
    message: 'Your changes to this connection are not saved.',
    confirmLabel: 'Discard',
    danger: true,
  });
}

// Quitting with this dialog open asks the same question first.
const removeQuitCheck = registerUnsavedCheck(mayDiscard);
onBeforeUnmount(removeQuitCheck);

async function close(): Promise<void> {
  if (!(await mayDiscard())) return;
  // A profile stored by this dialog is reported back so the tree and tabs pick it up.
  emit('done', profileStored.value ? current.value : null);
}

const title = computed(() => (props.profile ? `Edit connection: ${props.profile.name}` : 'New connection'));
</script>

<template>
  <Modal
    :title="title"
    :width="760"
    @close="close"
  >
    <template #lead>
      <EngineTile :engine="form.engine" />
    </template>
    <template #badges>
      <EnvBadge :environment="form.environment" />
    </template>
    <template #tabs>
      <div
        class="tabs"
        role="tablist"
        aria-label="Connection settings"
        @keydown="onTabKey"
      >
        <button
          v-for="tab in TABS"
          :id="id(`tab-${tab.name}`)"
          :key="tab.name"
          type="button"
          role="tab"
          class="tab"
          :aria-selected="activeTab === tab.name"
          :aria-controls="id(`panel-${tab.name}`)"
          :tabindex="activeTab === tab.name ? 0 : -1"
          @click="activeTab = tab.name"
        >
          {{ tab.label }}
          <span
            v-if="tabErrors.has(tab.name)"
            class="tab-dot error"
            role="img"
            aria-label="Has errors"
          />
          <span
            v-else-if="tab.name === 'ssh' && form.ssh.enabled"
            class="tab-dot on"
            role="img"
            aria-label="Enabled"
          />
        </button>
      </div>
    </template>

    <form
      class="form"
      novalidate
      @submit.prevent="submit"
    >
      <!-- General -->
      <div
        v-show="activeTab === 'general'"
        :id="id('panel-general')"
        role="tabpanel"
        :aria-labelledby="id('tab-general')"
        class="panel"
      >
        <div class="grid name-row">
          <FormField
            label="Name"
            :input-id="id('name')"
            :required="req('name')"
            :error="err('name')"
          >
            <input
              :id="id('name')"
              v-model="name"
              v-bind="nameAttrs"
              class="input"
              :aria-invalid="!!err('name')"
              :aria-describedby="describedBy('name', 'name')"
              data-autofocus
            >
          </FormField>
          <FormField
            label="Folder"
            :input-id="id('folder')"
          >
            <select
              :id="id('folder')"
              v-model="folderModel"
              v-bind="folderAttrs"
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
          <div class="field-block">
            <span
              :id="id('color-label')"
              class="block-label"
            >Color</span>
            <div
              class="colors"
              role="radiogroup"
              :aria-labelledby="id('color-label')"
            >
              <button
                v-for="c in COLORS"
                :key="c || 'none'"
                type="button"
                role="radio"
                class="swatch"
                :class="{ none: !c }"
                :aria-checked="form.color === c"
                :aria-label="c ? c : 'No color'"
                :tabindex="form.color === c ? 0 : -1"
                :style="c ? { background: colorVar(c) ?? undefined } : undefined"
                @click="setFieldValue('color', c)"
                @keydown="onRadioKey($event, COLORS, form.color as (typeof COLORS)[number], (v) => setFieldValue('color', v))"
              />
            </div>
          </div>
        </div>

        <div class="field-block">
          <span
            :id="id('engine-label')"
            class="block-label"
          >Engine</span>
          <div
            class="engines"
            role="radiogroup"
            :aria-labelledby="id('engine-label')"
          >
            <button
              v-for="e in ENGINES"
              :key="e"
              type="button"
              role="radio"
              class="engine"
              :aria-checked="form.engine === e"
              :tabindex="form.engine === e ? 0 : -1"
              @click="chooseEngine(e)"
              @keydown="onRadioKey($event, ENGINES, form.engine, chooseEngine)"
            >
              <EngineTile :engine="e" />
              <span class="engine-text">
                <span>{{ ENGINE_INFO[e].name }}</span>
                <span class="subtle">{{ ENGINE_INFO[e].versions }}</span>
              </span>
            </button>
          </div>
        </div>

        <div class="grid host-row">
          <FormField
            label="Host"
            :input-id="id('host')"
            :required="req('host')"
            :error="err('host')"
            :help="form.ssh.enabled ? `Resolved from the SSH host ${form.ssh.host || '(not set)'}` : ''"
          >
            <input
              :id="id('host')"
              v-model="host"
              v-bind="hostAttrs"
              class="input mono"
              spellcheck="false"
              autocomplete="off"
              :aria-invalid="!!err('host')"
              :aria-describedby="describedBy('host', 'host')"
            >
          </FormField>
          <FormField
            label="Port"
            :input-id="id('port')"
            :required="req('port')"
            :error="err('port')"
          >
            <input
              :id="id('port')"
              v-model="port"
              v-bind="portAttrs"
              class="input mono"
              inputmode="numeric"
              :placeholder="String(defaultPort(form.engine))"
              :aria-invalid="!!err('port')"
              :aria-describedby="describedBy('port', 'port')"
            >
          </FormField>
          <FormField
            v-if="!isRedis"
            :label="form.engine === 'postgres' ? 'Database' : 'Default schema'"
            :input-id="id('database')"
            :required="req('database')"
            :error="err('database')"
          >
            <input
              :id="id('database')"
              v-model="database"
              v-bind="databaseAttrs"
              class="input mono"
              spellcheck="false"
              autocomplete="off"
              :placeholder="form.engine === 'postgres' ? 'Same as the user name' : 'None'"
              :aria-invalid="!!err('database')"
            >
          </FormField>
          <FormField
            v-else
            label="Database number"
            :input-id="id('redis-db')"
            :required="req('options.redisDb')"
            :error="err('options.redisDb')"
          >
            <input
              :id="id('redis-db')"
              v-model="redisDb"
              v-bind="redisDbAttrs"
              class="input mono"
              inputmode="numeric"
              :aria-invalid="!!err('options.redisDb')"
              :aria-describedby="describedBy('options.redisDb', 'redis-db')"
            >
          </FormField>
        </div>

        <div class="grid user-row">
          <FormField
            :label="isRedis ? 'ACL user' : 'Username'"
            :input-id="id('user')"
            :required="req('username')"
            :error="err('username')"
          >
            <input
              :id="id('user')"
              v-model="username"
              v-bind="usernameAttrs"
              class="input mono"
              spellcheck="false"
              autocomplete="off"
              :placeholder="isRedis ? 'default' : ''"
            >
          </FormField>
          <FormField
            label="Password"
            :input-id="id('password')"
          >
            <div
              v-if="savedPassword && !ui.changePassword"
              class="saved"
            >
              <span class="pill tone-success"><Icon
                name="key"
                :size="12"
              />Saved in keychain</span>
              <button
                type="button"
                class="btn ghost compact"
                @click="ui.changePassword = true"
              >
                Change
              </button>
              <button
                type="button"
                class="btn ghost compact"
                @click="forget('db_password')"
              >
                Forget
              </button>
            </div>
            <template v-else>
              <input
                :id="id('password')"
                v-model="password"
                class="input"
                type="password"
                autocomplete="new-password"
                :placeholder="savedPassword ? 'New password' : 'Not saved; asked when you connect'"
              >
              <label class="check">
                <input
                  v-model="savePassword"
                  type="checkbox"
                >
                Save in keychain on this device
              </label>
            </template>
          </FormField>
        </div>
      </div>

      <!-- SSH -->
      <div
        v-show="activeTab === 'ssh'"
        :id="id('panel-ssh')"
        role="tabpanel"
        :aria-labelledby="id('tab-ssh')"
        class="panel"
      >
        <div class="switch-row">
          <button
            type="button"
            role="switch"
            class="switch"
            :aria-checked="form.ssh.enabled"
            :aria-labelledby="id('ssh-enabled')"
            @click="setFieldValue('ssh.enabled', !form.ssh.enabled)"
          />
          <div class="switch-text">
            <span :id="id('ssh-enabled')">Connect through an SSH tunnel</span>
            <span class="help">The database host is reached from the SSH server. One tunnel is shared by every tab of this connection.</span>
          </div>
        </div>

        <fieldset
          class="plain"
          :disabled="!form.ssh.enabled"
        >
          <legend class="visually-hidden">
            SSH tunnel
          </legend>
          <div class="grid host-row">
            <FormField
              label="SSH host"
              :input-id="id('ssh-host')"
              :required="req('ssh.host')"
              :error="err('ssh.host')"
            >
              <input
                :id="id('ssh-host')"
                v-model="sshHost"
                v-bind="sshHostAttrs"
                class="input mono"
                spellcheck="false"
                autocomplete="off"
                :aria-invalid="!!err('ssh.host')"
                :aria-describedby="describedBy('ssh.host', 'ssh-host')"
              >
            </FormField>
            <FormField
              label="Port"
              :input-id="id('ssh-port')"
              :required="req('ssh.port')"
              :error="err('ssh.port')"
            >
              <input
                :id="id('ssh-port')"
                v-model="sshPort"
                v-bind="sshPortAttrs"
                class="input mono"
                inputmode="numeric"
                :aria-invalid="!!err('ssh.port')"
                :aria-describedby="describedBy('ssh.port', 'ssh-port')"
              >
            </FormField>
            <FormField
              label="SSH user"
              :input-id="id('ssh-user')"
              :required="req('ssh.username')"
              :error="err('ssh.username')"
            >
              <input
                :id="id('ssh-user')"
                v-model="sshUser"
                v-bind="sshUserAttrs"
                class="input mono"
                spellcheck="false"
                autocomplete="off"
                :aria-invalid="!!err('ssh.username')"
                :aria-describedby="describedBy('ssh.username', 'ssh-user')"
              >
            </FormField>
          </div>

          <div class="field-block">
            <span
              :id="id('ssh-auth')"
              class="block-label"
            >Authentication</span>
            <div
              class="segmented"
              role="radiogroup"
              :aria-labelledby="id('ssh-auth')"
            >
              <button
                v-for="a in SSH_AUTHS"
                :key="a.value"
                type="button"
                role="radio"
                :aria-checked="form.ssh.auth === a.value"
                :tabindex="form.ssh.auth === a.value ? 0 : -1"
                @click="setFieldValue('ssh.auth', a.value)"
                @keydown="onRadioKey($event, SSH_AUTHS.map((x) => x.value), form.ssh.auth, (v) => setFieldValue('ssh.auth', v))"
              >
                {{ a.label }}
              </button>
            </div>
          </div>

          <FormField
            v-if="form.ssh.auth === 'key_file'"
            label="Private key file"
            :input-id="id('ssh-key')"
            :required="req('ssh.keyPath')"
            :error="err('ssh.keyPath')"
            help="The key stays on disk; only its path is stored, on this device."
          >
            <div class="file-row">
              <input
                :id="id('ssh-key')"
                v-model="sshKeyPath"
                v-bind="sshKeyPathAttrs"
                class="input mono"
                spellcheck="false"
                placeholder="~/.ssh/id_ed25519"
                :aria-invalid="!!err('ssh.keyPath')"
                :aria-describedby="describedBy('ssh.keyPath', 'ssh-key')"
              >
              <button
                type="button"
                class="btn"
                @click="browse('ssh.keyPath', 'Choose the SSH private key')"
              >
                Browse
              </button>
            </div>
          </FormField>

          <FormField
            v-if="sshKind"
            :label="sshKind === 'ssh_password' ? 'SSH password' : 'Key passphrase'"
            :input-id="id('ssh-secret')"
          >
            <div
              v-if="savedSshSecret && !ui.changeSshSecret"
              class="saved"
            >
              <span class="pill tone-success"><Icon
                name="key"
                :size="12"
              />Saved in keychain</span>
              <button
                type="button"
                class="btn ghost compact"
                @click="ui.changeSshSecret = true"
              >
                Change
              </button>
              <button
                type="button"
                class="btn ghost compact"
                @click="forget(sshKind)"
              >
                Forget
              </button>
            </div>
            <template v-else>
              <input
                :id="id('ssh-secret')"
                v-model="sshSecret"
                class="input"
                type="password"
                autocomplete="new-password"
                :placeholder="sshKind === 'ssh_password' ? 'Not saved; asked when you connect' : 'Leave empty for a key without passphrase'"
              >
              <label class="check">
                <input
                  v-model="saveSshSecret"
                  type="checkbox"
                >
                Save in keychain on this device
              </label>
            </template>
          </FormField>
          <p
            v-else
            class="notice tone-neutral"
          >
            The SSH agent of your system provides the key. Nothing is stored in Varve.
          </p>
        </fieldset>

        <p class="notice tone-info">
          <Icon name="info" />
          <span>Key files never leave this device: only their path is stored. Passphrases live in your system keychain. The
            first time you connect, Varve asks you to trust the server's host key.</span>
        </p>
      </div>

      <!-- TLS -->
      <div
        v-show="activeTab === 'tls'"
        :id="id('panel-tls')"
        role="tabpanel"
        :aria-labelledby="id('tab-tls')"
        class="panel"
      >
        <div class="field-block">
          <span
            :id="id('tls-mode')"
            class="block-label"
          >TLS mode</span>
          <div
            class="segmented"
            role="radiogroup"
            :aria-labelledby="id('tls-mode')"
            :aria-describedby="id('tls-help')"
          >
            <button
              v-for="m in TLS_MODES"
              :key="m"
              type="button"
              role="radio"
              :aria-checked="form.tls.mode === m"
              :tabindex="form.tls.mode === m ? 0 : -1"
              @click="setFieldValue('tls.mode', m)"
              @keydown="onRadioKey($event, TLS_MODES, form.tls.mode, (v) => setFieldValue('tls.mode', v))"
            >
              {{ TLS_MODE_INFO[m].label }}
            </button>
          </div>
          <span
            :id="id('tls-help')"
            class="help"
          >{{ TLS_MODE_INFO[form.tls.mode].help }}</span>
          <span
            v-if="err('tls.mode')"
            class="error"
            role="alert"
          >{{ err('tls.mode') }}</span>
        </div>

        <FormField
          label="CA certificate"
          :input-id="id('tls-ca')"
          :required="req('tls.caPath')"
          :error="err('tls.caPath')"
          help="Used by verify-ca and verify-full. Leave empty to use the system's trusted certificates."
        >
          <div class="file-row">
            <input
              :id="id('tls-ca')"
              v-model="caPath"
              v-bind="caPathAttrs"
              class="input mono"
              spellcheck="false"
            >
            <button
              type="button"
              class="btn"
              @click="browse('tls.caPath', 'Choose the CA certificate')"
            >
              Browse
            </button>
          </div>
        </FormField>

        <div class="grid two">
          <FormField
            label="Client certificate"
            :input-id="id('tls-cert')"
            :required="req('tls.clientCertPath')"
            :error="err('tls.clientCertPath')"
          >
            <div class="file-row">
              <input
                :id="id('tls-cert')"
                v-model="certPath"
                v-bind="certPathAttrs"
                class="input mono"
                spellcheck="false"
                :aria-invalid="!!err('tls.clientCertPath')"
                :aria-describedby="describedBy('tls.clientCertPath', 'tls-cert')"
              >
              <button
                type="button"
                class="btn"
                @click="browse('tls.clientCertPath', 'Choose the client certificate')"
              >
                Browse
              </button>
            </div>
          </FormField>
          <FormField
            label="Client key"
            :input-id="id('tls-key')"
            :required="req('tls.clientKeyPath')"
            :error="err('tls.clientKeyPath')"
          >
            <div class="file-row">
              <input
                :id="id('tls-key')"
                v-model="keyPath"
                v-bind="keyPathAttrs"
                class="input mono"
                spellcheck="false"
                :aria-invalid="!!err('tls.clientKeyPath')"
                :aria-describedby="describedBy('tls.clientKeyPath', 'tls-key')"
              >
              <button
                type="button"
                class="btn"
                @click="browse('tls.clientKeyPath', 'Choose the client key')"
              >
                Browse
              </button>
            </div>
          </FormField>
        </div>
        <FormField
          v-if="usesClientKey"
          label="Key passphrase"
          :input-id="id('tls-passphrase')"
        >
          <div
            v-if="savedTlsPassphrase && !ui.changeTlsPassphrase"
            class="saved"
          >
            <span class="pill tone-success"><Icon
              name="key"
              :size="12"
            />Saved in keychain</span>
            <button
              type="button"
              class="btn ghost compact"
              @click="ui.changeTlsPassphrase = true"
            >
              Change
            </button>
            <button
              type="button"
              class="btn ghost compact"
              @click="forget('tls_key_passphrase')"
            >
              Forget
            </button>
          </div>
          <template v-else>
            <input
              :id="id('tls-passphrase')"
              v-model="tlsPassphrase"
              class="input"
              type="password"
              autocomplete="new-password"
              placeholder="Leave empty for an unencrypted key; asked when you connect"
            >
            <label class="check">
              <input
                v-model="saveTlsPassphrase"
                type="checkbox"
              >
              Save in keychain on this device
            </label>
          </template>
        </FormField>
        <p class="help">
          Certificate and key paths are stored for this device only. Encrypted keys (PKCS#8, or the older OpenSSL
          format) are unlocked with their passphrase, which lives in your system keychain or is asked for when you
          connect.
        </p>
      </div>

      <!-- Advanced -->
      <div
        v-show="activeTab === 'advanced'"
        :id="id('panel-advanced')"
        role="tabpanel"
        :aria-labelledby="id('tab-advanced')"
        class="panel"
      >
        <h3 class="label">
          Safety
        </h3>
        <div class="field-block">
          <span
            :id="id('env')"
            class="block-label"
          >Environment</span>
          <div
            class="segmented"
            role="radiogroup"
            :aria-labelledby="id('env')"
            :aria-describedby="id('env-help')"
          >
            <button
              v-for="e in ENVIRONMENTS"
              :key="e"
              type="button"
              role="radio"
              class="env-option"
              :aria-checked="form.environment === e"
              :tabindex="form.environment === e ? 0 : -1"
              @click="setFieldValue('environment', e)"
              @keydown="onRadioKey($event, ENVIRONMENTS, form.environment, (v) => setFieldValue('environment', v))"
            >
              <span
                class="env-dot"
                :style="{ background: ENVIRONMENT_INFO[e].color }"
              />{{ ENVIRONMENT_INFO[e].label }}
            </button>
          </div>
          <span
            :id="id('env-help')"
            class="help"
          >{{ ENVIRONMENT_HELP[form.environment] }}</span>
        </div>

        <div class="switch-row">
          <button
            type="button"
            role="switch"
            class="switch"
            :aria-checked="form.options.readOnly"
            :aria-labelledby="id('ro')"
            @click="setFieldValue('options.readOnly', !form.options.readOnly)"
          />
          <div class="switch-text">
            <span :id="id('ro')">Read-only connection</span>
            <span class="help">{{
              isRedis
                ? 'Blocks write commands in the console, using the flags from COMMAND INFO.'
                : 'Every session starts read-only, so statements cannot change data.'
            }}</span>
          </div>
        </div>

        <div class="switch-row">
          <button
            type="button"
            role="switch"
            class="switch"
            :aria-checked="form.environment === 'production' || form.options.confirmDestructive"
            :aria-disabled="form.environment === 'production'"
            :aria-labelledby="id('cd')"
            :aria-describedby="id('cd-help')"
            @click="form.environment !== 'production' && setFieldValue('options.confirmDestructive', !form.options.confirmDestructive)"
          />
          <div class="switch-text">
            <span :id="id('cd')">Confirm destructive statements
              <span
                v-if="form.environment === 'production'"
                class="pill tone-neutral"
              ><Icon
                name="lock"
                :size="11"
              />Always on for production</span>
            </span>
            <span
              :id="id('cd-help')"
              class="help"
            >{{
              isRedis
                ? 'Asks before commands such as FLUSHALL, FLUSHDB and DEL.'
                : 'Asks before DROP, TRUNCATE, and DELETE or UPDATE without WHERE.'
            }}</span>
          </div>
        </div>

        <div
          v-if="!isRedis"
          class="switch-row"
        >
          <button
            type="button"
            role="switch"
            class="switch"
            :aria-checked="form.options.showSystemObjects"
            :aria-labelledby="id('sys')"
            @click="setFieldValue('options.showSystemObjects', !form.options.showSystemObjects)"
          />
          <div class="switch-text">
            <span :id="id('sys')">Show system objects</span>
            <span class="help">{{
              form.engine === 'postgres'
                ? 'Lists pg_catalog, information_schema and pg_toast in the tree.'
                : 'Lists mysql, sys, information_schema and performance_schema in the tree.'
            }}</span>
          </div>
        </div>

        <h3 class="label">
          Session
        </h3>
        <div class="grid two">
          <FormField
            label="Statement timeout (ms)"
            :input-id="id('timeout')"
            :error="err('options.statementTimeoutMs')"
            help="Empty means no timeout."
          >
            <input
              :id="id('timeout')"
              v-model="timeout"
              v-bind="timeoutAttrs"
              class="input mono"
              inputmode="numeric"
              :aria-invalid="!!err('options.statementTimeoutMs')"
              :aria-describedby="describedBy('options.statementTimeoutMs', 'timeout')"
            >
          </FormField>
          <FormField
            label="Keepalive interval (s)"
            :input-id="id('keepalive')"
            :error="err('options.keepaliveS')"
            help="Also keeps the SSH tunnel open. Empty uses 15 s."
          >
            <input
              :id="id('keepalive')"
              v-model="keepalive"
              v-bind="keepaliveAttrs"
              class="input mono"
              inputmode="numeric"
              :aria-invalid="!!err('options.keepaliveS')"
              :aria-describedby="describedBy('options.keepaliveS', 'keepalive')"
            >
          </FormField>
        </div>
        <FormField
          :label="isRedis ? 'Init commands (run on every new session)' : 'Init SQL (runs on every new session)'"
          :input-id="id('init')"
          :error="err('options.initSql')"
        >
          <textarea
            :id="id('init')"
            v-model="initSql"
            v-bind="initSqlAttrs"
            class="textarea"
            rows="4"
            spellcheck="false"
            :placeholder="isRedis ? 'CLIENT NO-EVICT on' : 'SET search_path TO public;'"
          />
        </FormField>
      </div>

      <p
        v-if="ui.formError"
        class="notice tone-danger"
        role="alert"
      >
        <Icon name="warning" />
        <span>{{ ui.formError }}</span>
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
      <div class="test">
        <button
          type="button"
          class="btn"
          :disabled="ui.test.state === 'running'"
          @click="test"
        >
          <Icon name="activity" />{{ ui.test.state === 'running' ? 'Testing…' : 'Test connection' }}
        </button>
        <div
          class="test-result"
          role="status"
          aria-live="polite"
        >
          <template v-if="ui.test.state === 'ok'">
            <span class="pill tone-success"><span class="dot" />Connected</span>
            <span class="mono">{{ ENGINE_INFO[ui.test.server.engine].name }} {{ ui.test.server.version }}</span>
            <span class="subtle">Latency {{ formatDuration(ui.test.latencyMs) }}</span>
          </template>
          <template v-else-if="ui.test.state === 'error'">
            <span class="pill tone-danger"><span class="dot" />{{ ui.test.title }}</span>
            <span
              class="test-error"
              :title="ui.test.message"
            >{{ ui.test.message }}</span>
          </template>
        </div>
      </div>
      <button
        type="button"
        class="btn"
        @click="close"
      >
        Cancel
      </button>
      <button
        type="button"
        class="btn primary"
        :disabled="ui.saving"
        @click="submit"
      >
        {{ ui.saving ? 'Saving…' : 'Save' }}
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.tabs {
  display: flex;
  flex-shrink: 0;
  gap: 4px;
  padding: 0 20px;
  border-bottom: 1px solid var(--border-inner);
}

.tab {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 40px;
  padding: 0 12px;
  border: 0;
  background: transparent;
  color: var(--text-muted);
  font-size: 13px;
  cursor: pointer;
}

.tab[aria-selected='true'] {
  color: var(--text);
  font-weight: 500;
  box-shadow: inset 0 -2px 0 var(--accent);
}

.tab-dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
}

.tab-dot.on {
  background: var(--success);
}

.tab-dot.error {
  background: var(--danger);
}

.form {
  display: flex;
  flex-direction: column;
  gap: 16px;
  min-height: 460px;
}

.panel {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.grid {
  display: grid;
  gap: 12px;
  align-items: start;
}

.name-row {
  grid-template-columns: 1fr 1fr auto;
}

.host-row {
  grid-template-columns: 2fr 90px 1fr;
}

.user-row,
.two {
  grid-template-columns: 1fr 1fr;
}

.field-block {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.block-label {
  font-size: 12px;
  font-weight: 500;
  color: var(--text-muted);
}

.colors {
  display: flex;
  gap: 6px;
  align-items: center;
  height: var(--control);
}

.swatch {
  width: 20px;
  height: 20px;
  padding: 0;
  border: 2px solid transparent;
  border-radius: 50%;
  cursor: pointer;
}

.swatch.none {
  border-color: var(--border);
  background: var(--surface);
}

.swatch[aria-checked='true'] {
  box-shadow: 0 0 0 2px var(--surface), 0 0 0 4px var(--accent);
}

.engines {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 8px;
}

.engine {
  display: flex;
  align-items: center;
  gap: 10px;
  height: 52px;
  padding: 0 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
  text-align: left;
  cursor: pointer;
}

.engine[aria-checked='true'] {
  border-color: var(--accent);
  background: var(--accent-tint);
}

.engine-text {
  display: flex;
  flex-direction: column;
  font-size: 13px;
}

.engine-text .subtle {
  font-size: 11px;
}

.saved {
  display: flex;
  align-items: center;
  gap: 4px;
  height: var(--control);
}

.check {
  display: flex;
  gap: 8px;
  align-items: center;
  font-size: 12px;
  color: var(--text-muted);
  cursor: pointer;
}

.check input {
  accent-color: var(--accent);
}

.switch-row {
  display: flex;
  gap: 12px;
  align-items: flex-start;
}

.switch-text {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.switch-text > span:first-child {
  display: flex;
  align-items: center;
  gap: 8px;
  font-weight: 500;
}

.help {
  margin: 0;
  font-size: 12px;
  color: var(--text-subtle);
}

.error {
  font-size: 12px;
  color: var(--danger-text);
}

.plain {
  display: flex;
  flex-direction: column;
  gap: 16px;
  margin: 0;
  padding: 0;
  border: 0;
}

.plain:disabled {
  opacity: 0.6;
}

.file-row {
  display: flex;
  gap: 8px;
}

.notice {
  margin: 0;
  align-items: flex-start;
}

h3.label {
  margin: 4px 0 0;
}

.env-option {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.env-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
}

.test {
  display: flex;
  flex: 1;
  align-items: center;
  gap: 12px;
  min-width: 0;
}

.test-result {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
  font-size: 12px;
}

.test-error {
  overflow: hidden;
  color: var(--danger-text);
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
