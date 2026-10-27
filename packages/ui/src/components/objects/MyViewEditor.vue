<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { MyView } from '../../api/ddl';
import { buildMyView, errorsOf, myViewFormFrom, myViewSchema, objectRef } from '../../lib/ddlPlan';
import type { MyViewForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import DefinerFields from './DefinerFields.vue';
import EditorFrame from './EditorFrame.vue';
import MySessionNote from './MySessionNote.vue';
import SqlCode from './SqlCode.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** MySQL and MariaDB view editor (design canvas ViewEditor). Use `ViewEditor.vue`, which picks the engine. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database ?? '');

const editor = useObjectEditor<MyView, MyViewForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'mysql' && d.object.kind === 'view' ? d.object : 'This object is not a MySQL or MariaDB view.'),
  formFrom: (view) => myViewFormFrom(view),
  validate: (form) => errorsOf(myViewSchema, form),
  blocked: () => (database.value === '' ? 'Choose a database first.' : null),
  build: (form, view) => buildMyView(form, view, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('view', database.value, null, form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [
  { id: 'definition', label: 'Definition' },
  ...(details.value ? [{ id: 'columns', label: 'Columns', count: details.value.columns.length }] : []),
  ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : []),
]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'mysql') : 'New view'));
const subtitle = computed(() => (details.value?.definer ? `definer ${details.value.definer}` : ''));
const ids = { name: useId(), algorithm: useId(), security: useId(), check: useId(), query: useId() };
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const renamed = computed(() => details.value !== null && form.name.trim() !== details.value.name);

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="View"
    :environment="session.environment"
    :subtitle="subtitle"
    :tabs="tabs"
    :read-only="session.readOnly"
    :dirty-text="editor.dirty.value ? 'Unsaved changes' : ''"
    :loading="editor.loading.value"
    :load-error="editor.loadError.value"
    @reload="editor.load"
  >
    <template #badges>
      <span
        v-if="details && details.updatable === false"
        class="pill tone-neutral"
      >Not updatable</span>
    </template>
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
            aria-label="SQL definition"
          >
            <div class="section-head">
              <h2>Definition</h2>
              <span class="hint mono">{{ editor.current.value && !renamed ? 'CREATE OR REPLACE VIEW' : 'CREATE VIEW' }} {{ form.name || '...' }} AS</span>
            </div>
            <div class="code-box tall">
              <SqlCode
                v-model="form.query"
                :engine="session.engine"
                label="View query"
                :described-by="errors.query ? ids.query : ''"
              />
            </div>
            <span
              v-if="errors.query"
              :id="ids.query"
              class="field-error"
              role="alert"
            >{{ errors.query }}</span>
            <p
              v-if="renamed"
              class="hint"
            >
              Renaming creates the view under the new name, then drops the old one.
            </p>
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
            @applied="editor.onApplied"
            @cancel-drop="editor.cancelDrop"
            @reconnect="emit('reconnect')"
            @refresh="editor.load"
          />
        </div>
        <div class="column">
          <section
            class="section"
            aria-label="Properties"
          >
            <h2>Properties</h2>
            <div class="grid">
              <FormField
                label="Name"
                :input-id="ids.name"
                required
                :error="errors.name ?? ''"
                class="wide"
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
                label="Algorithm"
                :input-id="ids.algorithm"
              >
                <select
                  :id="ids.algorithm"
                  v-model="form.algorithm"
                  class="select"
                >
                  <option value="">
                    Server default
                  </option>
                  <option value="undefined">
                    UNDEFINED
                  </option>
                  <option value="merge">
                    MERGE
                  </option>
                  <option value="temptable">
                    TEMPTABLE
                  </option>
                </select>
              </FormField>
              <FormField
                label="SQL security"
                :input-id="ids.security"
              >
                <select
                  :id="ids.security"
                  v-model="form.security"
                  class="select"
                >
                  <option value="">
                    Server default
                  </option>
                  <option value="definer">
                    DEFINER
                  </option>
                  <option value="invoker">
                    INVOKER
                  </option>
                </select>
              </FormField>
              <FormField
                label="Check option"
                :input-id="ids.check"
                class="wide"
                help="Only for updatable views: rejects rows the view would not show."
              >
                <select
                  :id="ids.check"
                  v-model="form.checkOption"
                  class="select"
                >
                  <option value="">
                    None
                  </option>
                  <option value="local">
                    LOCAL
                  </option>
                  <option value="cascaded">
                    CASCADED
                  </option>
                </select>
              </FormField>
            </div>
            <DefinerFields
              v-model:definer="form.definer"
              v-model:user="form.definerUser"
              v-model:host="form.definerHost"
              :errors="errors"
              :existing="details !== null && details.definer !== null"
            />
          </section>
          <MySessionNote
            v-if="details"
            :charset-client="details.charsetClient"
            :collation-connection="details.collationConnection"
          />
        </div>
      </div>
    </template>

    <template #columns>
      <div class="pad">
        <table
          v-if="details"
          class="table"
        >
          <caption class="visually-hidden">
            Columns of the view
          </caption>
          <thead>
            <tr>
              <th>#</th>
              <th>Name</th>
              <th>Type</th>
              <th>Nullable</th>
              <th>Comment</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="c in details.columns"
              :key="c.name"
            >
              <td class="mono subtle">
                {{ c.position }}
              </td>
              <td class="mono">
                {{ c.name }}
              </td>
              <td class="mono">
                {{ c.columnType }}
              </td>
              <td>{{ c.nullable ? 'yes' : 'no' }}</td>
              <td>{{ c.comment ?? '' }}</td>
            </tr>
          </tbody>
        </table>
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
