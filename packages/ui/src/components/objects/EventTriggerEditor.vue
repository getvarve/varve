<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { PgEventTrigger } from '../../api/ddl';
import { buildPgEventTrigger, errorsOf, objectRef, pgAtLeast, pgEventTriggerFormFrom, pgEventTriggerSchema } from '../../lib/ddlPlan';
import type { PgEventTriggerForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import { serverOf } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/**
 * PostgreSQL event trigger editor (design canvas TriggerEditor, "Event
 * triggers"): event, command tags, function, firing state, owner. Event
 * triggers are database wide. Props and events: `ObjectEditorProps` /
 * `ObjectEditorEmits`. Opens for an `eventTrigger` node.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const server = computed(() => serverOf(props.session));
const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);

const editor = useObjectEditor<PgEventTrigger, PgEventTriggerForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'eventTrigger' ? d.object : 'This object is not an event trigger.'),
  formFrom: (trigger) => pgEventTriggerFormFrom(trigger),
  validate: (form) => errorsOf(pgEventTriggerSchema(server.value), form),
  build: (form, trigger) => buildPgEventTrigger(form, trigger, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('eventTrigger', database.value, null, form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Event trigger' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? editor.current.value.name : 'New event trigger'));
const blocked = computed(() => Object.values(errors.value)[0] ?? null);
const ids = { event: useId(), tags: useId(), fn: useId() };
const EVENTS = computed(() => [
  { value: 'ddlCommandStart', label: 'ddl_command_start' },
  { value: 'ddlCommandEnd', label: 'ddl_command_end' },
  { value: 'tableRewrite', label: 'table_rewrite' },
  { value: 'sqlDrop', label: 'sql_drop' },
  ...(pgAtLeast(server.value, 17) ? [{ value: 'login', label: 'login' }] : []),
]);
const STATES = [
  { value: 'enabled', label: 'Origin' },
  { value: 'always', label: 'Always' },
  { value: 'replica', label: 'Replica' },
  { value: 'disabled', label: 'Disabled' },
] as const;

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Event trigger"
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
      <div class="layout">
        <section
          class="section"
          aria-label="Event trigger definition"
        >
          <p class="hint">
            Event triggers are database wide and fire for DDL by any user. Changing the event, tags or function drops and creates the trigger.
          </p>
          <IdentityFields
            v-model:name="form.name"
            v-model:owner="form.owner"
            v-model:comment="form.comment"
            :session="session"
            :errors="errors"
            no-schema
          />
          <div class="grid">
            <FormField
              label="Event"
              :input-id="ids.event"
              :error="errors.event ?? ''"
            >
              <select
                :id="ids.event"
                v-model="form.event"
                class="select mono"
              >
                <option
                  v-for="e in EVENTS"
                  :key="e.value"
                  :value="e.value"
                >
                  {{ e.label }}
                </option>
              </select>
            </FormField>
            <FormField
              label="Function"
              :input-id="ids.fn"
              required
              :error="errors.function ?? ''"
              help="A function returning event_trigger."
            >
              <input
                :id="ids.fn"
                v-model="form.function"
                class="input mono"
                spellcheck="false"
              >
            </FormField>
            <FormField
              label="Command tags"
              :input-id="ids.tags"
              class="wide"
              :error="errors.tags ?? ''"
              help="WHEN TAG IN: one per line or comma separated, for example CREATE TABLE. Blank fires for every command."
            >
              <textarea
                :id="ids.tags"
                v-model="form.tags"
                class="textarea"
                rows="3"
                spellcheck="false"
              />
            </FormField>
          </div>
          <div
            role="radiogroup"
            aria-label="Fires when"
            class="states"
          >
            <span class="label">Fires when</span>
            <label
              v-for="s in STATES"
              :key="s.value"
              class="check"
            ><input
              v-model="form.state"
              type="radio"
              :value="s.value"
            ><span>{{ s.label }}</span></label>
          </div>
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
.states {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  align-items: center;
}
</style>
