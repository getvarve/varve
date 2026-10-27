<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import { catalogDescribe } from '../../api/ddl';
import type { PgPolicy, QualifiedName } from '../../api/ddl';
import { objectRef } from '../../lib/ddlPlan';
import { buildPgPolicy, policyNeedsRecreate, pgPolicyErrors, pgPolicyFormFrom } from '../../lib/ddlRls';
import type { PgPolicyForm, TableRls } from '../../lib/ddlRls';
import FormField from '../common/FormField.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/**
 * PostgreSQL row level security policy editor (design doc 4.2, "Rules,
 * policies (RLS): enable / force RLS per table"): command, permissive or
 * restrictive, roles, USING and WITH CHECK, comment and name, plus row
 * level security of the table (ENABLE, FORCE). `ALTER POLICY` where it can;
 * a changed command or permissiveness drops and creates the policy. The
 * table is `object.parent`, or `container.parent` for a new policy. Props
 * and events: `ObjectEditorProps` / `ObjectEditorEmits`.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const table = computed<QualifiedName>(() => ({
  schema: props.object?.schema ?? props.container?.schema ?? 'public',
  name: props.object?.parent ?? props.container?.parent ?? '',
}));
/** Row level security of the table; `null` until read (or when it could not be read). */
const rls = ref<TableRls | null>(null);

async function readTableRls(): Promise<void> {
  rls.value = null;
  if (table.value.name === '') return;
  let lastError: unknown = null;
  // The policy's table is a table or a partitioned table; the reference does not say which.
  for (const kind of ['table', 'partitionedTable'] as const) {
    try {
      const d = await catalogDescribe(props.session.id, objectRef(kind, database.value, table.value.schema, table.value.name));
      if (d.engine === 'postgres' && (d.object.kind === 'table' || d.object.kind === 'partitionedTable')) {
        rls.value = { enabled: d.object.rlsEnabled, forced: d.object.rlsForced };
        return;
      }
    } catch (error: unknown) {
      lastError = error;
    }
  }
  if (lastError !== null) throw lastError;
}

const editor = useObjectEditor<PgPolicy, PgPolicyForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'policy' ? d.object : 'This object is not a policy.'),
  afterLoad: () => readTableRls(),
  formFrom: (policy) => pgPolicyFormFrom(policy, rls.value),
  validate: (form) => pgPolicyErrors(form),
  blocked: () => (table.value.name === '' ? 'Open the editor from a table: a policy belongs to one.' : null),
  build: (form, policy) => buildPgPolicy(form, policy, rls.value, table.value, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('policy', database.value, table.value.schema, form.name.trim(), { parent: table.value.name }),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Policy' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? editor.current.value.name : 'New policy'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const tableName = computed(() => `${table.value.schema ?? ''}.${table.value.name}`);
const recreate = computed(() => (details.value ? policyNeedsRecreate(form as PgPolicyForm, details.value) : null));
const usesUsing = computed(() => form.command !== 'insert');
const usesCheck = computed(() => form.command !== 'select' && form.command !== 'delete');
const ids = { name: useId(), command: useId(), roles: useId(), using: useId(), check: useId(), comment: useId() };
const COMMANDS = [
  { value: 'all', label: 'ALL' },
  { value: 'select', label: 'SELECT' },
  { value: 'insert', label: 'INSERT' },
  { value: 'update', label: 'UPDATE' },
  { value: 'delete', label: 'DELETE' },
] as const;

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Policy"
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
        <div class="column">
          <section
            class="section"
            aria-label="Policy definition"
          >
            <p class="hint">
              A policy decides which rows of {{ tableName }} a role sees (USING) and may write (WITH CHECK). Permissive policies are combined with OR, restrictive ones with AND on top.
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
                label="Command"
                :input-id="ids.command"
              >
                <select
                  :id="ids.command"
                  v-model="form.command"
                  class="select mono"
                >
                  <option
                    v-for="c in COMMANDS"
                    :key="c.value"
                    :value="c.value"
                  >
                    {{ c.label }}
                  </option>
                </select>
              </FormField>
              <FormField
                label="Roles"
                :input-id="ids.roles"
                :error="errors.roles ?? ''"
                help="Comma separated role names; blank means PUBLIC (every role)."
              >
                <input
                  :id="ids.roles"
                  v-model="form.roles"
                  class="input mono"
                  autocomplete="off"
                  spellcheck="false"
                >
              </FormField>
            </div>
            <div
              role="radiogroup"
              aria-label="Combination"
              class="states"
            >
              <label class="check"><input
                v-model="form.permissive"
                type="radio"
                :value="true"
              ><span>Permissive</span></label>
              <label class="check"><input
                v-model="form.permissive"
                type="radio"
                :value="false"
              ><span>Restrictive</span></label>
            </div>
            <FormField
              v-if="usesUsing"
              label="USING"
              :input-id="ids.using"
              :error="errors.using ?? ''"
              help="Rows for which this is true are visible (and can be updated or deleted). Blank on a new policy: none for this command."
            >
              <textarea
                :id="ids.using"
                v-model="form.using"
                class="textarea mono"
                rows="3"
                spellcheck="false"
              />
            </FormField>
            <FormField
              v-if="usesCheck"
              label="WITH CHECK"
              :input-id="ids.check"
              :error="errors.withCheck ?? ''"
              help="New or changed rows must make this true. Blank: USING applies."
            >
              <textarea
                :id="ids.check"
                v-model="form.withCheck"
                class="textarea mono"
                rows="3"
                spellcheck="false"
              />
            </FormField>
            <p
              v-if="!usesUsing && form.using.trim() !== ''"
              class="notice tone-warning"
            >
              {{ errors.using }}
            </p>
            <p
              v-if="!usesCheck && form.withCheck.trim() !== ''"
              class="notice tone-warning"
            >
              {{ errors.withCheck }}
            </p>
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
              v-if="recreate && editor.dirty.value"
              class="notice tone-warning"
            >
              {{ recreate }}
            </p>
          </section>
          <section
            class="section"
            aria-label="Row level security of the table"
          >
            <h2>Row level security on {{ tableName }}</h2>
            <p
              v-if="!form.rlsEnabled"
              class="notice tone-neutral"
            >
              Row level security is off on {{ tableName }}: this policy has no effect until it is enabled. Enabling it is a separate choice: from then on roles see only the rows some policy allows them.
            </p>
            <p
              v-if="rls === null"
              class="hint"
            >
              The table's row level security could not be read; it is left as it is.
            </p>
            <label class="check">
              <input
                v-model="form.rlsEnabled"
                type="checkbox"
                :disabled="rls === null && !editor.creating.value"
              ><span>Enabled<span class="sub">Policies only apply while row level security is enabled. Without any policy, enabled means no rows for anyone but the owner.</span></span>
            </label>
            <label class="check">
              <input
                v-model="form.rlsForced"
                type="checkbox"
                :disabled="rls === null && !editor.creating.value"
              ><span>Force for the owner<span class="sub">The table owner is held to the policies too (superusers and BYPASSRLS roles never are).</span></span>
            </label>
            <p
              v-if="errors.rlsForced"
              class="notice tone-warning"
            >
              {{ errors.rlsForced }}
            </p>
          </section>
        </div>
        <ChangePanel
          v-model:cascade="editor.cascade.value"
          :plan="editor.plan"
          engine="postgres"
          :read-only="session.readOnly"
          :mode="editor.mode.value"
          :creating="editor.creating.value"
          :object-name="title"
          :blocked="editor.dirty.value ? blocked : null"
          :cascade-available="false"
          @applied="editor.onApplied"
          @cancel-drop="editor.cancelDrop"
          @reconnect="emit('reconnect')"
          @refresh="editor.load"
        >
          <template #drop>
            <p class="notice tone-neutral">
              Dropping the policy takes its rows away from the roles it granted them to; row level security of {{ tableName }} stays as it is.
            </p>
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
</style>
