<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { PgSequence } from '../../api/ddl';
import {
  buildPgSequence,
  buildPgSequenceRestart,
  buildPgSequenceSetValue,
  bigIntegerProblem,
  errorsOf,
  integerText,
  objectRef,
  parseBigInteger,
  pgSequenceFormFrom,
  pgSequenceGoesBack,
  pgSequenceState,
  pgSequenceSchema,
} from '../../lib/ddlPlan';
import type { PgSequenceForm } from '../../lib/ddlPlan';
import { formatCount } from '../../lib/values';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import PlanPreview from './PlanPreview.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';
import { useObjectEditor } from './useObjectEditor';

/** PostgreSQL sequence editor (design canvas SequenceEditor). Use `SequenceEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const schemaDefault = computed(() => props.container?.schema ?? 'public');
const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);

const editor = useObjectEditor<PgSequence, PgSequenceForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'sequence' ? d.object : 'This object is not a PostgreSQL sequence.'),
  formFrom: (seq) => pgSequenceFormFrom(seq, schemaDefault.value),
  validate: (form) => errorsOf(pgSequenceSchema, form),
  build: (form, seq) => buildPgSequence(form, seq, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('sequence', database.value, form.schema.trim(), form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('general');
const tabs = computed(() => [{ id: 'general', label: 'General' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : 'New sequence'));
const blocked = computed(() => Object.values(errors.value)[0] ?? null);
const ids = { type: useId(), inc: useId(), start: useId(), cache: useId(), min: useId(), max: useId(), owned: useId(), log: useId(), reset: useId() };

// Values arrive as exact decimal text; BigInt keeps them exact.
const state = computed(() => (details.value ? pgSequenceState(details.value) : { current: null, next: null }));
const rangeUsed = computed(() => {
  const s = details.value;
  const current = state.value.current;
  if (!s || current === null) return null;
  const span = BigInt(s.max) - BigInt(s.min);
  if (span <= 0n) return null;
  const done = BigInt(s.increment) > 0n ? BigInt(current) - BigInt(s.min) : BigInt(s.max) - BigInt(current);
  const used = Number(done) / Number(span);
  return used < 0.0001 ? '< 0.01%' : `${(used * 100).toFixed(2)}%`;
});
const cacheWarning = computed(() => {
  const cache = parseBigInteger(form.cache);
  return cache !== null && cache > 1n && details.value !== null && details.value.cache === '1'
    ? `With cache ${cache.toString()} each session preallocates ${cache.toString()} numbers: values are no longer gapless or in commit order across sessions.`
    : null;
});

// Reset (setval / restart).
const resetPlan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => emit('changed'),
  reportError: (error) => props.reportError?.(error),
});
const resetKind = ref<'setval' | 'restart'>('setval');
const resetValue = ref('');
const nextReturnsIt = ref(true);
const resetError = computed(() => bigIntegerProblem(resetValue.value, resetKind.value === 'restart'));
const resetWarning = computed(() => {
  const s = details.value;
  if (!s) return null;
  const typed = parseBigInteger(resetValue.value);
  // What the next nextval returns after the reset.
  const next =
    resetKind.value === 'restart' ? (typed ?? BigInt(s.start)) : typed === null ? null : nextReturnsIt.value ? typed : typed + BigInt(s.increment);
  if (next === null || !pgSequenceGoesBack(s, next)) return null;
  return `The next value would be ${formatCount(next.toString())}, not past ${formatCount(state.value.next ?? '')}: values handed out before may be handed out again, and inserts into unique columns can fail.`;
});

async function planReset(): Promise<void> {
  const s = details.value;
  if (!s || resetError.value) return;
  const v = integerText(resetValue.value);
  const change =
    resetKind.value === 'restart' ? buildPgSequenceRestart(s, v) : v === null ? null : buildPgSequenceSetValue(s, v, !nextReturnsIt.value);
  await resetPlan.plan(change ? [{ kind: 'edit', change }] : []);
}

async function afterReset(): Promise<void> {
  resetPlan.clear();
  resetValue.value = '';
  await editor.load();
}

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Sequence"
    :environment="session.environment"
    :subtitle="details ? `owner ${details.owner}` : ''"
    :tabs="tabs"
    :read-only="session.readOnly"
    :dirty-text="editor.dirty.value ? 'Unsaved changes' : ''"
    :loading="editor.loading.value"
    :load-error="editor.loadError.value"
    @reload="editor.load"
  >
    <template #badges>
      <span
        v-if="details?.identity"
        class="pill tone-neutral"
      >identity of a column</span>
    </template>
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

    <template #general>
      <div class="layout">
        <div class="column">
          <section
            v-if="details"
            class="section stats"
            aria-label="Current state"
          >
            <div class="stat">
              <span class="label">Last value handed out</span>
              <span class="value mono">{{ state.current !== null ? formatCount(state.current) : state.next !== null ? 'none since it was created or reset' : 'unknown' }}</span>
            </div>
            <div class="stat">
              <span class="label">Next nextval()</span>
              <span class="value mono">{{ state.next === null ? 'unknown' : formatCount(state.next) }}</span>
              <span class="hint">sessions with cached values may differ</span>
            </div>
            <div
              v-if="rangeUsed"
              class="stat"
            >
              <span class="label">Range used</span>
              <span class="value mono">{{ rangeUsed }}</span>
            </div>
          </section>
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
            <div class="grid">
              <FormField
                label="Data type"
                :input-id="ids.type"
              >
                <select
                  :id="ids.type"
                  v-model="form.dataType"
                  class="select"
                >
                  <option value="bigint">
                    bigint
                  </option>
                  <option value="integer">
                    integer
                  </option>
                  <option value="smallint">
                    smallint
                  </option>
                </select>
              </FormField>
              <FormField
                label="Increment"
                :input-id="ids.inc"
                :error="errors.increment ?? ''"
              >
                <input
                  :id="ids.inc"
                  v-model="form.increment"
                  class="input mono"
                  inputmode="numeric"
                >
              </FormField>
              <FormField
                label="Start"
                :input-id="ids.start"
                :error="errors.start ?? ''"
              >
                <input
                  :id="ids.start"
                  v-model="form.start"
                  class="input mono"
                  inputmode="numeric"
                  placeholder="default"
                >
              </FormField>
              <FormField
                label="Cache"
                :input-id="ids.cache"
                :error="errors.cache ?? ''"
              >
                <input
                  :id="ids.cache"
                  v-model="form.cache"
                  class="input mono"
                  inputmode="numeric"
                >
              </FormField>
              <FormField
                label="Minimum"
                :input-id="ids.min"
                :error="errors.min ?? ''"
              >
                <input
                  :id="ids.min"
                  v-model="form.min"
                  class="input mono"
                  inputmode="numeric"
                  placeholder="type default"
                >
              </FormField>
              <FormField
                label="Maximum"
                :input-id="ids.max"
                :error="errors.max ?? ''"
              >
                <input
                  :id="ids.max"
                  v-model="form.max"
                  class="input mono"
                  inputmode="numeric"
                  placeholder="type default"
                >
              </FormField>
              <label class="check">
                <input
                  v-model="form.cycle"
                  type="checkbox"
                ><span>Cycle</span>
              </label>
              <FormField
                label="Owned by"
                :input-id="ids.owned"
                :error="errors.ownedBy ?? ''"
                help="table.column; blank for none"
              >
                <input
                  :id="ids.owned"
                  v-model="form.ownedBy"
                  class="input mono"
                  spellcheck="false"
                >
              </FormField>
              <FormField
                label="Persistence"
                :input-id="ids.log"
              >
                <select
                  :id="ids.log"
                  v-model="form.logged"
                  class="select"
                >
                  <option :value="true">
                    Logged
                  </option>
                  <option :value="false">
                    Unlogged
                  </option>
                </select>
              </FormField>
            </div>
            <p
              v-if="cacheWarning"
              class="notice tone-warning"
              role="note"
            >
              <Icon name="warning" />{{ cacheWarning }}
            </p>
            <p
              v-if="form.ownedBy.trim() !== ''"
              class="hint"
            >
              Owned by: dropping the column drops this sequence.
            </p>
          </section>
        </div>
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
            v-if="details"
            class="section"
            aria-label="Reset to"
          >
            <h2>Reset</h2>
            <div
              role="radiogroup"
              aria-label="How to reset"
              class="segmented"
            >
              <button
                type="button"
                role="radio"
                :aria-checked="resetKind === 'setval'"
                @click="resetKind = 'setval'"
              >
                setval
              </button>
              <button
                type="button"
                role="radio"
                :aria-checked="resetKind === 'restart'"
                @click="resetKind = 'restart'"
              >
                RESTART
              </button>
            </div>
            <FormField
              :label="resetKind === 'restart' ? 'Restart with (blank: start value)' : 'New value'"
              :input-id="ids.reset"
              :error="resetValue.trim() !== '' || resetKind === 'setval' ? (resetError ?? '') : ''"
            >
              <input
                :id="ids.reset"
                v-model="resetValue"
                class="input mono"
                inputmode="numeric"
              >
            </FormField>
            <label
              v-if="resetKind === 'setval'"
              class="check"
            >
              <input
                v-model="nextReturnsIt"
                type="checkbox"
              ><span>Next call returns it (is_called = false)</span>
            </label>
            <p
              v-if="resetWarning"
              class="notice tone-warning"
              role="alert"
            >
              <Icon name="warning" />{{ resetWarning }}
            </p>
            <div>
              <button
                type="button"
                class="btn compact"
                :disabled="session.readOnly || resetError !== null"
                @click="planReset"
              >
                Preview reset
              </button>
            </div>
            <PlanPreview
              v-if="resetPlan.state.steps.length > 0 || resetPlan.state.planProblem"
              :plan="resetPlan"
              engine="postgres"
              :read-only="session.readOnly"
              apply-label="Reset sequence"
              @applied="afterReset"
              @reconnect="emit('reconnect')"
            />
          </section>
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
.stats {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
}
</style>
