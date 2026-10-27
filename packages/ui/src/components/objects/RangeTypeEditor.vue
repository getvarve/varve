<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { PgRangeType } from '../../api/ddl';
import { buildPgRange, errorsOf, objectRef, pgRangeFormFrom, pgRangeSchema } from '../../lib/ddlPlan';
import type { PgRangeForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import TypeList from './TypeList.vue';
import { displayName, serverOf } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** PostgreSQL range type editor (design canvas TypeEditor). Rendered by `TypeEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const schemaDefault = computed(() => props.container?.schema ?? 'public');
const server = computed(() => serverOf(props.session));

const editor = useObjectEditor<PgRangeType, PgRangeForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'rangeType' ? d.object : 'This object is not a range type.'),
  formFrom: (type) => pgRangeFormFrom(type, schemaDefault.value),
  validate: (form) => errorsOf(pgRangeSchema(server.value), form),
  build: (form, type) => buildPgRange(form, type, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('rangeType', database.value, form.schema.trim(), form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Definition' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const revision = ref(0);
const schema = computed(() => editor.current.value?.schema ?? schemaDefault.value);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : 'New range type'));
const blocked = computed(() => Object.values(errors.value)[0] ?? null);
const fixed = computed(() => details.value !== null);
const ids = { subtype: useId(), opclass: useId(), collation: useId(), canonical: useId(), diff: useId(), multi: useId() };

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
    kind-label="Range type"
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
          aria-label="Range definition"
        >
          <p
            v-if="fixed"
            class="hint"
          >
            A range type's definition is fixed once created; only its name, schema, owner and comment change.
          </p>
          <div class="grid">
            <FormField
              label="Subtype"
              :input-id="ids.subtype"
              required
              :error="errors.subtype ?? ''"
            >
              <input
                :id="ids.subtype"
                v-model="form.subtype"
                class="input mono"
                :disabled="fixed"
              >
            </FormField>
            <FormField
              label="Subtype operator class"
              :input-id="ids.opclass"
            >
              <input
                :id="ids.opclass"
                v-model="form.subtypeOpclass"
                class="input mono"
                :disabled="fixed"
                placeholder="default"
              >
            </FormField>
            <FormField
              label="Collation"
              :input-id="ids.collation"
            >
              <input
                :id="ids.collation"
                v-model="form.collation"
                class="input mono"
                :disabled="fixed"
                placeholder="default"
              >
            </FormField>
            <FormField
              label="Canonical function"
              :input-id="ids.canonical"
            >
              <input
                :id="ids.canonical"
                v-model="form.canonical"
                class="input mono"
                :disabled="fixed"
                placeholder="none"
              >
            </FormField>
            <FormField
              label="Subtype diff function"
              :input-id="ids.diff"
            >
              <input
                :id="ids.diff"
                v-model="form.subtypeDiff"
                class="input mono"
                :disabled="fixed"
                placeholder="none"
              >
            </FormField>
            <FormField
              label="Multirange type name"
              :input-id="ids.multi"
              :error="errors.multirange ?? ''"
            >
              <input
                :id="ids.multi"
                v-model="form.multirange"
                class="input mono"
                :disabled="fixed"
                placeholder="generated"
              >
            </FormField>
          </div>
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
