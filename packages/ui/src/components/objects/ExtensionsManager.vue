<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { fetchActivity } from '../../api/activity';
import type { ActivityEvent } from '../../api/activity';
import { catalogList, catalogSchemas } from '../../api/catalog';
import type { ObjectSummary } from '../../api/catalog';
import { catalogAvailableExtensions, catalogDescribe } from '../../api/ddl';
import type { PgAvailableExtension, PgExtension } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import { useDialogs } from '../../composables/useDialogs';
import { formatDateTime } from '../../lib/display';
import {
  buildExtensionDrop,
  buildExtensionInstall,
  buildExtensionSchema,
  buildExtensionUpdate,
  errorsOf,
  extensionInstallSchema,
  objectRef,
} from '../../lib/ddlPlan';
import type { ExtensionInstallForm } from '../../lib/ddlPlan';
import EnvBadge from '../common/EnvBadge.vue';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import CommentEditor from './CommentEditor.vue';
import PlanPreview from './PlanPreview.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';

/**
 * PostgreSQL extensions of one database (design canvas ExtensionsManager):
 * installed extensions with version, schema, update and comment; install
 * from a picker filled from `pg_available_extensions` (name, description,
 * default and available versions, installed state) with a version, a
 * schema and CASCADE (suggested when a required extension is missing), or
 * by typed name when the list cannot be read; update, move and drop, and
 * recent extension changes from the activity log. Every action shows the
 * SQL before it runs; changing any input or the selection discards a
 * preview, so Apply always runs what is shown for what is selected. The
 * picker's hints (requires, rights, fixed schema) follow the chosen
 * version. The selected extension's comment reports unsaved text to the
 * tab, and switching to another extension asks before discarding it. Props and events: `ObjectEditorProps` / `ObjectEditorEmits`;
 * opens for an `extension` node (that extension selected) or with
 * `object: null`, `createKind: 'extension'` and the database as container.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const installed = ref<ObjectSummary[]>([]);
const loading = ref(false);
const loadError = ref<string | null>(null);
const selected = ref<PgExtension | null>(null);
const selectedName = ref<string | null>(props.object?.name ?? null);
const changes = ref<ActivityEvent[]>([]);

const plan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => emit('changed'),
  reportError: (error) => props.reportError?.(error),
});
const action = ref<string | null>(null);

// The install picker: what the server can install, and the database's schemas.
const available = ref<PgAvailableExtension[] | null>(null);
const availableError = ref<string | null>(null);
const schemas = ref<string[]>([]);
const filter = ref('');
const showInstalled = ref(false);

async function loadAvailable(): Promise<void> {
  try {
    available.value = await catalogAvailableExtensions(props.session.id);
    availableError.value = null;
  } catch (error: unknown) {
    // The picker falls back to typing the name; the tab still learns about a lost session.
    props.reportError?.(error);
    available.value = null;
    availableError.value = presentError(toCmdError(error)).message;
  }
  try {
    const db = database.value;
    schemas.value = db ? (await catalogSchemas(props.session.id, db)).filter((s) => !s.system).map((s) => s.object.name) : [];
  } catch (error: unknown) {
    props.reportError?.(error);
    schemas.value = [];
  }
}

async function load(): Promise<void> {
  loading.value = true;
  loadError.value = null;
  try {
    installed.value = await catalogList(props.session.id, { database: database.value, schema: null, parent: null }, 'extension', true);
    if (selectedName.value) await select(selectedName.value);
  } catch (error: unknown) {
    props.reportError?.(error);
    loadError.value = presentError(toCmdError(error)).message;
  } finally {
    loading.value = false;
  }
  await loadAvailable();
  try {
    const page = await fetchActivity({ actions: ['schema.changed', 'object.dropped'], connectionId: props.session.connectionId, text: 'EXTENSION', limit: 8 });
    changes.value = page.events;
  } catch (error: unknown) {
    props.reportError?.(error);
  }
}

async function select(name: string): Promise<void> {
  // Re-reading the same extension (after an update or move) keeps its
  // comment editor mounted, so typed text survives.
  if (selected.value?.name !== name) selected.value = null;
  selectedName.value = name;
  try {
    const d = await catalogDescribe(props.session.id, objectRef('extension', database.value, null, name));
    if (d.engine === 'postgres' && d.object.kind === 'extension') {
      selected.value = d.object;
      updateVersion.value = d.object.availableUpdates[d.object.availableUpdates.length - 1] ?? '';
      moveSchema.value = d.object.schema;
    }
  } catch (error: unknown) {
    props.reportError?.(error);
    loadError.value = presentError(toCmdError(error)).message;
  }
}

// Install.
const install = ref<ExtensionInstallForm>({ name: '', schema: '', version: '', cascade: false });
const installErrors = computed(() => errorsOf(extensionInstallSchema, install.value));
const ids = { name: useId(), schema: useId(), version: useId(), update: useId(), move: useId(), filter: useId(), schemas: useId() };
/** True while the picker has the list; false when the name is typed (the list could not be read). */
const picking = computed(() => available.value !== null);
const installedNames = computed(() => new Set((available.value ?? []).filter((e) => e.installedVersion !== null).map((e) => e.name)));
const candidates = computed(() => {
  const words = filter.value.trim().toLocaleLowerCase();
  return (available.value ?? []).filter(
    (e) =>
      (showInstalled.value || e.installedVersion === null) &&
      (words === '' || e.name.toLocaleLowerCase().includes(words) || (e.comment ?? '').toLocaleLowerCase().includes(words)),
  );
});
const notInstalledCount = computed(() => (available.value ?? []).filter((e) => e.installedVersion === null).length);
const picked = computed(() => (available.value ?? []).find((e) => e.name === install.value.name) ?? null);
/** Required extensions not installed in this database: CREATE fails without CASCADE. */
/** The control file data of the chosen version (the default when none is chosen). */
const pickedVersion = computed(() => {
  const ext = picked.value;
  if (!ext) return null;
  const version = install.value.version || ext.defaultVersion;
  return (
    ext.versionDetails.find((v) => v.version === version) ?? {
      version: version ?? '',
      superuser: ext.superuser,
      trusted: ext.trusted,
      relocatable: ext.relocatable,
      fixedSchema: ext.fixedSchema,
      requires: ext.requires,
    }
  );
});
const missingRequires = computed(() => (pickedVersion.value ? pickedVersion.value.requires.filter((r) => !installedNames.value.has(r)) : []));
/** Versions newest first, for the select. */
const versionsNewestFirst = computed(() => [...(picked.value?.versions ?? [])].reverse());
const canInstall = computed(() => {
  if (props.session.readOnly || install.value.name.trim() === '') return false;
  if (!picking.value) return true;
  return picked.value !== null && picked.value.installedVersion === null;
});

