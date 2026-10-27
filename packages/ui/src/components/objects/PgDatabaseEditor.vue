<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { catalogDatabases } from '../../api/catalog';
import type { DropImpact, PgDatabase } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import { inputFromProfile, updateConnection } from '../../api/records';
import { defaultDatabaseOf } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import { useWorkspace } from '../../composables/useWorkspace';
import { appTabsNote } from '../../lib/dropImpact';
import { buildPgDatabase, buildPgDatabaseDrop, needsDatabaseToItself, pgDatabaseErrors, pgDatabaseFormFrom } from '../../lib/ddlDatabase';
import type { PgDatabaseForm } from '../../lib/ddlDatabase';
import { objectRef } from '../../lib/ddlPlan';
import type { FieldErrors } from '../../lib/ddlPlan';
import { formatBytes } from '../../lib/values';
import FormField from '../common/FormField.vue';
import { useObjectBrowser } from '../tree/objectBrowser';
import { appTabsOf } from './appTabs';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import DropImpactPanel from './DropImpactPanel.vue';
import EditorFrame from './EditorFrame.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/**
 * PostgreSQL database editor (design doc 4.2: owner, encoding, collation,
 * ICU locale, templates). Create takes every `CREATE DATABASE` option the
 * server version has; alter changes owner, connection limit, connections,
 * template flag, session defaults, tablespace, comment and name. Drop shows
 * the sessions connected to the database and what blocks it; terminating
 * them (`WITH (FORCE)`) is a separate choice. PostgreSQL cannot drop or
 * rename the database a session uses, so the tab runs those from another
 * database (`switchDatabase`). Props and events: `ObjectEditorProps` /
 * `ObjectEditorEmits`.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const major = computed(() => props.session.server.major);
const force = ref(false);
const databases = ref<string[]>([]);
const runFrom = ref('');
const impact = ref<DropImpact | null>(null);
/** After a rename of the connection's own database: point the connection at the new name. */
const followRename = ref(false);

const editor = useObjectEditor<PgDatabase, PgDatabaseForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'database' ? d.object : 'This object is not a database.'),
  afterLoad: async () => {
    const list = await catalogDatabases(props.session.id);
    databases.value = list.map((d) => d.object.name);
  },
  formFrom: (db) => pgDatabaseFormFrom(db),
  validate: (form): FieldErrors => pgDatabaseErrors(form, editor.current.value === null, props.session.server),
  blocked: (form, db) => {
    if (db?.system) return `${db.name} is a template database of the server; it is not changed here.`;
    if (db && needsDatabaseToItself(form, db) && props.session.database === db.name) return onTargetText(db.name);
    return null;
  },
  build: (form, db) => buildPgDatabase(form, db).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('database', form.name.trim(), null, form.name.trim()),
  dropChange: (object) => buildPgDatabaseDrop(object.name, force.value),
  afterRun: (): Promise<void> => followRenamedDatabase(),
  confirmWarnings: (): string[] => {
    const out: string[] = [];
    if (editor.mode.value === 'drop') {
      const tabs = appTabsNote('postgres', appTabsOf(impact.value), force.value);
      if (tabs) out.push(tabs);
    }
    if (profileDbNote.value) out.push(profileDbNote.value);
    return out;
  },
  beforeApply: async (): Promise<void> => {
    const db = editor.details.value;
    if (!db) return;
    if (editor.mode.value === 'drop' || needsDatabaseToItself(editor.form as PgDatabaseForm, db)) {
      // The tree's own sessions on this database would make PostgreSQL refuse.
      await useObjectBrowser().releaseDatabase(props.session.connectionId, db.name);
    }
  },
});
const { form, errors, details } = editor;

function onTargetText(name: string): string {
  return `This tab is connected to ${name}. PostgreSQL cannot drop or rename the database a session uses: choose another database to run it from.`;
}

