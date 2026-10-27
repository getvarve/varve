<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { fetchActivity } from '../../api/activity';
import type { ActivityEvent } from '../../api/activity';
import type { PgMaterializedView } from '../../api/ddl';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { formatDateTime } from '../../lib/display';
import {
  buildPgMatView,
  buildPgMatViewRefresh,
  concurrentRefreshProblem,
  errorsOf,
  objectRef,
  pgMatViewFormFrom,
  pgMatViewSchema,
} from '../../lib/ddlPlan';
import type { PgMatViewForm } from '../../lib/ddlPlan';
import type { StepSpec } from '../../lib/ddlApply';
import { formatBytes, formatCount, formatDuration } from '../../lib/values';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import { requestFocus } from '../table/designerIntent';
import ChangePanel from './ChangePanel.vue';
import DdlView from './DdlView.vue';
import EditorFrame from './EditorFrame.vue';
import IdentityFields from './IdentityFields.vue';
import PlanPreview from './PlanPreview.vue';
import SqlCode from './SqlCode.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';
import { useObjectEditor } from './useObjectEditor';

/**
 * PostgreSQL materialized view editor (design canvas MatViewEditor):
 * definition (a changed query drops and recreates the view and its
 * indexes), refresh (plain or concurrent, WITH [NO] DATA), refresh history
 * from the activity log, indexes and DDL. Props and events: `ObjectEditorProps`
 * / `ObjectEditorEmits`. Opens for a `materializedView` node.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const schemaDefault = computed(() => props.container?.schema ?? 'public');
const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);

const editor = useObjectEditor<PgMaterializedView, PgMatViewForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'materializedView' ? d.object : 'This object is not a materialized view.'),
  formFrom: (view) => pgMatViewFormFrom(view, schemaDefault.value),
  validate: (form) => errorsOf(pgMatViewSchema, form),
  build: (form, view) => {
    const built = buildPgMatView(form, view, database.value);
    const steps: StepSpec[] = built.changes.map((change) => ({ kind: 'edit', change }));
    if (built.indexes.length > 0) {
      steps.push({ kind: 'ddl', ddl: built.indexes.map((i) => `${i};`).join('\n'), database: null, label: 'Recreate indexes' });
    }
    return steps;
  },
  identity: (form) => objectRef('materializedView', database.value, form.schema.trim(), form.name.trim()),
});
const { form, errors, details } = editor;

const recreates = computed(() => details.value !== null && form.query.trim() !== pgMatViewFormFrom(details.value, details.value.schema).query.trim());
const tab = ref('definition');
const tabs = computed(() => [
  { id: 'definition', label: 'Definition' },
  ...(details.value ? [{ id: 'columns', label: 'Columns', count: details.value.columns.length }] : []),
  ...(details.value ? [{ id: 'indexes', label: 'Indexes', count: details.value.indexes.length }] : []),
  ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : []),
]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : 'New materialized view'));
const blocked = computed(() => Object.values(errors.value)[0] ?? null);
const ids = { tablespace: useId(), withData: useId() };

// Refresh
const refreshPlan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => {
    emit('changed');
    void loadHistory();
  },
  reportError: (error) => props.reportError?.(error),
});
const refreshWithData = ref(true);
const concurrentProblem = computed(() => (details.value ? concurrentRefreshProblem(details.value, refreshWithData.value) : 'Not created yet.'));
const refreshMode = ref<'plain' | 'concurrent' | null>(null);
/** No unique index qualifies for REFRESH CONCURRENTLY (whatever the other conditions). */
const lacksUniqueIndex = computed(() => details.value !== null && concurrentRefreshProblem({ ...details.value, populated: true }, true) !== null);

/** Opens the table designer on the view's indexes (a unique index enables REFRESH CONCURRENTLY). */
function designIndexes(): void {
  const object = editor.current.value;
  const profile = useWorkspace().connection(props.session.connectionId);
  if (!object || !profile) return;
  const tabs = useTabs();
  const tab = tabs.openTableDesigner(profile, object, { database: object.database, schema: object.schema, parent: null });
  requestFocus(tab.id, 'indexes', null);
}

