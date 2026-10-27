<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import { catalogDescribe } from '../../api/ddl';
import type { MyTrigger } from '../../api/ddl';
import { isMissing } from '../../lib/ddlApply';
import { buildMyTrigger, errorsOf, myTriggerFormFrom, myTriggerSchema, objectRef } from '../../lib/ddlPlan';
import type { MyTriggerForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import DefinerFields from './DefinerFields.vue';
import EditorFrame from './EditorFrame.vue';
import MySessionNote from './MySessionNote.vue';
import SqlCode from './SqlCode.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** MySQL and MariaDB trigger editor (design canvas TriggerEditor). Use `TriggerEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database ?? '');
const table = ref(props.object?.parent ?? props.container?.parent ?? '');
/** Other triggers on the table, for FOLLOWS / PRECEDES. */
const siblings = ref<string[]>([]);

async function loadSiblings(trigger: MyTrigger | null): Promise<void> {
  if (trigger) table.value = trigger.table;
  siblings.value = [];
  if (!table.value) return;
  try {
    const d = await catalogDescribe(props.session.id, objectRef('table', database.value, null, table.value));
    if (d.engine === 'mysql' && d.object.kind === 'table') siblings.value = d.object.triggers;
  } catch (error: unknown) {
    if (!isMissing(error)) throw error;
  }
}

const editor = useObjectEditor<MyTrigger, MyTriggerForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'mysql' && d.object.kind === 'trigger' ? d.object : 'This object is not a MySQL or MariaDB trigger.'),
  afterLoad: loadSiblings,
  formFrom: (trigger) => myTriggerFormFrom(trigger),
  validate: (form) => errorsOf(myTriggerSchema(props.session.engine), form),
  blocked: () => (database.value === '' || table.value === '' ? 'The trigger needs a database and a table.' : null),
  build: (form, trigger) => buildMyTrigger(form, trigger, database.value, table.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('trigger', database.value, null, form.name.trim(), { parent: table.value }),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Definition' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? editor.current.value.name : 'New trigger'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const others = computed(() => siblings.value.filter((t) => t !== details.value?.name));
const multi = computed(() => props.session.engine === 'mariadb');
const ids = { name: useId(), order: useId(), other: useId() };

function pickEvent(event: 'insert' | 'update' | 'delete'): void {
  form.insert = event === 'insert';
  form.update = event === 'update';
  form.delete = event === 'delete';
}

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Trigger"
    :environment="session.environment"
    :subtitle="table ? `on ${database}.${table}` : ''"
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
            aria-label="Trigger definition"
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
            </div>
            <div class="row">
              <span class="label">Timing</span>
              <div
                role="radiogroup"
                aria-label="Timing"
                class="segmented"
              >
                <button
                  v-for="t in (['before', 'after'] as const)"
                  :key="t"
                  type="button"
                  role="radio"
                  :aria-checked="form.timing === t"
                  @click="form.timing = t"
                >
                  {{ t.toUpperCase() }}
                </button>
              </div>
            </div>
            <fieldset class="events">
              <legend class="label">
                {{ multi ? 'Events' : 'Event' }}
              </legend>
              <template v-if="multi">
                <label class="check"><input
                  v-model="form.insert"
                  type="checkbox"
                ><span class="mono">INSERT</span></label>
                <label class="check"><input
                  v-model="form.update"
                  type="checkbox"
                ><span class="mono">UPDATE</span></label>
                <label class="check"><input
                  v-model="form.delete"
                  type="checkbox"
                ><span class="mono">DELETE</span></label>
              </template>
              <template v-else>
                <label
                  v-for="e in (['insert', 'update', 'delete'] as const)"
                  :key="e"
                  class="check"
                ><input
                  type="radio"
                  name="my-trigger-event"
                  :checked="form[e]"
                  @change="pickEvent(e)"
                ><span class="mono">{{ e.toUpperCase() }}</span></label>
              </template>
              <span
                v-if="errors.events"
                class="field-error"
                role="alert"
              >{{ errors.events }}</span>
            </fieldset>
            <div class="grid">
              <FormField
                label="Order"
                :input-id="ids.order"
              >
                <select
                  :id="ids.order"
                  v-model="form.orderKind"
                  class="select"
                >
                  <option value="">
                    Default
                  </option>
                  <option value="follows">
                    FOLLOWS
                  </option>
                  <option value="precedes">
                    PRECEDES
                  </option>
                </select>
              </FormField>
              <FormField
                v-if="form.orderKind !== ''"
                label="Other trigger"
                :input-id="ids.other"
                :error="errors.orderTrigger ?? ''"
              >
                <select
                  :id="ids.other"
                  v-model="form.orderTrigger"
                  class="select mono"
                >
                  <option
                    v-for="t in others"
                    :key="t"
                    :value="t"
                  >
                    {{ t }}
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
          <section
            class="section"
            aria-label="Body"
          >
            <h2>Body</h2>
            <div class="code-box tall">
              <SqlCode
                v-model="form.body"
                :engine="session.engine"
                label="Trigger body"
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
            v-if="details && session.engine === 'mysql' && editor.dirty.value"
            class="notice tone-info"
            role="note"
          >
            <Icon name="info" />MySQL has no CREATE OR REPLACE TRIGGER: saving drops the trigger and creates it again.
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
.row {
  display: flex;
  align-items: center;
  gap: 12px;
}

.events {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  margin: 0;
  padding: 12px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
}
</style>
