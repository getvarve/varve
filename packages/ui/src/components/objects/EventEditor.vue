<script setup lang="ts">
import { computed, onMounted, ref, useId } from 'vue';
import type { MyEvent } from '../../api/ddl';
import { INTERVAL_UNITS, buildMyEvent, errorsOf, myEventFormFrom, myEventSchema, nextRuns, objectRef } from '../../lib/ddlPlan';
import type { MyEventForm } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import DefinerFields from './DefinerFields.vue';
import EditorFrame from './EditorFrame.vue';
import MySessionNote from './MySessionNote.vue';
import SqlCode from './SqlCode.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useObjectEditor } from './useObjectEditor';

/**
 * MySQL and MariaDB scheduled event editor (design canvas EventEditor):
 * schedule (AT or EVERY with STARTS / ENDS), on completion, status, body,
 * scheduler state and the next run times. Saving an existing event is one
 * ALTER EVENT with only what changed. Props and events:
 * `ObjectEditorProps` / `ObjectEditorEmits`. Opens for an `event` node.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database ?? '');

const editor = useObjectEditor<MyEvent, MyEventForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'mysql' && d.object.kind === 'event' ? d.object : 'This object is not a scheduled event.'),
  formFrom: (event) => myEventFormFrom(event),
  validate: (form) => errorsOf(myEventSchema, form),
  blocked: () => (database.value === '' ? 'Choose a database first.' : null),
  build: (form, event) => buildMyEvent(form, event, database.value).map((change) => ({ kind: 'edit', change })),
  identity: (form) => objectRef('event', database.value, null, form.name.trim()),
});
const { form, errors, details } = editor;

const tab = ref('definition');
const tabs = computed(() => [{ id: 'definition', label: 'Definition' }, ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : [])]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'mysql') : 'New event'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const ids = { name: useId(), comment: useId(), at: useId(), plusQ: useId(), every: useId(), starts: useId(), ends: useId() };
const runs = computed(() => nextRuns(form, new Date(), 5));
const statusLabel = computed(() => {
  switch (details.value?.status) {
    case 'enabled':
      return 'Enabled';
    case 'disabled':
      return 'Disabled';
    case 'slavesideDisabled':
      return 'Disabled on replica';
    default:
      return '';
  }
});
const scheduler = computed(() => details.value?.scheduler ?? null);
const runFormat = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'medium' });

onMounted(() => void editor.load());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Event"
    :environment="session.environment"
    :subtitle="details?.definer ? `definer ${details.definer}` : ''"
    :tabs="tabs"
    :read-only="session.readOnly"
    :dirty-text="editor.dirty.value ? 'Unsaved changes' : ''"
    :loading="editor.loading.value"
    :load-error="editor.loadError.value"
    @reload="editor.load"
  >
    <template #badges>
      <span
        v-if="statusLabel"
        :class="['pill', details?.status === 'enabled' ? 'tone-success' : 'tone-neutral']"
      ><span class="dot" />{{ statusLabel }}</span>
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

    <template #definition>
      <div
        v-if="scheduler"
        :class="['notice', 'scheduler', scheduler === 'on' ? 'tone-success' : 'tone-warning']"
        role="status"
      >
        <Icon :name="scheduler === 'on' ? 'check' : 'warning'" />
        <span>Event scheduler is {{ scheduler.toUpperCase() }}<template v-if="scheduler !== 'on'">: events do not run until event_scheduler is ON</template>.</span>
        <span v-if="details?.lastExecuted">Last executed {{ details.lastExecuted }}</span>
      </div>
      <div class="layout">
        <div class="column">
          <section
            class="section"
            aria-label="Schedule"
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
                  :aria-invalid="!!errors.name"
                >
              </FormField>
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
            </div>
            <div class="row">
              <span class="label">Schedule</span>
              <div
                role="radiogroup"
                aria-label="Schedule"
                class="segmented"
              >
                <button
                  type="button"
                  role="radio"
                  :aria-checked="form.kind === 'at'"
                  @click="form.kind = 'at'"
                >
                  Once AT
                </button>
                <button
                  type="button"
                  role="radio"
                  :aria-checked="form.kind === 'every'"
                  @click="form.kind = 'every'"
                >
                  Recurring EVERY
                </button>
              </div>
            </div>
            <div
              v-if="form.kind === 'at'"
              class="grid"
            >
              <FormField
                label="At"
                :input-id="ids.at"
                :error="errors.at ?? ''"
                help="Blank: CURRENT_TIMESTAMP"
              >
                <input
                  :id="ids.at"
                  v-model="form.at"
                  class="input mono"
                  placeholder="YYYY-MM-DD HH:MM:SS"
                >
              </FormField>
              <FormField
                label="Plus interval"
                :input-id="ids.plusQ"
                :error="errors.atPlusQuantity ?? ''"
              >
                <div class="pair">
                  <input
                    :id="ids.plusQ"
                    v-model="form.atPlusQuantity"
                    class="input mono"
                    placeholder="none"
                  >
                  <select
                    v-model="form.atPlusUnit"
                    class="select"
                    aria-label="Interval unit"
                  >
                    <option
                      v-for="u in INTERVAL_UNITS"
                      :key="u.value"
                      :value="u.value"
                    >
                      {{ u.label }}
                    </option>
                  </select>
                </div>
              </FormField>
            </div>
            <div
              v-else
              class="grid"
            >
              <FormField
                label="Every"
                :input-id="ids.every"
                :error="errors.everyQuantity ?? ''"
              >
                <div class="pair">
                  <input
                    :id="ids.every"
                    v-model="form.everyQuantity"
                    class="input mono"
                  >
                  <select
                    v-model="form.everyUnit"
                    class="select"
                    aria-label="Interval unit"
                  >
                    <option
                      v-for="u in INTERVAL_UNITS"
                      :key="u.value"
                      :value="u.value"
                    >
                      {{ u.label }}
                    </option>
                  </select>
                </div>
              </FormField>
              <FormField
                label="Starts"
                :input-id="ids.starts"
                :error="errors.starts ?? ''"
              >
                <input
                  :id="ids.starts"
                  v-model="form.starts"
                  class="input mono"
                  placeholder="now"
                >
              </FormField>
              <FormField
                label="Ends"
                :input-id="ids.ends"
                :error="errors.ends ?? ''"
              >
                <input
                  :id="ids.ends"
                  v-model="form.ends"
                  class="input mono"
                  placeholder="no end"
                >
              </FormField>
            </div>
            <div class="row">
              <span class="label">On completion</span>
              <div
                role="radiogroup"
                aria-label="On completion"
                class="segmented"
              >
                <button
                  type="button"
                  role="radio"
                  :aria-checked="form.preserve"
                  @click="form.preserve = true"
                >
                  PRESERVE
                </button>
                <button
                  type="button"
                  role="radio"
                  :aria-checked="!form.preserve"
                  @click="form.preserve = false"
                >
                  NOT PRESERVE
                </button>
              </div>
              <span class="hint">PRESERVE keeps the event after its last run instead of dropping it.</span>
            </div>
            <div
              role="radiogroup"
              aria-label="Status"
              class="row"
            >
              <span class="label">Status</span>
              <label class="check"><input
                v-model="form.status"
                type="radio"
                value="enable"
              ><span>Enabled</span></label>
              <label class="check"><input
                v-model="form.status"
                type="radio"
                value="disable"
              ><span>Disabled</span></label>
              <label class="check"><input
                v-model="form.status"
                type="radio"
                value="disableOnReplica"
              ><span>Disabled on replica</span></label>
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
            <div class="section-head">
              <h2>Body</h2>
              <span class="hint mono">DO</span>
            </div>
            <div class="code-box tall">
              <SqlCode
                v-model="form.body"
                :engine="session.engine"
                label="Event body"
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
            v-if="runs !== null"
            class="section"
            aria-label="Next runs"
          >
            <div class="section-head">
              <h2>Next runs</h2>
              <span class="hint">time zone {{ details?.timeZone ?? 'of the server' }}, shown as this computer reads the times</span>
            </div>
            <ol class="list">
              <li
                v-for="r in runs"
                :key="r.getTime()"
                class="mono"
              >
                {{ runFormat.format(r) }}
              </li>
              <li
                v-if="runs.length === 0"
                class="hint"
              >
                No runs left before the end.
              </li>
            </ol>
          </section>
          <section
            v-if="details"
            class="section"
            aria-label="Details"
          >
            <h2>Details</h2>
            <dl class="facts">
              <dt>Created</dt><dd>{{ details.created ?? '' }}</dd>
              <dt>Altered</dt><dd>{{ details.altered ?? '' }}</dd>
              <dt>Last executed</dt><dd>{{ details.lastExecuted ?? 'never' }}</dd>
              <dt>Time zone</dt><dd>{{ details.timeZone ?? '' }}</dd>
              <dt>Originator</dt><dd>{{ details.originator ?? '' }}</dd>
            </dl>
            <p class="hint">
              The server records only when the last run started, not whether it succeeded.
            </p>
          </section>
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
.scheduler {
  margin: 16px 16px 0;
  flex-wrap: wrap;
  gap: 8px 16px;
}

.row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px 12px;
}

.pair {
  display: flex;
  gap: 8px;
}

.facts {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 4px 12px;
  margin: 0;
  font-size: 12px;
}

.facts dt {
  color: var(--text-muted);
}

.facts dd {
  margin: 0;
}
</style>