async function planRefresh(concurrently: boolean): Promise<void> {
  if (!details.value) return;
  refreshMode.value = concurrently ? 'concurrent' : 'plain';
  await refreshPlan.plan([{ kind: 'edit', change: buildPgMatViewRefresh(details.value, concurrently, refreshWithData.value) }]);
}

watch(refreshWithData, () => {
  if (refreshMode.value !== null) void planRefresh(refreshMode.value === 'concurrent' && concurrentProblem.value === null);
});

async function refreshed(): Promise<void> {
  refreshMode.value = null;
  refreshPlan.clear();
  await editor.load();
}

// Refresh history from the activity log (matview.refreshed).
const history = ref<ActivityEvent[]>([]);
const historyError = ref<string | null>(null);
async function loadHistory(): Promise<void> {
  if (!editor.current.value) return;
  try {
    const page = await fetchActivity({
      actions: ['matview.refreshed'],
      connectionId: props.session.connectionId,
      text: editor.current.value.name,
      limit: 10,
    });
    history.value = page.events.filter((e) => (e.target ?? '').includes(editor.current.value?.name ?? ''));
    historyError.value = null;
  } catch (error: unknown) {
    historyError.value = error instanceof Error ? error.message : 'The activity log could not be read.';
  }
}

function detailsMode(event: ActivityEvent): string {
  const d = event.details;
  if (typeof d === 'object' && d !== null && 'concurrently' in d) return (d as { concurrently: unknown }).concurrently ? 'Concurrently' : 'Plain';
  return '';
}