const tab = ref('definition');
const tabs = computed(() => [
  { id: 'definition', label: 'Database' },
  { id: 'settings', label: 'Settings' },
  ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : []),
]);
const creating = computed(() => editor.current.value === null);
const title = computed(() => (editor.current.value ? editor.current.value.name : 'New database'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const onTarget = computed(() => details.value !== null && props.session.database === details.value.name);
const otherDatabases = computed(() => databases.value.filter((d) => d !== details.value?.name));
const dropBlocked = computed(() => {
  if (details.value?.system) return `${details.value.name} is a template database of the server; it is not dropped from here.`;
  return onTarget.value && details.value ? onTargetText(details.value.name) : null;
});
/** The connection's own database (its profile names it, or new sessions land on it). */
const isProfileDb = computed(() => {
  const profile = useWorkspace().connection(props.session.connectionId);
  return details.value !== null && profile !== null && defaultDatabaseOf(profile) === details.value.name;
});
const renaming = computed(() => details.value !== null && form.name.trim() !== details.value.name);
const profileDbNote = computed(() => {
  if (!isProfileDb.value || !details.value) return null;
  if (editor.mode.value === 'drop') {
    return `${details.value.name} is this connection's own database: after the drop the object tree, new tabs and every reconnect fail until you choose another database in the connection settings.`;
  }
  if (renaming.value) {
    return followRename.value
      ? `${details.value.name} is this connection's own database: the connection is switched to the new name after the rename.`
      : `${details.value.name} is this connection's own database: after the rename the object tree, new tabs and reconnects fail until the connection uses the new name.`;
  }
  return null;
});

/**
 * After the rename ran and before the tree refreshes: point the connection
 * at the new name when asked, so the refresh opens the database by that name.
 * A keychain error means the connection was stored and only removing old
 * keychain entries failed: a warning, not a failure.
 */
async function followRenamedDatabase(): Promise<void> {
  const before = details.value;
  const newName = form.name.trim();
  if (editor.mode.value !== 'edit' || !before || !isProfileDb.value || !followRename.value || newName === before.name) return;
  const profile = useWorkspace().connection(props.session.connectionId);
  if (!profile) return;
  try {
    await updateConnection(profile.id, { ...inputFromProfile(profile), database: newName });
  } catch (error: unknown) {
    const cmd = toCmdError(error);
    if (cmd.kind === 'keychain') {
      useToasts().push(
        'warning',
        `The connection now uses ${newName}`,
        `Keychain entries of the previous settings could not be removed (Varve retries later): ${presentError(cmd).message}`,
        10_000,
      );
    } else {
      useToasts().pushError(error, 'The database was renamed, but the connection could not be switched to the new name');
    }
  }
  await useWorkspace().refresh();
}

async function applied(): Promise<void> {
  followRename.value = false;
  await editor.onApplied();
}

const showRunFrom = computed(() => onTarget.value && (editor.mode.value === 'drop' || (details.value !== null && needsDatabaseToItself(form as PgDatabaseForm, details.value))));
const subtitle = computed(() => {
  const db = details.value;
  if (!db) return '';
  const size = db.sizeBytes !== null ? ` · ${formatBytes(db.sizeBytes)}` : '';
  return `owner ${db.owner} · ${db.encoding} · ${db.localeProvider}${size}`;
});

watch(force, () => {
  if (editor.mode.value === 'drop') void editor.planNow();
});
watch(
  () => editor.mode.value,
  () => {
    force.value = false;
  },
);

function runElsewhere(): void {
  if (runFrom.value !== '') emit('switchDatabase', runFrom.value);
}

const ids = {
  name: useId(),
  owner: useId(),
  comment: useId(),
  template: useId(),
  templates: useId(),
  encoding: useId(),
  strategy: useId(),
  provider: useId(),
  locale: useId(),
  collate: useId(),
  ctype: useId(),
  icu: useId(),
  icuRules: useId(),
  builtin: useId(),
  tablespace: useId(),
  limit: useId(),
  settings: useId(),
  runFrom: useId(),
};

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Database"
    :environment="session.environment"
    :subtitle="subtitle"
    :tabs="tabs"
    :read-only="session.readOnly"
    :dirty-text="editor.dirty.value ? 'Unsaved changes' : ''"
    :loading="editor.loading.value"
    :load-error="editor.loadError.value"
    @reload="editor.load"
  >
    <template #actions>
      <button
        type="button"
        class="btn compact"
        :disabled="!editor.dirty.value"
        @click="editor.revert"
      >
        Revert
      </button>
      <button
        v-if="editor.current.value"
        type="button"
        class="btn compact"
        :disabled="session.readOnly || editor.mode.value === 'drop'"
        @click="editor.startDrop"
      >
        Drop
      </button>
    </template>

    <template #definition>
      <div class="layout">
        <div class="column">
          <section
            class="section"
            aria-label="Database"
          >
            <div class="grid">
              <FormField
                label="Name"
                :input-id="ids.name"
                required
                :error="errors.name ?? ''"
              >
                <input
                  :id="ids.name"
                  v-model="form.name"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                  :aria-invalid="!!errors.name"
                >
              </FormField>
              <FormField
                label="Owner"
                :input-id="ids.owner"
                :error="errors.owner ?? ''"
                :help="creating ? 'A role name; blank makes you the owner.' : 'A role name; blank keeps the current owner.'"
              >
                <input
                  :id="ids.owner"
                  v-model="form.owner"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                  :aria-invalid="!!errors.owner"
                >
              </FormField>
              <FormField
                label="Tablespace"
                :input-id="ids.tablespace"
                :help="creating ? 'Blank: pg_default.' : 'Moving copies every file and needs every session off the database.'"
              >
                <input
                  :id="ids.tablespace"
                  v-model="form.tablespace"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                label="Connection limit"
                :input-id="ids.limit"
                :error="errors.connectionLimit ?? ''"
                help="-1 for no limit."
              >
                <input
                  :id="ids.limit"
                  v-model="form.connectionLimit"
                  class="input mono"
                  inputmode="numeric"
                  :aria-invalid="!!errors.connectionLimit"
                >
              </FormField>
              <FormField
                label="Comment"
                :input-id="ids.comment"
                class="wide"
              >
                <textarea
                  :id="ids.comment"
                  v-model="form.comment"
                  class="textarea"
                  rows="2"
                />
              </FormField>
            </div>
            <label class="check">
              <input
                v-model="form.allowConnections"
                type="checkbox"
              ><span>Allow connections<span class="sub">Off refuses every new connection (existing ones stay).</span></span>
            </label>
            <label class="check">
              <input
                v-model="form.isTemplate"
                type="checkbox"
              ><span>Template<span class="sub">Any user with CREATEDB can clone it.</span></span>
            </label>
          </section>

          <section
            v-if="creating"
            class="section"
            aria-label="Encoding and locale"
          >
            <h2>Encoding and locale</h2>
            <p class="hint">
              Set when the database is created and fixed afterwards. Blank values come from the template. A different encoding or locale than the template's needs template0.
            </p>
            <div class="grid">
              <FormField
                label="Template"
                :input-id="ids.template"
                help="Blank: template1."
              >
                <input
                  :id="ids.template"
                  v-model="form.template"
                  class="input mono"
                  :list="ids.templates"
                  autocomplete="off"
                  spellcheck="false"
                >
                <datalist :id="ids.templates">
                  <option
                    v-for="d in databases"
                    :key="d"
                    :value="d"
                  />
                </datalist>
              </FormField>
              <FormField
                label="Encoding"
                :input-id="ids.encoding"
                help="For example UTF8."
              >
                <input
                  :id="ids.encoding"
                  v-model="form.encoding"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                v-if="major >= 15"
                label="Copy strategy"
                :input-id="ids.strategy"
                :error="errors.strategy ?? ''"
              >
                <select
                  :id="ids.strategy"
                  v-model="form.strategy"
                  class="select"
                >
                  <option value="">
                    Default (WAL_LOG)
                  </option>
                  <option value="walLog">
                    WAL_LOG
                  </option>
                  <option value="fileCopy">
                    FILE_COPY
                  </option>
                </select>
              </FormField>
              <FormField
                v-if="major >= 15"
                label="Locale provider"
                :input-id="ids.provider"
                :error="errors.localeProvider ?? ''"
              >
                <select
                  :id="ids.provider"
                  v-model="form.localeProvider"
                  class="select"
                >
                  <option value="">
                    From the template
                  </option>
                  <option value="libc">
                    libc
                  </option>
                  <option value="icu">
                    icu
                  </option>
                  <option
                    v-if="major >= 17"
                    value="builtin"
                  >
                    builtin
                  </option>
                </select>
              </FormField>
              <FormField
                label="Locale"
                :input-id="ids.locale"
                help="Sets LC_COLLATE and LC_CTYPE together."
              >
                <input
                  :id="ids.locale"
                  v-model="form.locale"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                label="LC_COLLATE"
                :input-id="ids.collate"
              >
                <input
                  :id="ids.collate"
                  v-model="form.lcCollate"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                label="LC_CTYPE"
                :input-id="ids.ctype"
              >
                <input
                  :id="ids.ctype"
                  v-model="form.lcCtype"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                v-if="major >= 15"
                label="ICU locale"
                :input-id="ids.icu"
                :error="errors.icuLocale ?? ''"
                help="For example en-US (icu provider)."
              >
                <input
                  :id="ids.icu"
                  v-model="form.icuLocale"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                  :aria-invalid="!!errors.icuLocale"
                >
              </FormField>
              <FormField
                v-if="major >= 16"
                label="ICU rules"
                :input-id="ids.icuRules"
                :error="errors.icuRules ?? ''"
              >
                <input
                  :id="ids.icuRules"
                  v-model="form.icuRules"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                v-if="major >= 17"
                label="Builtin locale"
                :input-id="ids.builtin"
                :error="errors.builtinLocale ?? ''"
                help="C or C.UTF-8 (builtin provider)."
              >
                <input
                  :id="ids.builtin"
                  v-model="form.builtinLocale"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                >
              </FormField>
            </div>
          </section>
          <section
            v-else-if="details"
            class="section"
            aria-label="Encoding and locale"
          >
            <h2>Encoding and locale</h2>
            <dl class="facts">
              <dt>Encoding</dt>
              <dd class="mono">
                {{ details.encoding }}
              </dd>
              <dt>Provider</dt>
              <dd class="mono">
                {{ details.localeProvider }}
              </dd>
              <dt>LC_COLLATE</dt>
              <dd class="mono">
                {{ details.collation }}
              </dd>
              <dt>LC_CTYPE</dt>
              <dd class="mono">
                {{ details.ctype }}
              </dd>
              <template v-if="details.locale">
                <dt>Locale</dt>
                <dd class="mono">
                  {{ details.locale }}
                </dd>
              </template>
              <template v-if="details.icuRules">
                <dt>ICU rules</dt>
                <dd class="mono">
                  {{ details.icuRules }}
                </dd>
              </template>
              <template v-if="details.collationVersion">
                <dt>Collation version</dt>
                <dd class="mono">
                  {{ details.collationVersion }}
                </dd>
              </template>
            </dl>
            <p class="hint">
              Encoding and locale are fixed when a database is created.
            </p>
          </section>
          <section
            v-if="profileDbNote && editor.mode.value === 'edit'"
            class="section"
            aria-label="Connection database"
          >
            <p class="notice tone-warning">
              {{ profileDbNote }}
            </p>
            <label class="check">
              <input
                v-model="followRename"
                type="checkbox"
              ><span>Point the connection at the new name after the rename<span class="sub">Changes the database in the connection settings; open tabs of the connection reconnect.</span></span>
            </label>
          </section>
          <section
            v-if="showRunFrom"
            class="section"
            aria-label="Run from another database"
          >
            <p class="notice tone-warning">
              {{ details ? onTargetText(details.name) : '' }}
            </p>
            <div class="row">
              <FormField
                label="Run from"
                :input-id="ids.runFrom"
              >
                <select
                  :id="ids.runFrom"
                  v-model="runFrom"
                  class="select mono"
                >
                  <option value="">
                    Choose a database
                  </option>
                  <option
                    v-for="d in otherDatabases"
                    :key="d"
                    :value="d"
                  >
                    {{ d }}
                  </option>
                </select>
              </FormField>
              <button
                type="button"
                class="btn compact"
                :disabled="runFrom === ''"
                @click="runElsewhere"
              >
                Reconnect this tab there
              </button>
            </div>
          </section>
        </div>
        <ChangePanel
          v-model:cascade="editor.cascade.value"
          :plan="editor.plan"
          engine="postgres"
          :read-only="session.readOnly"
          :mode="editor.mode.value"
          :creating="editor.creating.value"
          :object-name="title"
          :blocked="editor.dirty.value ? blocked : null"
          :cascade-available="false"
          :drop-blocked="dropBlocked"
          @applied="applied"
          @cancel-drop="editor.cancelDrop"
          @reconnect="emit('reconnect')"
          @refresh="editor.load"
        >
          <template #drop>
            <DropImpactPanel
              v-if="editor.current.value"
              :session="session"
              :object="editor.current.value"
              :force="force"
              :report-error="reportError"
              @loaded="impact = $event"
            />
            <p
              v-if="profileDbNote"
              class="notice tone-warning"
            >
              {{ profileDbNote }}
            </p>
            <label class="check force">
              <input
                v-model="force"
                type="checkbox"
              ><span>Terminate the other sessions first (WITH FORCE)<span class="sub">Ends every other connection to this database, rolling back its open transaction. Off: the drop fails while any session is connected.</span></span>
            </label>
          </template>
        </ChangePanel>
      </div>
    </template>

    <template #settings>
      <div class="layout single">
        <section
          class="section"
          aria-label="Session defaults"
        >
          <h2>Session defaults</h2>
          <p class="hint">
            ALTER DATABASE ... SET: defaults for every new session in this database, one name = value per line (for example work_mem = 64MB). Removing a line resets it. Settings for one role in this database are not shown here.
          </p>
          <FormField
            label="Settings"
            :input-id="ids.settings"
            :error="errors.settings ?? ''"
          >
            <textarea
              :id="ids.settings"
              v-model="form.settings"
              class="textarea mono"
              rows="8"
              spellcheck="false"
              :aria-invalid="!!errors.settings"
            />
          </FormField>
          <p class="hint">
            The SQL for these changes shows on the Database tab.
          </p>
        </section>
      </div>
    </template>

    <template #ddl>
      <DdlView
        v-if="editor.current.value"
        :session="session"
        :connection-name="connectionName"
        :object="editor.current.value"
        :report-error="reportError"
        @open-sql="emit('openSql', $event)"
        @changed="editor.load(); emit('changed')"
        @reconnect="emit('reconnect')"
      />
    </template>
  </EditorFrame>
</template>

<style scoped src="./editor.css"></style>
<style scoped>
.facts {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 4px 12px;
  margin: 0;
  font-size: 13px;
}

.facts dt {
  color: var(--text-muted);
}

.facts dd {
  margin: 0;
}

.row {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-end;
  gap: 8px;
}

.force {
  padding: 0 4px;
}
</style>
