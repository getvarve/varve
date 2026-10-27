<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import type { PgCompositeType } from '../../api/ddl';
import { buildPgComposite, compositeReordered, errorsOf, objectRef, pgCompositeFormFrom, pgCompositeSchema } from '../../lib/ddlPlan';
import type { PgCompositeForm } from '../../lib/ddlPlan';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import TypeList from './TypeList.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** PostgreSQL composite type editor (design canvas TypeEditor). Rendered by `TypeEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const schemaDefault = computed(() => props.container?.schema ?? 'public');

const editor = useObjectEditor<PgCompositeType, PgCompositeForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'compositeType' ? d.object : 'This object is not a composite type.'),
  formFrom: (type) => pgCompositeFormFrom(type, schemaDefault.value),
  validate: (form) => errorsOf(pgCompositeSchema, form),
  blocked: (form, type) => (type && compositeReordered(form, type) ? 'Attributes of an existing composite type cannot be reordered.' : null),
  build: (form, type) => buildPgComposite(form, type, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('compositeType', database.value, form.schema.trim(), form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Attributes' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const revision = ref(0);
const schema = computed(() => editor.current.value?.schema ?? schemaDefault.value);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : 'New composite type'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);

function add(): void {
  form.attributes.push({ name: '', dataType: '', collation: '', original: null });
}

async function applied(): Promise<void> {
  revision.value += 1;
  await editor.onApplied();
}

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Composite type"
    :environment="session.environment"
    :subtitle="details ? `owner ${details.meta.owner ?? ''}` : ''"
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
        <section
          class="section"
          aria-label="Attributes"
        >
          <h2>Attributes</h2>
          <table class="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Type</th>
                <th>Collation</th>
                <th><span class="visually-hidden">Remove</span></th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(a, i) in form.attributes"
                :key="i"
              >
                <td>
                  <input
                    v-model="a.name"
                    class="input mono"
                    :aria-label="`Name of attribute ${i + 1}`"
                    :aria-invalid="!!errors[`attributes.${i}.name`]"
                  >
                  <span
                    v-if="errors[`attributes.${i}.name`]"
                    class="field-error"
                  >{{ errors[`attributes.${i}.name`] }}</span>
                </td>
                <td>
                  <input
                    v-model="a.dataType"
                    class="input mono"
                    :aria-label="`Type of attribute ${i + 1}`"
                    :aria-invalid="!!errors[`attributes.${i}.dataType`]"
                  >
                  <span
                    v-if="errors[`attributes.${i}.dataType`]"
                    class="field-error"
                  >{{ errors[`attributes.${i}.dataType`] }}</span>
                </td>
                <td>
                  <input
                    v-model="a.collation"
                    class="input mono"
                    :aria-label="`Collation of attribute ${i + 1}`"
                    placeholder="default"
                  >
                </td>
                <td>
                  <button
                    type="button"
                    class="icon-btn"
                    :aria-label="`Remove attribute ${a.name || i + 1}`"
                    @click="form.attributes.splice(i, 1)"
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
              @click="add"
            >
              <Icon name="plus" />Add attribute
            </button>
          </div>
          <label
            v-if="details"
            class="check"
          >
            <input
              v-model="form.cascade"
              type="checkbox"
            ><span>CASCADE<span class="sub">Also change typed tables that use this type.</span></span>
          </label>
        </section>
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
</style>
