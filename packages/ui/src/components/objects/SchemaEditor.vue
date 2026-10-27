<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import type { PgSchema } from '../../api/ddl';
import { buildPgSchema, pgSchemaErrors, pgSchemaFormFrom } from '../../lib/ddlDatabase';
import type { PgSchemaForm } from '../../lib/ddlDatabase';
import { objectRef } from '../../lib/ddlPlan';
import type { FieldErrors } from '../../lib/ddlPlan';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import DropImpactPanel from './DropImpactPanel.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/**
 * PostgreSQL schema editor: name, owner (`AUTHORIZATION`), comment; drop
 * with CASCADE, listing what the schema holds and what outside it depends
 * on its objects. Props and events: `ObjectEditorProps` /
 * `ObjectEditorEmits`. Opens for a `schema` node, or with `object: null`,
 * `createKind: 'schema'` and the database in `container` ("New schema").
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);

const editor = useObjectEditor<PgSchema, PgSchemaForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'schema' ? d.object : 'This object is not a schema.'),
  formFrom: (schema) => pgSchemaFormFrom(schema),
  validate: (form): FieldErrors => pgSchemaErrors(form, editor.details.value),
  blocked: (_form, schema) => {
    if (!schema) return null;
    if (schema.extension) return `Extension ${schema.extension} owns this schema; change it through the extension.`;
    return schema.system ? 'This is a system schema; it cannot be changed here.' : null;
  },
  build: (form, schema) => buildPgSchema(form, schema, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('schema', database.value, null, form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Schema' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? editor.current.value.name : 'New schema'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const dropBlocked = computed(() => (details.value?.system || details.value?.extension ? 'System and extension schemas are not dropped from here.' : null));

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Schema"
    :environment="session.environment"
    :subtitle="details ? `owner ${details.owner}${database ? ` · database ${database}` : ''}` : database ? `in database ${database}` : ''"
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
          aria-label="Schema definition"
        >
          <p class="hint">
            A schema groups tables, views, functions and types inside database {{ database ?? '' }}. Renaming it changes the qualified name of everything in it; code that names the old schema stops working.
          </p>
          <IdentityFields
            v-model:name="form.name"
            v-model:owner="form.owner"
            v-model:comment="form.comment"
            :session="session"
            :errors="errors"
            no-schema
            :disabled="!!editor.blockedReason.value"
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
          :drop-blocked="dropBlocked"
          @applied="editor.onApplied"
          @cancel-drop="editor.cancelDrop"
          @reconnect="emit('reconnect')"
          @refresh="editor.load"
        >
          <template #drop>
            <DropImpactPanel
              v-if="editor.current.value"
              :session="session"
              :object="editor.current.value"
              :cascade="editor.cascade.value"
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
