<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { PgEnumType } from '../../api/ddl';
import { buildPgEnum, enumNeedsRecreate, errorsOf, objectRef, pgEnumFormFrom, pgEnumSchema } from '../../lib/ddlPlan';
import type { PgEnumForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import TypeList from './TypeList.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** PostgreSQL enum type editor (design canvas TypeEditor). Rendered by `TypeEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const schemaDefault = computed(() => props.container?.schema ?? 'public');

const editor = useObjectEditor<PgEnumType, PgEnumForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'enumType' ? d.object : 'This object is not an enum type.'),
  formFrom: (type) => pgEnumFormFrom(type, schemaDefault.value),
  validate: (form) => errorsOf(pgEnumSchema, form),
  blocked: (form, type) => (type ? enumNeedsRecreate(form, type) : null),
  build: (form, type) => buildPgEnum(form, type, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('enumType', database.value, form.schema.trim(), form.name.trim()),
});
const { form, errors, details } = editor;

const newValue = ref('');
const newId = useId();
const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Values' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const revision = ref(0);
const schema = computed(() => editor.current.value?.schema ?? schemaDefault.value);
const added = computed(() => form.values.filter((v) => v.original === null).length);
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : 'New enum'));

function add(): void {
  const v = newValue.value;
  if (v === '') return;
  form.values.push({ label: v, original: null });
  newValue.value = '';
}

function move(index: number, step: -1 | 1): void {
  const target = index + step;
  const item = form.values[index];
  if (!item || target < 0 || target >= form.values.length) return;
  form.values.splice(index, 1);
  form.values.splice(target, 0, item);
}

onMounted(() => void editor.load());

async function applied(): Promise<void> {
  revision.value += 1;
  await editor.onApplied();
}
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Enum"
    :environment="session.environment"
    :subtitle="details ? `owner ${details.meta.owner ?? ''}` : ''"
    :tabs="tabs"
    :read-only="session.readOnly"
    :dirty-text="editor.dirty.value ? (added > 0 ? `${added} value${added === 1 ? '' : 's'} added` : 'Unsaved changes') : ''"
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
      <div class="type-layout">
        <TypeList
          :session="session"
          :database="database"
          :schema="schema"
          :current="editor.current.value"
          :revision="revision"
          :report-error="reportError"
          @open-object="emit('openObject', $event)"
          @create-object="(k, c) => emit('createObject', k, c)"
        />
        <div class="column">
          <section
            class="section"
            aria-label="Enum values"
          >
            <div class="section-head">
              <h2>Values</h2>
              <span class="hint">in sort order</span>
            </div>
            <ol
              class="list"
              :aria-label="`${title} values`"
            >
              <li
                v-for="(v, i) in form.values"
                :key="i"
                class="value-row"
              >
                <span class="subtle mono">{{ i + 1 }}</span>
                <input
                  v-model="v.label"
                  class="input mono"
                  :aria-label="`Value ${i + 1}`"
                  :aria-invalid="!!errors[`values.${i}`]"
                >
                <span
                  v-if="v.original === null"
                  class="pill tone-info"
                >new</span>
                <span
                  v-else-if="v.original !== v.label"
                  class="pill tone-warning"
                >renamed</span>
                <template v-if="v.original === null || editor.creating.value">
                  <button
                    type="button"
                    class="icon-btn"
                    :aria-label="`Move ${v.label || 'value'} up`"
                    @click="move(i, -1)"
                  >
                    <Icon
                      name="chevronDown"
                      class="up"
                    />
                  </button>
                  <button
                    type="button"
                    class="icon-btn"
                    :aria-label="`Move ${v.label || 'value'} down`"
                    @click="move(i, 1)"
                  >
                    <Icon name="chevronDown" />
                  </button>
                  <button
                    type="button"
                    class="icon-btn"
                    :aria-label="`Remove ${v.label || 'value'}`"
                    @click="form.values.splice(i, 1)"
                  >
                    <Icon name="close" />
                  </button>
                </template>
                <span
                  v-if="errors[`values.${i}`]"
                  class="field-error"
                >{{ errors[`values.${i}`] }}</span>
              </li>
            </ol>
            <FormField
              label="Add value"
              :input-id="newId"
              help="New values go where you place them; existing values keep their place."
            >
              <div class="add">
                <input
                  :id="newId"
                  v-model="newValue"
                  class="input mono"
                  placeholder="e.g. on_hold"
                  @keydown.enter.prevent="add"
                >
                <button
                  type="button"
                  class="btn compact"
                  :disabled="newValue === ''"
                  @click="add"
                >
                  Add
                </button>
              </div>
            </FormField>
          </section>
          <div
            v-if="details"
            class="notice tone-info"
            role="note"
          >
            <Icon name="info" />
            <span>Existing values cannot be removed or moved in place. That needs the type recreated: a new type, ALTER COLUMN ... USING on every column that uses it, then dropping the old one. Each new value is added by its own statement and committed before it can be used.</span>
          </div>
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
            @applied="applied"
            @cancel-drop="editor.cancelDrop"
            @reconnect="emit('reconnect')"
            @refresh="editor.load"
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
<style scoped>
.value-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}

.value-row .input {
  flex: 1 1 160px;
  height: 28px;
}

.up {
  transform: rotate(180deg);
}

.type-layout {
  display: grid;
  grid-template-columns: 260px minmax(0, 1fr) 380px;
  gap: 16px;
  padding: 16px;
  align-items: start;
}

@media (max-width: 1280px) {
  .type-layout {
    grid-template-columns: minmax(0, 1fr);
  }
}

.add {
  display: flex;
  gap: 8px;
}
</style>
