<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { catalogList } from '../../api/catalog';
import type { ObjectKind, ObjectSummary } from '../../api/catalog';
import { catalogDescribe } from '../../api/ddl';
import type { PgFunction, PgTrigger } from '../../api/ddl';
import { isMissing } from '../../lib/ddlApply';
import { parseQualified } from '../../lib/ddlText';
import { buildPgTrigger, errorsOf, objectRef, pgAtLeast, pgTriggerFormFrom, pgTriggerSchema } from '../../lib/ddlPlan';
import type { PgTriggerForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import SqlCode from './SqlCode.vue';
import { serverOf } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/** PostgreSQL table and view trigger editor (design canvas TriggerEditor). Use `TriggerEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const server = computed(() => serverOf(props.session));
const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const tableSchema = computed(() => props.object?.schema ?? props.container?.schema ?? 'public');
const tableName = computed(() => props.object?.parent ?? props.container?.parent ?? '');
/** Kind of the table the trigger is on, found while loading (INSTEAD OF needs a view). */
const tableKind = ref<ObjectKind>('table');
const siblings = ref<PgTrigger[]>([]);

const TABLE_KINDS: ObjectKind[] = ['table', 'partitionedTable', 'view', 'foreignTable', 'partition'];

/** The parent relation's triggers; tries the relation kinds until the catalog knows it. */
async function loadParent(): Promise<void> {
  siblings.value = [];
  if (!tableName.value) return;
  for (const kind of TABLE_KINDS) {
    try {
      const d = await catalogDescribe(props.session.id, objectRef(kind, database.value, tableSchema.value, tableName.value));
      if (d.engine === 'postgres' && 'triggers' in d.object) {
        tableKind.value = kind;
        siblings.value = d.object.triggers;
        return;
      }
    } catch (error: unknown) {
      // Not this kind of relation: try the next one. Anything else is a real failure.
      if (!isMissing(error)) {
        props.reportError?.(error);
        throw error;
      }
    }
  }
}

const editor = useObjectEditor<PgTrigger, PgTriggerForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'trigger' ? d.object : 'This object is not a PostgreSQL trigger.'),
  afterLoad: loadParent,
  formFrom: (trigger) => pgTriggerFormFrom(trigger),
  validate: (form) => errorsOf(pgTriggerSchema(tableKind.value === 'view'), form),
  blocked: () => (tableName.value === '' ? 'The trigger needs a table.' : null),
  build: (form, trigger) =>
    buildPgTrigger(form, trigger, { schema: tableSchema.value, name: tableName.value, kind: tableKind.value }, database.value, server.value).map((change) => ({
      kind: 'edit',
      change,
    })),
  identity: (form) => objectRef('trigger', database.value, tableSchema.value, form.name.trim(), { parent: tableName.value }),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [
  { id: 'definition', label: 'Table triggers', count: siblings.value.length },
  ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : []),
]);
const title = computed(() => (editor.current.value ? editor.current.value.name : 'New trigger'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const ids = { name: useId(), when: useId(), fn: useId(), args: useId(), col: useId(), oldTable: useId(), newTable: useId(), comment: useId() };
const newColumn = ref('');
const argsText = computed({
  get: () => form.arguments.join(', '),
  set: (value: string) => {
    form.arguments = value.split(',').map((v) => v.trim()).filter((v) => v !== '');
  },
});

function addColumn(): void {
  const c = newColumn.value.trim();
  if (c !== '' && !form.updateColumns.includes(c)) form.updateColumns.push(c);
  newColumn.value = '';
}

const STATES = [
  { value: 'enabled', label: 'Origin', help: 'default, ENABLE' },
  { value: 'always', label: 'Always', help: 'also on replicas' },
  { value: 'replica', label: 'Replica', help: 'replica role only' },
  { value: 'disabled', label: 'Disabled', help: 'never fires' },
] as const;

function triggerSummary(t: PgTrigger): string {
  const timing = t.timing === 'insteadOf' ? 'INSTEAD OF' : t.timing.toUpperCase();
  const state = t.enabled === 'disabled' ? ' · disabled' : '';
  return `${timing} ${t.events.map((e) => e.toUpperCase()).join(', ')} · ${t.level}${state}`;
}

// Trigger function preview.
const fnPreview = ref<PgFunction | null>(null);
const fnList = ref<ObjectSummary[]>([]);
watch(
  () => form.function,
  async (text) => {
    fnPreview.value = null;
    const q = parseQualified(text);
    if (q.name === '') return;
    try {
      const d = await catalogDescribe(props.session.id, objectRef('function', database.value, q.schema ?? tableSchema.value, q.name, { signature: '' }));
      if (d.engine === 'postgres' && d.object.kind === 'function') fnPreview.value = d.object;
    } catch (error: unknown) {
      // Not a known function (yet, or overloaded): the preview stays empty.
      if (!isMissing(error)) props.reportError?.(error);
    }
  },
);

onMounted(async () => {
  await editor.load();
  try {
    const list = await catalogList(props.session.id, { database: database.value, schema: tableSchema.value, parent: null }, 'function');
    fnList.value = list.filter((f) => (f.detail ?? '').includes('trigger'));
  } catch (error: unknown) {
    props.reportError?.(error);
  }
});
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Trigger"
    :environment="session.environment"
    :subtitle="`on ${tableSchema}.${tableName}`"
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
      <div class="trigger-layout">
        <section
          class="section"
          :aria-label="`Triggers on ${tableName}`"
        >
          <div class="section-head">
            <h2>On {{ tableName }}</h2>
            <span class="grow" />
            <button
              type="button"
              class="icon-btn"
              aria-label="New trigger"
              @click="emit('createObject', 'trigger', { database, schema: tableSchema, parent: tableName })"
            >
              <Icon name="plus" />
            </button>
          </div>
          <ul class="list">
            <li
              v-for="t in siblings"
              :key="t.name"
            >
              <button
                type="button"
                class="list-item stacked"
                :aria-current="t.name === editor.current.value?.name"
                @click="emit('openObject', objectRef('trigger', database, t.schema, t.name, { parent: t.table }))"
              >
                <span class="mono">{{ t.name }}</span>
                <span class="hint">{{ triggerSummary(t) }}</span>
              </button>
            </li>
          </ul>
        </section>

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
                v-for="t in (['before', 'after', 'insteadOf'] as const)"
                :key="t"
                type="button"
                role="radio"
                :aria-checked="form.timing === t"
                :disabled="t === 'insteadOf' && tableKind !== 'view'"
                :title="t === 'insteadOf' && tableKind !== 'view' ? 'Only on views' : undefined"
                @click="form.timing = t"
              >
                {{ t === 'insteadOf' ? 'INSTEAD OF' : t.toUpperCase() }}
              </button>
            </div>
            <span class="label">Level</span>
            <div
              role="radiogroup"
              aria-label="Level"
              class="segmented"
            >
              <button
                type="button"
                role="radio"
                :aria-checked="form.level === 'row'"
                @click="form.level = 'row'"
              >
                FOR EACH ROW
              </button>
              <button
                type="button"
                role="radio"
                :aria-checked="form.level === 'statement'"
                @click="form.level = 'statement'"
              >
                STATEMENT
              </button>
            </div>
          </div>
          <span
            v-if="errors.timing || errors.level"
            class="field-error"
          >{{ errors.timing ?? errors.level }}</span>
          <fieldset class="events">
            <legend class="label">
              Events
            </legend>
            <label class="check"><input
              v-model="form.insert"
              type="checkbox"
            ><span class="mono">INSERT</span></label>
            <label class="check"><input
              v-model="form.update"
              type="checkbox"
            ><span class="mono">UPDATE{{ form.updateColumns.length > 0 ? ' OF' : '' }}</span></label>
            <div
              v-if="form.update"
              class="chips"
            >
              <span
                v-for="(c, i) in form.updateColumns"
                :key="c"
                class="chip"
              >{{ c }}<button
                type="button"
                :aria-label="`Remove ${c}`"
                @click="form.updateColumns.splice(i, 1)"
              ><Icon
                name="close"
                :size="12"
              /></button></span>
              <label
                :for="ids.col"
                class="visually-hidden"
              >Add column</label>
              <input
                :id="ids.col"
                v-model="newColumn"
                class="input mono col-input"
                placeholder="Add column"
                @keydown.enter.prevent="addColumn"
                @blur="addColumn"
              >
            </div>
            <label class="check"><input
              v-model="form.delete"
              type="checkbox"
            ><span class="mono">DELETE</span></label>
            <label class="check"><input
              v-model="form.truncate"
              type="checkbox"
            ><span class="mono">TRUNCATE</span><span class="sub">statement level only</span></label>
            <span
              v-if="errors.events"
              class="field-error"
              role="alert"
            >{{ errors.events }}</span>
          </fieldset>
          <FormField
            label="WHEN condition"
            :input-id="ids.when"
            :error="errors.condition ?? ''"
          >
            <textarea
              :id="ids.when"
              v-model="form.condition"
              class="textarea"
              rows="2"
              spellcheck="false"
              placeholder="OLD.* IS DISTINCT FROM NEW.*"
            />
          </FormField>
          <div class="grid">
            <FormField
              label="Function"
              :input-id="ids.fn"
              required
              :error="errors.function ?? ''"
            >
              <input
                :id="ids.fn"
                v-model="form.function"
                class="input mono"
                list="pg-trigger-functions"
                spellcheck="false"
                :aria-invalid="!!errors.function"
              >
              <datalist id="pg-trigger-functions">
                <option
                  v-for="f in fnList"
                  :key="`${f.object.schema}.${f.object.name}`"
                  :value="`${f.object.schema}.${f.object.name}`"
                />
              </datalist>
            </FormField>
            <FormField
              label="Arguments"
              :input-id="ids.args"
              help="Comma separated; passed as text (TG_ARGV)."
            >
              <input
                :id="ids.args"
                v-model="argsText"
                class="input mono"
                placeholder="none"
                spellcheck="false"
              >
            </FormField>
          </div>
          <details class="advanced">
            <summary>Constraint trigger and transition tables</summary>
            <label class="check"><input
              v-model="form.constraint"
              type="checkbox"
            ><span>Constraint trigger (AFTER ... FOR EACH ROW)</span></label>
            <span
              v-if="errors.constraint"
              class="field-error"
            >{{ errors.constraint }}</span>
            <label class="check"><input
              v-model="form.deferrable"
              type="checkbox"
              :disabled="!form.constraint"
            ><span>DEFERRABLE</span></label>
            <label class="check"><input
              v-model="form.initiallyDeferred"
              type="checkbox"
              :disabled="!form.deferrable"
            ><span>INITIALLY DEFERRED</span></label>
            <span
              v-if="errors.deferrable || errors.initiallyDeferred"
              class="field-error"
            >{{ errors.deferrable ?? errors.initiallyDeferred }}</span>
            <div class="grid">
              <FormField
                label="OLD TABLE AS"
                :input-id="ids.oldTable"
                :error="errors.oldTable ?? ''"
              >
                <input
                  :id="ids.oldTable"
                  v-model="form.oldTable"
                  class="input mono"
                >
              </FormField>
              <FormField
                label="NEW TABLE AS"
                :input-id="ids.newTable"
                :error="errors.newTable ?? ''"
              >
                <input
                  :id="ids.newTable"
                  v-model="form.newTable"
                  class="input mono"
                >
              </FormField>
            </div>
          </details>
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
            >
              <input
                v-model="form.state"
                type="radio"
                :value="s.value"
              ><span>{{ s.label }}<span class="sub">{{ s.help }}</span></span>
            </label>
          </div>
          <FormField
            label="Comment"
            :input-id="ids.comment"
          >
            <input
              :id="ids.comment"
              v-model="form.comment"
              class="input"
            >
          </FormField>
          <p
            v-if="editor.current.value && !pgAtLeast(server, 14)"
            class="hint"
          >
            PostgreSQL {{ server.major }} has no CREATE OR REPLACE TRIGGER: a changed definition drops and creates the trigger.
          </p>
        </section>

        <div class="column">
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
          <section
            v-if="fnPreview"
            class="section"
            aria-label="Function preview"
          >
            <div class="section-head">
              <h2 class="mono">
                {{ fnPreview.name }}()
              </h2>
              <span class="grow" />
              <button
                type="button"
                class="btn compact ghost"
                @click="emit('openObject', objectRef('function', database, fnPreview.schema, fnPreview.name, { signature: fnPreview.signature }))"
              >
                Open function
              </button>
            </div>
            <span class="hint">returns {{ fnPreview.result ?? fnPreview.returnType ?? '' }} · {{ fnPreview.language }}</span>
            <div class="code-box">
              <SqlCode
                :model-value="fnPreview.source ?? fnPreview.definition"
                engine="postgres"
                :label="`Source of ${fnPreview.name}`"
                readonly
              />
            </div>
          </section>
        </div>
      </div>
    </template>

    <template #ddl>
      <DdlView
        v-if="editor.current.value && details"
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
.trigger-layout {
  display: grid;
  grid-template-columns: 260px minmax(0, 1fr) 360px;
  gap: 16px;
  padding: 16px;
  align-items: start;
}

.row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
}

.events {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 0;
  padding: 12px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
}

.col-input {
  width: 160px;
  height: 26px;
}

.stacked {
  flex-direction: column;
  align-items: flex-start;
  gap: 2px;
}

.advanced {
  display: flex;
  flex-direction: column;
  gap: 8px;
  font-size: 13px;
}

.advanced summary {
  cursor: pointer;
  color: var(--text-muted);
}

.states {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  align-items: center;
}

@media (max-width: 1280px) {
  .trigger-layout {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
