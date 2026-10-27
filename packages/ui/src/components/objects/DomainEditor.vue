<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { PgDomain } from '../../api/ddl';
import { buildPgDomain, buildPgDomainValidate, domainProblem, errorsOf, objectRef, pgDomainFormFrom, pgDomainSchema } from '../../lib/ddlPlan';
import type { PgDomainForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import PlanPreview from './PlanPreview.vue';
import TypeList from './TypeList.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';
import { useObjectEditor } from './useObjectEditor';

/** PostgreSQL domain editor (design canvas TypeEditor). Rendered by `TypeEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);
const schemaDefault = computed(() => props.container?.schema ?? 'public');

const editor = useObjectEditor<PgDomain, PgDomainForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'domain' ? d.object : 'This object is not a domain.'),
  formFrom: (domain) => pgDomainFormFrom(domain, schemaDefault.value),
  validate: (form) => errorsOf(pgDomainSchema, form),
  blocked: (form, domain) => domainProblem(form, domain),
  build: (form, domain) => buildPgDomain(form, domain, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('domain', database.value, form.schema.trim(), form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Definition' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const revision = ref(0);
const schema = computed(() => editor.current.value?.schema ?? schemaDefault.value);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : 'New domain'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const ids = { type: useId(), collation: useId(), def: useId() };

const validatePlan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => emit('changed'),
  reportError: (error) => props.reportError?.(error),
});

async function planValidate(name: string): Promise<void> {
  if (!details.value) return;
  await validatePlan.plan([{ kind: 'edit', change: buildPgDomainValidate(details.value, name) }]);
}

async function validated(): Promise<void> {
  validatePlan.clear();
  await editor.load();
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
    kind-label="Domain"
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
          aria-label="Domain definition"
        >
          <div class="grid">
            <FormField
              label="Base type"
              :input-id="ids.type"
              required
              :error="errors.dataType ?? ''"
            >
              <input
                :id="ids.type"
                v-model="form.dataType"
                class="input mono"
                :disabled="details !== null"
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
                :disabled="details !== null"
                placeholder="default"
              >
            </FormField>
            <FormField
              label="Default"
              :input-id="ids.def"
              class="wide"
            >
              <input
                :id="ids.def"
                v-model="form.default"
                class="input mono"
                placeholder="none"
              >
            </FormField>
            <label class="check"><input
              v-model="form.notNull"
              type="checkbox"
            ><span>NOT NULL</span></label>
          </div>
          <h2>Checks</h2>
          <ul class="list">
            <li
              v-for="(c, i) in form.checks"
              :key="i"
              class="check-row"
            >
              <input
                v-model="c.name"
                class="input mono name"
                :aria-label="`Name of check ${i + 1}`"
                placeholder="generated name"
                :aria-invalid="!!errors[`checks.${i}.name`]"
              >
              <span class="mono subtle">CHECK (</span>
              <input
                v-model="c.expression"
                class="input mono"
                :aria-label="`Expression of check ${i + 1}`"
                placeholder="VALUE >= 0"
                :aria-invalid="!!errors[`checks.${i}.expression`]"
              >
              <span class="mono subtle">)</span>
              <button
                v-if="c.original !== null && !c.validated"
                type="button"
                class="btn compact"
                :disabled="session.readOnly"
                @click="planValidate(c.original)"
              >
                Validate
              </button>
              <button
                type="button"
                class="icon-btn"
                :aria-label="`Remove check ${c.name || i + 1}`"
                @click="form.checks.splice(i, 1)"
              >
                <Icon name="close" />
              </button>
              <input
                v-model="c.comment"
                class="input comment"
                :aria-label="`Comment of check ${c.name || i + 1}`"
                placeholder="comment"
              >
              <span
                v-if="errors[`checks.${i}.expression`]"
                class="field-error"
              >{{ errors[`checks.${i}.expression`] }}</span>
              <span
                v-if="errors[`checks.${i}.name`]"
                class="field-error"
              >{{ errors[`checks.${i}.name`] }}</span>
            </li>
          </ul>
          <div>
            <button
              type="button"
              class="btn compact"
              @click="form.checks.push({ name: '', expression: '', original: null, validated: true, comment: '' })"
            >
              <Icon name="plus" />Add check
            </button>
          </div>
          <label
            v-if="details"
            class="check"
          ><input
            v-model="form.notValid"
            type="checkbox"
          ><span>Add new checks NOT VALID<span class="sub">Existing columns are not checked now; validate later.</span></span></label>
          <p
            v-if="details"
            class="hint"
          >
            A changed check is dropped and added again. The base type and collation of an existing domain cannot change.
          </p>
          <PlanPreview
            v-if="validatePlan.state.steps.length > 0 || validatePlan.state.planProblem"
            :plan="validatePlan"
            engine="postgres"
            :read-only="session.readOnly"
            apply-label="Validate"
            @applied="validated"
            @reconnect="emit('reconnect')"
          />
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

.check-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
}

.check-row .input {
  flex: 1 1 180px;
  height: 28px;
}

.check-row .name {
  flex: 0 1 160px;
}

.check-row .comment {
  flex: 1 1 100%;
}

@media (max-width: 1280px) {
  .type-layout {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