function pick(ext: PgAvailableExtension): void {
  install.value = { name: ext.name, schema: '', version: '', cascade: ext.requires.some((r) => !installedNames.value.has(r)) };
}
// Another version may need other extensions: suggest CASCADE for it again,
// and drop a schema its control file does not allow.
watch(
  () => install.value.version,
  () => {
    if (!picking.value || !pickedVersion.value) return;
    install.value.cascade = missingRequires.value.length > 0;
    if (pickedVersion.value.fixedSchema !== null) install.value.schema = '';
  },
);
const updateVersion = ref('');
const moveSchema = ref('');
const cascade = ref(false);

// A preview belongs to the inputs and the selection it was planned from:
// any change discards it, so Install, Update, Move or Drop never runs a
// statement for other values or another extension.
watch([install, updateVersion, moveSchema, cascade, selectedName], () => {
  if (action.value === null || plan.state.applying) return;
  plan.clear();
  action.value = null;
}, { deep: true });

// The selected extension's comment: unsaved text marks the tab and is not
// thrown away without asking.
const commentDirty = ref(false);
function commentDirtyChanged(value: boolean): void {
  commentDirty.value = value;
  props.reportDirty?.(value);
}

async function choose(name: string): Promise<void> {
  if (name !== selectedName.value && commentDirty.value) {
    const discard = await useDialogs().askConfirm({
      title: 'Discard the typed comment?',
      message: `The comment typed for ${selectedName.value ?? 'this extension'} has not been applied. Selecting ${name} discards it.`,
      confirmLabel: 'Discard and switch',
      danger: false,
    });
    if (!discard) return;
  }
  await select(name);
}

