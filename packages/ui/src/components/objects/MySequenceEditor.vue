<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { MySequence } from '../../api/ddl';
import {
  buildMySequence,
  buildMySequenceRestart,
  buildMySequenceSetValue,
  bigIntegerProblem,
  errorsOf,
  INT64_MIN,
  integerText,
  mySequenceFormFrom,
  mySequenceSchema,
  objectRef,
  UINT64_MAX,
} from '../../lib/ddlPlan';
import type { MySequenceForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import PlanPreview from './PlanPreview.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';
import { useObjectEditor } from './useObjectEditor';

/** MariaDB sequence editor (design canvas SequenceEditor). Use `SequenceEditor.vue`. */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database ?? '');

const editor = useObjectEditor<MySequence, MySequenceForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'mysql' && d.object.kind === 'sequence' ? d.object : 'This object is not a MariaDB sequence.'),
  formFrom: (seq) => mySequenceFormFrom(seq),
  validate: (form) => errorsOf(mySequenceSchema, form),
  blocked: (form, seq) => (database.value === '' ? 'Choose a database first.' : seq && form.name.trim() !== seq.name ? 'Sequences are renamed with RENAME TABLE, which this editor does not run. Use the console.' : null),
  build: (form, seq) => buildMySequence(form, seq, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('sequence', database.value, null, form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('general');
const tabs = computed(() => [{ id: 'general', label: 'General' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'mysql') : 'New sequence'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const ids = { name: useId(), type: useId(), inc: useId(), start: useId(), cache: useId(), min: useId(), max: useId(), comment: useId(), reset: useId() };

const resetPlan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => emit('changed'),
  reportError: (error) => props.reportError?.(error),
});
const resetKind = ref<'setval' | 'restart'>('setval');
const resetValue = ref('');
const isUsed = ref(false);
// MariaDB sequence values range from the signed to the unsigned 64-bit limit; sent as exact text.
const resetError = computed(() => bigIntegerProblem(resetValue.value, resetKind.value === 'restart', INT64_MIN, UINT64_MAX));

async function planReset(): Promise<void> {
  const s = details.value;
  if (!s || resetError.value) return;
  const v = integerText(resetValue.value, INT64_MIN, UINT64_MAX);
  const change = resetKind.value === 'restart' ? buildMySequenceRestart(s, v) : v === null ? null : buildMySequenceSetValue(s, v, isUsed.value);
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

    <template #general>
      <div class="layout">
        <div class="column">
          <section
            v-if="details"
            class="section stats"
            aria-label="Current state"
          >
            <div class="stat">
              <span class="label">Next value</span><span class="value mono">{{ details.nextValue }}</span>
            </div>
            <div class="stat">
              <span class="label">Last value (this session)</span><span class="value mono">{{ details.lastValue ?? 'none' }}</span>
            </div>
            <div class="stat">
              <span class="label">Cycles</span><span class="value mono">{{ details.cycleCount }}</span>
            </div>
          </section>
          <section
            class="section"
            aria-label="Properties"
          >
            <h2>Properties</h2>
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
                  :disabled="details !== null"
                >
              </FormField>
              <FormField
                label="Data type"
                :input-id="ids.type"
              >
                <div class="type">
                  <select
                    :id="ids.type"
                    v-model="form.dataType"
                    class="select"
                  >
                    <option value="">
                      Default (bigint)
                    </option>
                    <option
                      v-for="t in ['tinyint', 'smallint', 'mediumint', 'int', 'bigint']"
                      :key="t"
                      :value="t"
                    >
                      {{ t }}
                    </option>
                  </select>
                  <label class="check"><input
                    v-model="form.unsigned"
                    type="checkbox"
                    :disabled="form.dataType === ''"
                  >unsigned</label>
                </div>
              </FormField>
              <FormField
                label="Increment"
                :input-id="ids.inc"
                :error="errors.increment ?? ''"
                help="0 uses auto_increment_increment."
              >
                <input
                  :id="ids.inc"
                  v-model="form.increment"
                  class="input mono"
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
                  placeholder="NO MINVALUE"
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
                  placeholder="NO MAXVALUE"
                >
              </FormField>
              <label class="check"><input
                v-model="form.cycle"
                type="checkbox"
              ><span>Cycle</span></label>
              <FormField
                v-if="!details"
                label="Comment"
                :input-id="ids.comment"
                class="wide"
              >
                <input
                  :id="ids.comment"
                  v-model="form.comment"
                  class="input"
                >
              </FormField>
            </div>
          </section>
        </div>
        <div class="column">
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
          <section
            v-if="details"
            class="section"
            aria-label="Reset"
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
                SETVAL
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
              :label="resetKind === 'restart' ? 'Restart with (blank: start value)' : 'Value'"
              :input-id="ids.reset"
              :error="resetValue.trim() !== '' || resetKind === 'setval' ? (resetError ?? '') : ''"
            >
              <input
                :id="ids.reset"
                v-model="resetValue"
                class="input mono"
              >
            </FormField>
            <label
              v-if="resetKind === 'setval'"
              class="check"
            ><input
              v-model="isUsed"
              type="checkbox"
            ><span>The value counts as used (the next value comes after it)</span></label>
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
              :engine="session.engine"
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
  grid-template-columns: repeat(auto-fill, minmax(160px, 1fr));
}

.type {
  display: flex;
  gap: 8px;
  align-items: center;
}
</style>
