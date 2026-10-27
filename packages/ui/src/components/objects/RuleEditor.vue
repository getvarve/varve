<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { PgRule, QualifiedName } from '../../api/ddl';
import { objectRef } from '../../lib/ddlPlan';
import { buildPgRule, parseRuleDefinition, pgRuleErrors, pgRuleFormFrom } from '../../lib/ddlRls';
import type { PgRuleForm } from '../../lib/ddlRls';
import FormField from '../common/FormField.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import DropImpactPanel from './DropImpactPanel.vue';
import EditorFrame from './EditorFrame.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/**
 * PostgreSQL rewrite rule editor (design doc 4.2, "Rules, policies"):
 * event, condition, DO INSTEAD or ALSO, the commands (or NOTHING), firing
 * state, comment and name. A changed definition is saved with
 * `CREATE OR REPLACE RULE`. Rules belong to a table or view: `object.parent`
 * names it, or `container.parent` for a new rule. Props and events:
 * `ObjectEditorProps` / `ObjectEditorEmits`.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const table = computed<QualifiedName>(() => ({
  schema: props.object?.schema ?? props.container?.schema ?? 'public',
  name: props.object?.parent ?? props.container?.parent ?? '',
}));

const editor = useObjectEditor<PgRule, PgRuleForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'rule' ? d.object : 'This object is not a rule.'),
  formFrom: (rule) => pgRuleFormFrom(rule),
  validate: (form) => pgRuleErrors(form),
  blocked: (_form, rule) => {
    if (table.value.name === '') return 'Open the editor from a table or view: a rule belongs to one.';
    if (rule && parseRuleDefinition(rule.definition) === null) return 'The rule definition could not be read here; edit it on the DDL tab.';
    if (rule?.name === '_RETURN') return '_RETURN is the rule that makes a view; edit the view instead.';
    return null;
  },
  build: (form, rule) => buildPgRule(form, rule ? { rule, parts: parseRuleDefinition(rule.definition) } : null, table.value, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('rule', database.value, table.value.schema, form.name.trim(), { parent: table.value.name }),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Rule' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? editor.current.value.name : 'New rule'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const tableName = computed(() => `${table.value.schema ?? ''}.${table.value.name}`);
const ids = { name: useId(), event: useId(), condition: useId(), actions: useId(), comment: useId() };
const EVENTS = [
  { value: 'insert', label: 'INSERT' },
  { value: 'update', label: 'UPDATE' },
  { value: 'delete', label: 'DELETE' },
  { value: 'select', label: 'SELECT' },
] as const;
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
    kind-label="Rule"
    :environment="session.environment"
    :subtitle="`on ${tableName}`"
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
          aria-label="Rule definition"
        >
          <p class="hint">
            A rule rewrites every {{ form.event.toUpperCase() }} on {{ tableName }} before it runs: DO INSTEAD replaces the command, DO ALSO runs the commands as well. In the condition and commands, NEW and OLD are the row. Triggers are usually the better tool; rules suit views and redirects.
          </p>
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
                autocomplete="off"
                spellcheck="false"
                :aria-invalid="!!errors.name"
              >
            </FormField>
            <FormField
              label="Event"
              :input-id="ids.event"
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
              label="Condition"
              :input-id="ids.condition"
              class="wide"
              :error="errors.condition ?? ''"
              help="WHERE: blank applies the rule to every row."
            >
              <textarea
                :id="ids.condition"
                v-model="form.condition"
                class="textarea mono"
                rows="2"
                spellcheck="false"
              />
            </FormField>
          </div>
          <div
            role="radiogroup"
            aria-label="Rule action"
            class="states"
          >
            <label class="check"><input
              v-model="form.instead"
              type="radio"
              :value="true"
            ><span>DO INSTEAD</span></label>
            <label class="check"><input
              v-model="form.instead"
              type="radio"
              :value="false"
            ><span>DO ALSO</span></label>
            <span
              v-if="errors.instead"
              class="error"
            >{{ errors.instead }}</span>
          </div>
          <FormField
            label="Commands"
            :input-id="ids.actions"
            :error="errors.actions ?? ''"
            help="One or more INSERT, UPDATE, DELETE, SELECT or NOTIFY commands separated by semicolons (NOTIFY only without a condition). Blank means NOTHING."
          >
            <textarea
              :id="ids.actions"
              v-model="form.actions"
              class="textarea mono"
              rows="6"
              spellcheck="false"
              :aria-invalid="!!errors.actions"
            />
          </FormField>
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
          <FormField
            label="Comment"
            :input-id="ids.comment"
          >
            <textarea
              :id="ids.comment"
              v-model="form.comment"
              class="textarea"
              rows="2"
            />
          </FormField>
          <p
            v-if="details"
            class="hint mono definition"
          >
            {{ details.definition }}
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
<style scoped>
.states {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  align-items: center;
}

.error {
  color: var(--danger);
  font-size: 12px;
}

.definition {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}
</style>