async function planAction(kind: 'install' | 'update' | 'schema' | 'drop'): Promise<void> {
  action.value = kind;
  const ext = selected.value;
  switch (kind) {
    case 'install':
      if (Object.keys(installErrors.value).length > 0 || !canInstall.value) return;
      await plan.plan([{ kind: 'edit', change: buildExtensionInstall(install.value) }]);
      return;
    case 'update':
      if (ext) await plan.plan([{ kind: 'edit', change: buildExtensionUpdate(ext.name, updateVersion.value || null) }]);
      return;
    case 'schema':
      if (ext && moveSchema.value.trim() !== '') await plan.plan([{ kind: 'edit', change: buildExtensionSchema(ext.name, moveSchema.value) }]);
      return;
    case 'drop':
      if (ext) await plan.plan([{ kind: 'edit', change: buildExtensionDrop(ext.name, cascade.value) }]);
  }
}

async function done(): Promise<void> {
  const kind = action.value;
  const name = kind === 'install' ? install.value.name.trim() : selectedName.value;
  plan.clear();
  action.value = null;
  if (kind === 'drop' && selected.value) {
    emit('dropped', objectRef('extension', database.value, null, selected.value.name));
    selectedName.value = null;
    selected.value = null;
  } else if (name && !(kind === 'install' && commentDirty.value && selectedName.value !== null)) {
    // After an install the new extension is selected, unless that would
    // discard a comment typed for the selected one: then it stays selected.
    selectedName.value = name;
  }
  if (kind === 'install') install.value = { name: '', schema: '', version: '', cascade: false };
  await load();
}

const applyLabel = computed(() => {
  switch (action.value) {
    case 'install':
      return 'Install';
    case 'update':
      return 'Update';
    case 'schema':
      return 'Move';
    case 'drop':
      return 'Drop';
    default:
      return 'Apply';
  }
});
const updates = computed(() => installed.value.filter((e) => e.flags.includes('update available')).length);
// Keyed on the name only, so re-reading the same extension keeps the comment editor's object (and its typed text).
const selectedExtension = computed(() => selected.value?.name ?? null);
const selectedRef = computed(() => (selectedExtension.value ? objectRef('extension', database.value, null, selectedExtension.value) : null));

onMounted(load);
</script>