onMounted(async () => {
  await editor.load();
  await loadHistory();
});
watch(
  () => editor.current.value?.name,
  () => void loadHistory(),
);
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="title"
    kind-label="Materialized view"
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
        v-if="details"
        :class="['pill', details.populated ? 'tone-success' : 'tone-warning']"
      ><span class="dot" />{{ details.populated ? 'Populated' : 'Not populated' }}</span>
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
      <div class="layout">
        <div class="column">
          <section
            class="section"
            aria-label="Definition"
          >
            <div class="section-head">
              <h2>Definition</h2>
              <span
                v-if="editor.current.value"
                class="hint"
              >Changing it recreates the view (DROP and CREATE); its indexes are created again.</span>
            </div>
            <div class="code-box tall">
              <SqlCode
                v-model="form.query"
                engine="postgres"
                label="Materialized view query"
              />
            </div>
            <span
              v-if="errors.query"
              class="field-error"
              role="alert"
            >{{ errors.query }}</span>
            <p
              v-if="recreates"
              class="notice tone-warning"
              role="note"
            >
              <Icon name="warning" />The view is dropped and created again: grants on it are lost, and views that depend on it make the drop fail
              (the server refuses it; nothing is dropped then).
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
          />
          <section
            v-if="details"
            class="section"
            aria-label="Refresh history"
          >
            <div class="section-head">
              <h2>Refresh history</h2>
              <span class="hint">from the activity log, event <span class="mono">matview.refreshed</span></span>
            </div>
            <p
              v-if="historyError"
              class="hint"
              role="alert"
            >
              {{ historyError }}
            </p>
            <table
              v-else-if="history.length > 0"
              class="table"
            >
              <thead>
                <tr>
                  <th>When</th>
                  <th>Mode</th>
                  <th>Duration</th>
                  <th>Result</th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="h in history"
                  :key="h.id"
                >
                  <td>{{ formatDateTime(h.occurredAt) }}</td>
                  <td>{{ detailsMode(h) }}</td>
                  <td class="mono">
                    {{ h.durationMs === null ? '' : formatDuration(h.durationMs) }}
                  </td>
                  <td>
                    <span :class="['pill', h.outcome === 'success' ? 'tone-success' : 'tone-danger']"><span class="dot" />{{ h.outcome }}</span>
                  </td>
                </tr>
              </tbody>
            </table>
            <p
              v-else
              class="hint"
            >
              No refresh from this app is logged yet.
            </p>
          </section>
        </div>
        <div class="column">
          <section
            v-if="details"
            class="section"
            aria-label="Refresh"
          >
            <h2>Refresh</h2>
            <div class="stats">
              <div class="stat">
                <span class="label">Last refresh</span><span>{{ details.lastRefreshNote }}</span>
              </div>
              <div class="stat">
                <span class="label">Size</span><span>{{ details.totalBytes === null ? (details.sizeNote ?? 'unknown') : formatBytes(details.totalBytes) }}</span>
              </div>
              <div class="stat">
                <span class="label">Rows (estimate)</span><span>{{ details.rowEstimate === null ? 'unknown' : formatCount(details.rowEstimate) }}</span>
              </div>
            </div>
            <label class="check">
              <input
                :id="ids.withData"
                v-model="refreshWithData"
                type="checkbox"
              ><span><span class="mono">WITH DATA</span><span class="sub">Off runs WITH NO DATA: the view is emptied and reads fail until the next refresh.</span></span>
            </label>
            <div class="row">
              <button
                type="button"
                class="btn compact"
                :disabled="session.readOnly"
                @click="planRefresh(false)"
              >
                <Icon name="refresh" />Refresh
              </button>
              <button
                type="button"
                class="btn compact"
                :disabled="session.readOnly || concurrentProblem !== null"
                :title="concurrentProblem ?? undefined"
                @click="planRefresh(true)"
              >
                Refresh concurrently
              </button>
            </div>
            <p class="hint">
              {{ concurrentProblem ?? 'Concurrent refresh keeps the view readable. A plain refresh locks it for reads while it runs.' }}
              <button
                v-if="lacksUniqueIndex && editor.current.value"
                type="button"
                class="btn compact"
                @click="designIndexes"
              >
                Design indexes
              </button>
            </p>
            <PlanPreview
              v-if="refreshMode !== null"
              :plan="refreshPlan"
              engine="postgres"
              :read-only="session.readOnly"
              apply-label="Refresh"
              @applied="refreshed"
              @reconnect="emit('reconnect')"
              @refresh="editor.load"
            />
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
            <FormField
              label="Tablespace"
              :input-id="ids.tablespace"
              help="Blank uses the database default."
            >
              <input
                :id="ids.tablespace"
                v-model="form.tablespace"
                class="input mono"
                autocomplete="off"
                spellcheck="false"
              >
            </FormField>
            <label
              v-if="editor.creating.value"
              class="check"
            >
              <input
                v-model="form.withData"
                type="checkbox"
              ><span>Populate when created (WITH DATA)</span>
            </label>
          </section>
        </div>
      </div>
    </template>

    <template #columns>
      <div class="pad">
        <table
          v-if="details"
          class="table"
        >
          <thead>
            <tr>
              <th>#</th>
              <th>Name</th>
              <th>Type</th>
              <th>Comment</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="c in details.columns"
              :key="c.name"
            >
              <td class="mono subtle">
                {{ c.position }}
              </td>
              <td class="mono">
                {{ c.name }}
              </td>
              <td class="mono">
                {{ c.dataType }}
              </td>
              <td>{{ c.comment ?? '' }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </template>

    <template #indexes>
      <div class="pad">
        <ul
          v-if="details"
          class="list"
        >
          <li
            v-for="i in details.indexes"
            :key="i.name"
            class="section"
          >
            <div class="section-head">
              <span class="mono">{{ i.name }}</span>
              <span
                v-if="i.unique"
                class="pill tone-info"
              >UNIQUE</span>
              <span
                v-if="!i.valid"
                class="pill tone-warning"
              >invalid</span>
              <span class="grow" />
              <span class="hint">{{ i.sizeBytes === null ? '' : formatBytes(i.sizeBytes) }}</span>
            </div>
            <span class="hint mono">{{ i.definition }}</span>
          </li>
          <li
            v-if="details.indexes.length === 0"
            class="hint"
          >
            No indexes. Concurrent refresh needs a unique index.
          </li>
        </ul>
        <button
          v-if="details && editor.current.value"
          type="button"
          class="btn compact"
          @click="designIndexes"
        >
          {{ lacksUniqueIndex ? 'Design indexes (add a unique index for concurrent refresh)' : 'Design indexes' }}
        </button>
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
  flex-wrap: wrap;
  gap: 8px;
}
</style>
