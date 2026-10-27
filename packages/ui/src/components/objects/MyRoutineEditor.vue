<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { MyRoutine } from '../../api/ddl';
import { buildMyRoutine, errorsOf, myRoutineFormFrom, myRoutineSchema, objectRef } from '../../lib/ddlPlan';
import type { MyRoutineForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import DefinerFields from './DefinerFields.vue';
import EditorFrame from './EditorFrame.vue';
import MySessionNote from './MySessionNote.vue';
import SqlCode from './SqlCode.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** MySQL and MariaDB function and procedure editor (design canvas FunctionEditor). Use `RoutineEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const kind = computed<'function' | 'procedure'>(() => ((props.object?.kind ?? props.createKind) === 'function' ? 'function' : 'procedure'));
const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database ?? '');

const editor = useObjectEditor<MyRoutine, MyRoutineForm>({
  props,
  emit,
  pick: (d) =>
    d.engine === 'mysql' && (d.object.kind === 'function' || d.object.kind === 'procedure') ? d.object : 'This object is not a MySQL or MariaDB routine.',
  formFrom: (routine) => myRoutineFormFrom(routine, kind.value),
  validate: (form) => errorsOf(myRoutineSchema, form),
  blocked: () => (database.value === '' ? 'Choose a database first.' : null),
  build: (form, routine) => buildMyRoutine(form, routine, database.value, props.session.engine).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef(kind.value, database.value, null, form.name.trim()),
});
const { form, errors, details } = editor;

const isFunction = computed(() => kind.value === 'function');
const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Definition' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'mysql') : `New ${kind.value}`));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const renamed = computed(() => details.value !== null && form.name.trim() !== details.value.name);
const ids = { name: useId(), returns: useId(), access: useId(), security: useId(), comment: useId() };

function addParam(): void {
  form.params.push({ mode: 'in', name: '', dataType: '' });
}

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    :kind-label="isFunction ? 'Function' : 'Procedure'"
    :environment="session.environment"
    :subtitle="details?.definer ? `definer ${details.definer}` : ''"
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
            aria-label="Signature and options"
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
                  spellcheck="false"
                  :aria-invalid="!!errors.name"
                >
              </FormField>
              <FormField
                v-if="isFunction"
                label="Returns"
                :input-id="ids.returns"
                required
                :error="errors.returns ?? ''"
              >
                <input
                  :id="ids.returns"
                  v-model="form.returns"
                  class="input mono"
                  spellcheck="false"
                  placeholder="decimal(10,2)"
                  :aria-invalid="!!errors.returns"
                >
              </FormField>
            </div>
            <table class="table">
              <caption class="visually-hidden">
                Parameters
              </caption>
              <thead>
                <tr>
                  <th v-if="!isFunction">
                    Mode
                  </th>
                  <th>Parameter</th>
                  <th>Type</th>
                  <th><span class="visually-hidden">Remove</span></th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="(p, i) in form.params"
                  :key="i"
                >
                  <td v-if="!isFunction">
                    <select
                      v-model="p.mode"
                      class="select"
                      :aria-label="`Mode of parameter ${i + 1}`"
                    >
                      <option value="in">
                        IN
                      </option>
                      <option value="out">
                        OUT
                      </option>
                      <option value="inout">
                        INOUT
                      </option>
                    </select>
                  </td>
                  <td>
                    <input
                      v-model="p.name"
                      class="input mono"
                      :aria-label="`Name of parameter ${i + 1}`"
                      :aria-invalid="!!errors[`params.${i}.name`]"
                    >
                    <span
                      v-if="errors[`params.${i}.name`]"
                      class="field-error"
                    >{{ errors[`params.${i}.name`] }}</span>
                  </td>
                  <td>
                    <input
                      v-model="p.dataType"
                      class="input mono"
                      :aria-label="`Type of parameter ${i + 1}`"
                      :aria-invalid="!!errors[`params.${i}.dataType`]"
                    >
                    <span
                      v-if="errors[`params.${i}.dataType`]"
                      class="field-error"
                    >{{ errors[`params.${i}.dataType`] }}</span>
                  </td>
                  <td>
                    <button
                      type="button"
                      class="icon-btn"
                      :aria-label="`Remove parameter ${p.name || i + 1}`"
                      @click="form.params.splice(i, 1)"
                    >
                      <Icon name="close" />
                    </button>
                  </td>
                </tr>
              </tbody>
            </table>
            <div>
              <button
                type="button"
                class="btn compact"
                @click="addParam"
              >
                <Icon name="plus" />Add parameter
              </button>
            </div>
            <div class="grid">
              <FormField
                label="Data access"
                :input-id="ids.access"
              >
                <select
                  :id="ids.access"
                  v-model="form.dataAccess"
                  class="select"
                >
                  <option value="">
                    Server default
                  </option>
                  <option value="containsSql">
                    CONTAINS SQL
                  </option>
                  <option value="noSql">
                    NO SQL
                  </option>
                  <option value="readsSqlData">
                    READS SQL DATA
                  </option>
                  <option value="modifiesSqlData">
                    MODIFIES SQL DATA
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
              <label class="check">
                <input
                  v-model="form.deterministic"
                  type="checkbox"
                ><span>Deterministic</span>
              </label>
              <FormField
                label="Comment"
                :input-id="ids.comment"
                class="wide"
              >
                <input
                  :id="ids.comment"
                  v-model="form.comment"
                  class="input"
                >
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
          <section
            class="section"
            aria-label="Body"
          >
            <div class="section-head">
              <h2>Body</h2>
              <span class="hint">One statement, usually BEGIN ... END</span>
            </div>
            <div class="code-box tall">
              <SqlCode
                v-model="form.body"
                :engine="session.engine"
                label="Routine body"
              />
            </div>
            <span
              v-if="errors.body"
              class="field-error"
              role="alert"
            >{{ errors.body }}</span>
          </section>
        </div>
        <div class="column">
          <p
            v-if="renamed"
            class="notice tone-info"
            role="note"
          >
            <Icon name="info" />Renaming creates the routine under the new name, then drops the old one.
          </p>
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
          <MySessionNote
            v-if="details"
            :sql-mode="details.sqlMode"
            :charset-client="details.charsetClient"
            :collation-connection="details.collationConnection"
            :database-collation="details.databaseCollation"
          />
        </div>
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
