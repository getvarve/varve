<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { PgView } from '../../api/ddl';
import { buildPgView, errorsOf, objectRef, pgAtLeast, pgViewFormFrom, pgViewSchema } from '../../lib/ddlPlan';
import type { PgViewForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import SqlCode from './SqlCode.vue';
import { displayName, serverOf } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** PostgreSQL view editor (design canvas ViewEditor). Use `ViewEditor.vue`, which picks the engine. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const server = computed(() => serverOf(props.session));
const schemaDefault = computed(() => props.container?.schema ?? 'public');
const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);

const editor = useObjectEditor<PgView, PgViewForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'view' ? d.object : 'This object is not a PostgreSQL view.'),
  formFrom: (view) => pgViewFormFrom(view, schemaDefault.value),
  validate: (form) => errorsOf(pgViewSchema, form),
  build: (form, view) => buildPgView(form, view, database.value, server.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('view', database.value, form.schema.trim(), form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [
  { id: 'definition', label: 'Definition' },
  ...(details.value ? [{ id: 'columns', label: 'Columns', count: details.value.columns.length }] : []),
  ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : []),
]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : 'New view'));
const subtitle = computed(() => (details.value ? `owner ${details.value.owner}` : ''));
const invoker = computed(() => pgAtLeast(server.value, 15));
const checkId = useId();
const queryErrorId = useId();
const blocked = computed(() => {
  const first = Object.values(errors.value)[0];
  return first ?? editor.blockedReason.value;
});

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
              <span class="hint mono">{{ editor.current.value ? 'CREATE OR REPLACE VIEW' : 'CREATE VIEW' }} {{ form.schema }}.{{ form.name || '...' }} AS</span>
            </div>
            <div class="code-box tall">
              <SqlCode
                v-model="form.query"
                engine="postgres"
                label="View query"
                :described-by="errors.query ? queryErrorId : ''"
              />
            </div>
            <span
              v-if="errors.query"
              :id="queryErrorId"
              class="field-error"
              role="alert"
            >{{ errors.query }}</span>
            <p
              v-if="editor.current.value"
              class="hint"
            >
              CREATE OR REPLACE keeps dependent views. Renaming, dropping or retyping a column needs those views dropped and recreated first; the server refuses it otherwise.
            </p>
          </section>
          <ChangePanel
            v-model:cascade="editor.cascade.value"
            :plan="editor.plan"
            engine="postgres"
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
            <IdentityFields
              v-model:name="form.name"
              v-model:schema="form.schema"
              v-model:owner="form.owner"
              v-model:comment="form.comment"
              :session="session"
              :errors="errors"
            />
            <label class="check">
              <input
                v-model="form.securityBarrier"
                type="checkbox"
              ><span><span class="mono">security_barrier</span><span class="sub">Conditions run before user-supplied functions see the rows.</span></span>
            </label>
            <label
              v-if="invoker"
              class="check"
            >
              <input
                v-model="form.securityInvoker"
                type="checkbox"
              ><span><span class="mono">security_invoker</span><span class="sub">Check base table rights as the caller, not the view owner.</span></span>
            </label>
            <FormField
              label="Check option"
              :input-id="checkId"
              help="Only for automatically updatable views: rejects rows the view would not show."
            >
              <select
                :id="checkId"
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
          </section>
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
              <th>Default</th>
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
                {{ c.dataType }}
              </td>
              <td class="mono">
                {{ c.default ?? '' }}
              </td>
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