<template>
  <div class="manager">
    <header class="header">
      <h1>Extensions</h1>
      <span class="subtle">database <span class="mono">{{ database }}</span></span>
      <EnvBadge :environment="session.environment" />
      <span
        v-if="session.readOnly"
        class="pill tone-neutral"
      ><Icon name="lock" />Read only</span>
      <span class="grow" />
      <button
        type="button"
        class="icon-btn"
        aria-label="Reload the extension list"
        :disabled="loading"
        @click="load"
      >
        <Icon name="refresh" />
      </button>
    </header>
    <p
      v-if="loadError"
      class="notice tone-danger state"
      role="alert"
    >
      <Icon name="warning" />{{ loadError }}
    </p>
    <div class="layout">
      <div class="column">
        <section
          class="section"
          aria-label="Installed extensions"
        >
          <div class="section-head">
            <h2>Installed</h2>
            <span class="hint">{{ installed.length }}</span>
            <span
              v-if="updates > 0"
              class="pill tone-info"
            >{{ updates }} update{{ updates === 1 ? '' : 's' }} available</span>
          </div>
          <ul class="list">
            <li
              v-for="e in installed"
              :key="e.object.name"
            >
              <button
                type="button"
                class="list-item"
                :aria-current="e.object.name === selectedName"
                @click="choose(e.object.name)"
              >
                <span class="mono">{{ e.object.name }}</span>
                <span class="hint">{{ e.detail ?? '' }}</span>
                <span class="grow" />
                <span
                  v-for="f in e.flags"
                  :key="f"
                  class="pill tone-info"
                >{{ f }}</span>
                <span
                  v-if="e.system"
                  class="pill tone-neutral"
                >built in</span>
              </button>
            </li>
          </ul>
        </section>
        <section
          v-if="selected"
          class="section"
          :aria-label="`Extension ${selected.name}`"
        >
          <div class="section-head">
            <h2 class="mono">
              {{ selected.name }} {{ selected.version }}
            </h2>
            <span class="hint">schema {{ selected.schema }}</span>
          </div>
          <p
            v-if="selected.description"
            class="hint"
          >
            {{ selected.description }}
          </p>
          <p
            v-if="selected.requires.length > 0"
            class="hint"
          >
            Requires {{ selected.requires.join(', ') }}.
          </p>
          <div class="grid">
            <FormField
              label="Update to"
              :input-id="ids.update"
              :help="selected.availableUpdates.length === 0 ? 'No update path from this version.' : ''"
            >
              <div class="pair">
                <select
                  :id="ids.update"
                  v-model="updateVersion"
                  class="select"
                  :disabled="selected.availableUpdates.length === 0"
                >
                  <option
                    v-for="v in selected.availableUpdates"
                    :key="v"
                    :value="v"
                  >
                    {{ v }}{{ v === selected.defaultVersion ? ' (default)' : '' }}
                  </option>
                </select>
                <button
                  type="button"
                  class="btn compact"
                  :disabled="session.readOnly || selected.availableUpdates.length === 0"
                  @click="planAction('update')"
                >
                  Update
                </button>
              </div>
            </FormField>
            <FormField
              label="Move to schema"
              :input-id="ids.move"
              :help="selected.relocatable ? '' : 'This extension is not relocatable.'"
            >
              <div class="pair">
                <input
                  :id="ids.move"
                  v-model="moveSchema"
                  class="input mono"
                  :disabled="!selected.relocatable"
                >
                <button
                  type="button"
                  class="btn compact"
                  :disabled="session.readOnly || !selected.relocatable || moveSchema.trim() === '' || moveSchema.trim() === selected.schema"
                  @click="planAction('schema')"
                >
                  Move
                </button>
              </div>
            </FormField>
          </div>
          <div class="drop-row">
            <label class="check"><input
              v-model="cascade"
              type="checkbox"
            ><span>CASCADE<span class="sub">Also drop objects that use it (for example indexes and constraints using its operators).</span></span></label>
            <button
              type="button"
              class="btn compact"
              :disabled="session.readOnly || selected.meta.system"
              :title="selected.meta.system ? 'Built in; cannot be dropped' : undefined"
              @click="planAction('drop')"
            >
              Drop
            </button>
          </div>
          <CommentEditor
            v-if="selectedRef"
            class="nested"
            :session="session"
            :connection-name="connectionName"
            :object="selectedRef"
            :report-error="reportError"
            :report-dirty="commentDirtyChanged"
            @changed="emit('changed')"
            @reconnect="emit('reconnect')"
          />
          <details v-if="selected.members.length > 0">
            <summary class="hint">
              {{ selected.members.length }} member objects
            </summary>
            <ul class="members mono">
              <li
                v-for="m in selected.members"
                :key="m"
              >
                {{ m }}
              </li>
            </ul>
          </details>
        </section>
      </div>
      <div class="column">
        <section
          class="section"
          aria-label="Install an extension"
        >
          <div class="section-head">
            <h2>Install</h2>
            <span
              v-if="picking"
              class="hint"
            >{{ notInstalledCount }} available</span>
          </div>
          <template v-if="picking">
            <div class="pair">
              <input
                :id="ids.filter"
                v-model="filter"
                class="input"
                type="search"
                placeholder="Filter by name or description"
                aria-label="Filter the available extensions"
                spellcheck="false"
              >
              <label class="check inline"><input
                v-model="showInstalled"
                type="checkbox"
              ><span>Show installed</span></label>
            </div>
            <ul
              class="list picker"
              aria-label="Available extensions"
            >
              <li
                v-for="e in candidates"
                :key="e.name"
              >
                <button
                  type="button"
                  class="list-item available"
                  :aria-pressed="e.name === install.name"
                  @click="pick(e)"
                >
                  <span class="line">
                    <span class="mono">{{ e.name }}</span>
                    <span class="hint">{{ e.defaultVersion ?? '' }}</span>
                    <span class="grow" />
                    <span
                      v-if="e.installedVersion !== null"
                      class="pill tone-neutral"
                    >installed {{ e.installedVersion }} in {{ e.installedSchema }}</span>
                    <span
                      v-else-if="e.superuser && !e.trusted"
                      class="pill tone-warning"
                    >superuser</span>
                  </span>
                  <span
                    v-if="e.comment"
                    class="hint"
                  >{{ e.comment }}</span>
                </button>
              </li>
            </ul>
            <p
              v-if="candidates.length === 0"
              class="hint"
            >
              {{ (available ?? []).length === 0 ? 'The server has no extensions available (no control files installed).' : 'No extension matches.' }}
            </p>
            <template v-if="picked">
              <p class="hint">
                <span class="mono">{{ picked.name }}</span>{{ picked.comment ? `: ${picked.comment}` : '' }}
              </p>
              <p
                v-if="picked.installedVersion !== null"
                class="notice tone-info"
              >
                Already installed in this database ({{ picked.installedVersion }} in {{ picked.installedSchema }}). Use the installed list to update, move or drop it.
              </p>
              <div class="grid">
                <FormField
                  label="Version"
                  :input-id="ids.version"
                >
                  <select
                    :id="ids.version"
                    v-model="install.version"
                    class="select mono"
                  >
                    <option value="">
                      default{{ picked.defaultVersion ? ` (${picked.defaultVersion})` : '' }}
                    </option>
                    <option
                      v-for="v in versionsNewestFirst"
                      :key="v"
                      :value="v"
                    >
                      {{ v }}{{ v === picked.defaultVersion ? ' (default)' : '' }}
                    </option>
                  </select>
                </FormField>
                <FormField
                  label="Schema"
                  :input-id="ids.schema"
                  :help="pickedVersion?.fixedSchema ? `Its control file installs it into ${pickedVersion.fixedSchema}.` : pickedVersion?.relocatable === false ? 'It cannot be moved to another schema later.' : ''"
                >
                  <select
                    :id="ids.schema"
                    v-model="install.schema"
                    class="select mono"
                    :disabled="pickedVersion?.fixedSchema != null"
                  >
                    <option value="">
                      {{ pickedVersion?.fixedSchema ?? 'default (first schema in search_path)' }}
                    </option>
                    <option
                      v-for="name in schemas"
                      :key="name"
                      :value="name"
                    >
                      {{ name }}
                    </option>
                  </select>
                </FormField>
              </div>
              <p
                v-if="pickedVersion && pickedVersion.requires.length > 0"
                class="hint"
              >
                Requires {{ pickedVersion.requires.join(', ') }}{{ missingRequires.length > 0 ? `; not installed here: ${missingRequires.join(', ')}` : '' }}.
              </p>
              <p
                v-if="pickedVersion?.superuser && !pickedVersion.trusted"
                class="hint"
              >
                Only a superuser can install it.
              </p>
            </template>
            <p
              v-else
              class="hint"
            >
              Pick an extension to install.
            </p>
          </template>
          <template v-else>
            <p
              class="notice tone-warning"
              role="status"
            >
              <Icon name="warning" />The list of available extensions could not be read{{ availableError ? ` (${availableError})` : '' }}. Type the name instead; the extension must be available on the server (its files installed).
            </p>
            <div class="grid">
              <FormField
                label="Name"
                :input-id="ids.name"
                required
                :error="install.name !== '' ? (installErrors.name ?? '') : ''"
              >
                <input
                  :id="ids.name"
                  v-model="install.name"
                  class="input mono"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                label="Schema"
                :input-id="ids.schema"
              >
                <input
                  :id="ids.schema"
                  v-model="install.schema"
                  class="input mono"
                  placeholder="default"
                  :list="ids.schemas"
                >
                <datalist :id="ids.schemas">
                  <option
                    v-for="name in schemas"
                    :key="name"
                    :value="name"
                  />
                </datalist>
              </FormField>
              <FormField
                label="Version"
                :input-id="ids.version"
              >
                <input
                  :id="ids.version"
                  v-model="install.version"
                  class="input mono"
                  placeholder="default"
                >
              </FormField>
            </div>
          </template>
          <label class="check"><input
            v-model="install.cascade"
            type="checkbox"
          ><span>CASCADE<span class="sub">Also install the extensions it requires.</span></span></label>
          <div>
            <button
              type="button"
              class="btn compact"
              :disabled="!canInstall"
              @click="planAction('install')"
            >
              Preview install
            </button>
          </div>
        </section>
        <PlanPreview
          v-if="action !== null"
          :plan="plan"
          engine="postgres"
          :read-only="session.readOnly"
          :apply-label="applyLabel"
          @applied="done"
          @reconnect="emit('reconnect')"
          @refresh="load"
        />
        <section
          class="section"
          aria-label="Recent extension changes"
        >
          <h2>Recent changes</h2>
          <ul
            v-if="changes.length > 0"
            class="list"
          >
            <li
              v-for="c in changes"
              :key="c.id"
              class="change"
            >
              <span class="hint">{{ formatDateTime(c.occurredAt) }}</span>
              <span class="mono">{{ c.target ?? c.action }}</span>
            </li>
          </ul>
          <p
            v-else
            class="hint"
          >
            No extension changes logged from this app.
          </p>
        </section>
      </div>
    </div>
  </div>
</template>

<style scoped src="./editor.css"></style>
<style scoped>
.manager {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: auto;
  background: var(--ground);
}

.header {
  display: flex;
  align-items: center;
  gap: 12px;
  min-height: 48px;
  padding: 8px 16px;
  border-bottom: 1px solid var(--border);
  background: var(--surface);
}

h1 {
  margin: 0;
  font-size: 15px;
  font-weight: 500;
}

.state {
  margin: 16px 16px 0;
}

.pair {
  display: flex;
  gap: 8px;
}

.drop-row {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 12px;
}

.members {
  max-height: 200px;
  overflow: auto;
  font-size: 12px;
}

.picker {
  max-height: 280px;
  overflow: auto;
}

.available {
  flex-direction: column;
  align-items: stretch;
  gap: 2px;
  text-align: start;
}

.available .line {
  display: flex;
  align-items: center;
  gap: 8px;
}

.available[aria-pressed='true'] {
  background: var(--accent-tint);
  box-shadow: inset 2px 0 0 var(--accent);
}

.check.inline {
  align-items: center;
  white-space: nowrap;
}

.check.inline input {
  margin: 0;
}

.nested {
  padding: 0;
  border: none;
}

.change {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
</style>
