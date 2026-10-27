<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { MyDatabase } from '../../api/ddl';
import { buildMyDatabase, myDatabaseErrors, myDatabaseFeatures, myDatabaseFormFrom } from '../../lib/ddlDatabase';
import type { MyDatabaseForm } from '../../lib/ddlDatabase';
import { objectRef } from '../../lib/ddlPlan';
import type { FieldErrors } from '../../lib/ddlPlan';
import { formatBytes } from '../../lib/values';
import FormField from '../common/FormField.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import DropImpactPanel from './DropImpactPanel.vue';
import EditorFrame from './EditorFrame.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/**
 * MySQL and MariaDB database editor (design doc 4.2: charset, collation):
 * default character set and collation, default encryption (MySQL 8.0.16+),
 * read only (MySQL 8.0.22+) and comment (MariaDB 10.5+). Databases cannot be
 * renamed. Drop lists what goes with it, foreign keys and views elsewhere
 * that depend on it, and the connections using it (MySQL and MariaDB do not
 * refuse a drop for them). Props and events: `ObjectEditorProps` /
 * `ObjectEditorEmits`.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const features = computed(() => myDatabaseFeatures(props.session.server));

const editor = useObjectEditor<MyDatabase, MyDatabaseForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'mysql' && d.object.kind === 'database' ? d.object : 'This object is not a database.'),
  formFrom: (db) => myDatabaseFormFrom(db),
  validate: (form): FieldErrors => myDatabaseErrors(form, editor.details.value, features.value),
  blocked: (_form, db) => (db?.system ? `${db.name} is a system database; it is not changed here.` : null),
  build: (form, db) => buildMyDatabase(form, db).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('database', form.name.trim(), null, form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Database' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const creating = computed(() => editor.current.value === null);
const title = computed(() => (editor.current.value ? editor.current.value.name : 'New database'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const dropBlocked = computed(() => {
  if (details.value?.system) return `${details.value.name} is a system database; it is not dropped from here.`;
  if (details.value?.readOnly === true) return `${details.value.name} is read only (READ ONLY = 1): MySQL refuses to drop it until that is turned off.`;
  return null;
});

/** The separate, explicit step before dropping a read-only database: plan READ ONLY = 0 for review. */
function makeWritable(): void {
  editor.cancelDrop();
  form.readOnly = 'off';
}
const subtitle = computed(() => {
  const db = details.value;
  if (!db) return '';
  const parts = [db.charset, db.collation].filter((p): p is string => !!p);
  if (db.tableCount !== null) parts.push(`${db.tableCount} ${db.tableCount === 1 ? 'table' : 'tables'}`);
  if (db.sizeBytes !== null) parts.push(formatBytes(db.sizeBytes));
  return parts.join(' · ');
});
const ids = { name: useId(), charset: useId(), collation: useId(), charsets: useId(), encryption: useId(), readOnly: useId(), comment: useId() };
const CHARSETS = ['utf8mb4', 'utf8mb3', 'latin1', 'ascii', 'binary', 'utf16', 'utf32'];

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
        <section
          class="section"
          aria-label="Database"
        >
          <p class="hint">
            The default character set and collation apply to tables created later; existing tables keep theirs.
          </p>
          <div class="grid">
            <FormField
              label="Name"
              :input-id="ids.name"
              required
              :error="errors.name ?? ''"
              :help="creating ? '' : 'Databases cannot be renamed.'"
            >
              <input
                :id="ids.name"
                v-model="form.name"
                class="input mono"
                autocomplete="off"
                spellcheck="false"
                :readonly="!creating"
                :aria-invalid="!!errors.name"
              >
            </FormField>
            <FormField
              label="Character set"
              :input-id="ids.charset"
              :error="errors.charset ?? ''"
              help="Blank: the server default."
            >
              <input
                :id="ids.charset"
                v-model="form.charset"
                class="input mono"
                :list="ids.charsets"
                autocomplete="off"
                spellcheck="false"
                :aria-invalid="!!errors.charset"
              >
              <datalist :id="ids.charsets">
                <option
                  v-for="c in CHARSETS"
                  :key="c"
                  :value="c"
                />
              </datalist>
            </FormField>
            <FormField
              label="Collation"
              :input-id="ids.collation"
              :error="errors.collation ?? ''"
              help="Blank: the character set's default."
            >
              <input
                :id="ids.collation"
                v-model="form.collation"
                class="input mono"
                autocomplete="off"
                spellcheck="false"
                :aria-invalid="!!errors.collation"
              >
            </FormField>
            <FormField
              v-if="features.encryption"
              label="Default encryption"
              :input-id="ids.encryption"
              :error="errors.encryption ?? ''"
            >
              <select
                :id="ids.encryption"
                v-model="form.encryption"
                class="select"
              >
                <option value="">
                  Server default
                </option>
                <option value="on">
                  Encrypted (Y)
                </option>
                <option value="off">
                  Not encrypted (N)
                </option>
              </select>
            </FormField>
            <FormField
              v-if="features.readOnly && !creating"
              label="Read only"
              :input-id="ids.readOnly"
              :error="errors.readOnly ?? ''"
              help="Refuses changes to the database and its objects."
            >
              <select
                :id="ids.readOnly"
                v-model="form.readOnly"
                class="select"
              >
                <option value="">
                  Unchanged
                </option>
                <option value="on">
                  Read only
                </option>
                <option value="off">
                  Writable
                </option>
              </select>
            </FormField>
            <FormField
              v-if="features.comment"
              label="Comment"
              :input-id="ids.comment"
              class="wide"
              :error="errors.comment ?? ''"
            >
              <textarea
                :id="ids.comment"
                v-model="form.comment"
                class="textarea"
                rows="2"
              />
            </FormField>
          </div>
        </section>
        <ChangePanel
          v-model:cascade="editor.cascade.value"
          :plan="editor.plan"
          :engine="session.engine"
          :read-only="session.readOnly"
          :mode="editor.mode.value"
          :creating="editor.creating.value"
          :object-name="title"
          :blocked="editor.dirty.value ? blocked : null"
          :cascade-available="false"
          :drop-blocked="dropBlocked"
          @applied="editor.onApplied"
          @cancel-drop="editor.cancelDrop"
          @reconnect="emit('reconnect')"
          @refresh="editor.load"
        >
          <template #drop>
            <div
              v-if="details?.readOnly === true"
              class="notice tone-warning"
            >
              <span class="grow">The database is read only. Turn that off first (ALTER DATABASE ... READ ONLY = 0, reviewed on its own), then drop it.</span>
              <button
                type="button"
                class="btn compact"
                :disabled="session.readOnly"
                @click="makeWritable"
              >
                Make it writable first
              </button>
            </div>
            <DropImpactPanel
              v-if="editor.current.value"
              :session="session"
              :object="editor.current.value"
              :report-error="reportError"
            />
          </template>
        </ChangePanel>
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
