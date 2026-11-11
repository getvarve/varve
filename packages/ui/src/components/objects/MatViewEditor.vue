<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { fetchActivity } from '../../api/activity';
import type { ActivityEvent } from '../../api/activity';
import { catalogDescribe, catalogDropImpact } from '../../api/ddl';
import type { PgMaterializedView } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { latestOnly } from '../../lib/async';
import { formatDateTime } from '../../lib/display';
import {
  buildPgMatView,
  buildPgMatViewRefresh,
  concurrentRefreshProblem,
  errorsOf,
  objectRef,
  pgMatViewFormFrom,
  pgMatViewRecreateProblem,
  pgMatViewSchema,
  pgMatViewStanding,
  pgMatViewStandingText,
} from '../../lib/ddlPlan';
import type { PgMatViewBuild, PgMatViewForm, PgMatViewStanding } from '../../lib/ddlPlan';
import { isMissing } from '../../lib/ddlApply';
import { refreshedMatView } from '../../lib/ddlText';
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
 * definition (a changed query creates the new view under a temporary name
 * with its owner and indexes, then drops the old one and gives the new one
 * and its indexes the real names; a recreate that stops part way keeps its
 * remaining steps and says what exists on the server),
 * refresh (plain or concurrent, WITH [NO] DATA), refresh history
 * from the activity log, indexes and DDL. Props and events: `ObjectEditorProps`
 * / `ObjectEditorEmits`. Opens for a `materializedView` node.
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const schemaDefault = computed(() => props.container?.schema ?? 'public');
const database = computed(() => props.object?.database ?? props.container?.database ?? props.session.database);

const built = (form: PgMatViewForm, view: PgMaterializedView | null): PgMatViewBuild => buildPgMatView(form, view, database.value);
const named = (schema: string, name: string): string => displayName({ schema, database: null, name }, 'postgres');

/** The materialized view of that name, or `null` when there is none. */
async function describeView(schema: string, name: string): Promise<PgMaterializedView | null> {
  try {
    const described = await catalogDescribe(props.session.id, objectRef('materializedView', database.value, schema, name));
    return described.engine === 'postgres' && described.object.kind === 'materializedView' ? described.object : null;
  } catch (error: unknown) {
    if (isMissing(error)) return null;
    throw error;
  }
}

/**
 * Whether a recreate can start (nothing depends on the view, the temporary
 * name and a new name are free), read from the server before it is planned.
 * `null` until the look for the current `key` is back.
 */
const recreateCheck = ref<{ key: string; reason: string | null } | null>(null);
/** What a recreate check depends on: the session, the view and the names the recreate uses. */
const recreateKeyOf = (form: PgMatViewForm, view: PgMaterializedView): string =>
  JSON.stringify([props.session.id, view.schema, view.name, form.schema.trim(), form.name.trim()]);
/** Where a recreate that stopped part way stands; `null` when none did, or when the server could not be read. */
const standing = ref<PgMatViewStanding | null>(null);

/** What a recreate that stopped part way left on the server, as a sentence; `null` when the save was no recreate or nothing of it is left. */
async function recreateStanding(form: PgMatViewForm, view: PgMaterializedView | null): Promise<string | null> {
  const recreate = view ? built(form, view).recreate : null;
  if (!view || !recreate) {
    standing.value = null;
    return null;
  }
  const schema = form.schema.trim();
  const name = form.name.trim();
  let found: { old: boolean; temporary: boolean; finalIndexes: string[] };
  if ((await describeView(schema, recreate.tempName)) !== null) {
    found = { old: (await describeView(view.schema, view.name)) !== null, temporary: true, finalIndexes: [] };
  } else {
    const final = await describeView(schema, name);
    found = { old: false, temporary: false, finalIndexes: final?.indexes.map((i) => i.name) ?? [] };
  }
  standing.value = pgMatViewStanding(recreate, found);
  if (standing.value === null) return null;
  return pgMatViewStandingText(standing.value, {
    old: named(view.schema, view.name),
    temporary: named(schema, recreate.tempName),
    final: named(schema, name),
  });
}

const editor = useObjectEditor<PgMaterializedView, PgMatViewForm>({
  props,
  emit,
  pick: (d) => (d.engine === 'postgres' && d.object.kind === 'materializedView' ? d.object : 'This object is not a materialized view.'),
  formFrom: (view) => pgMatViewFormFrom(view, schemaDefault.value),
  validate: (form) => errorsOf(pgMatViewSchema, form),
  blocked: (form, view) => {
    const build = built(form, view);
    if (build.problems[0] !== undefined) return build.problems[0];
    if (!view || !build.recreate) return null;
    const check = recreateCheck.value;
    if (check === null || check.key !== recreateKeyOf(form, view)) return 'Checking that the view can be dropped and recreated.';
    return check.reason;
  },
  build: (form, view) => built(form, view).steps,
  identity: (form) => objectRef('materializedView', database.value, form.schema.trim(), form.name.trim()),
  unfinished: recreateStanding,
});
const { form, errors, details } = editor;

/** The names a changed query recreates the view with, or `null` when the save changes it in place. */
const recreate = computed(() => (details.value ? built(form, details.value).recreate : null));
const recreates = computed(() => recreate.value !== null);
/** The temporary name a recreate creates the new view under. */
const tempName = computed(() => recreate.value?.tempName ?? null);
/** A recreate stopped part way: the form waits until its remaining steps ran (or, while the old view still exists, the user reverts). */
const unfinished = computed(() => editor.unfinished.value);

const recreateKey = computed(() => (details.value && recreate.value ? recreateKeyOf(form, details.value) : null));
const recreateChecks = latestOnly();
/** Looks at the server before a recreate is planned; the result blocks the plan or lets it through. */
async function checkRecreate(): Promise<void> {
  const token = recreateChecks.next();
  const key = recreateKey.value;
  const view = details.value;
  const object = editor.current.value;
  const names = recreate.value;
  recreateCheck.value = null;
  // The temporary view of an unfinished recreate is its own; nothing new is planned until that is over.
  if (key === null || view === null || object === null || names === null || unfinished.value) return;
  const schema = form.schema.trim();
  const name = form.name.trim();
  let reason: string | null;
  const sessionId = props.session.id;
  try {
    const impact = await catalogDropImpact(sessionId, object);
    const temporaryTaken = (await describeView(schema, names.tempName)) !== null;
    const renamed = schema !== view.schema || name !== view.name;
    const nameTaken = renamed && (await describeView(schema, name)) !== null;
    reason = pgMatViewRecreateProblem(
      {
        dependents: impact.dependents.filter((d) => d.effect === 'blocks').map((d) => d.description),
        dependentsTruncated: impact.dependentsTruncated,
        temporaryTaken,
        nameTaken,
      },
      { temporary: named(schema, names.tempName), final: named(schema, name) },
    );
  } catch (error: unknown) {
    props.reportError?.(error, sessionId);
    const p = presentError(toCmdError(error));
    reason = `Could not check that the view can be dropped and recreated (${p.detail ? `${p.message}; ${p.detail}` : p.message}). Reload the editor to check again.`;
  }
  if (!recreateChecks.isCurrent(token)) return;
  recreateCheck.value = { key, reason };
}
watch(recreateKey, () => void checkRecreate());
// A reload may have changed what depends on the view, and a run that failed may have left the temporary view.
watch(details, () => void checkRecreate());
watch(
  () => editor.plan.state.applying,
  (applying) => {
    if (!applying) void checkRecreate();
  },
);
// The plan waited for the check: plan now that it allows it (unless the edit's own plan is still on its way).
watch(recreateCheck, (check) => {
  if (check !== null && check.reason === null) editor.planIfDue();
});

const tab = ref('definition');
const tabs = computed(() => [
  { id: 'definition', label: 'Definition' },
  ...(details.value ? [{ id: 'columns', label: 'Columns', count: details.value.columns.length }] : []),
  ...(details.value ? [{ id: 'indexes', label: 'Indexes', count: details.value.indexes.length }] : []),
  ...(editor.current.value ? [{ id: 'ddl', label: 'DDL' }] : []),
]);
const title = computed(() => (editor.current.value ? displayName(editor.current.value, 'postgres') : 'New materialized view'));
const blocked = computed(() => Object.values(errors.value)[0] ?? editor.blockedReason.value);
const ids = { tablespace: useId(), withData: useId() };

// Refresh
const refreshPlan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => {
    emit('changed');
    void loadHistory();
  },
  reportError: (error, sessionId) => props.reportError?.(error, sessionId),
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
// A planned refresh names the view as it was: after a save, rename or reload it may name another one.
watch(details, () => {
  if (refreshPlan.state.applying) return;
  refreshMode.value = null;
  refreshPlan.clear();
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
    const view = editor.current.value;
    const page = await fetchActivity({
      actions: ['matview.refreshed'],
      connectionId: props.session.connectionId,
      text: view.name,
      // Read more than are shown: the filter below drops other views' refreshes.
      limit: 50,
    });
    // The text filter matches anywhere (sales also finds sales_daily and eu.sales): keep the refreshes of exactly
    // this view. The event's target names it as `schema.name` (`name` for a change planned without a schema),
    // whatever the log keeps of the statement; events from before targets were recorded are matched by statement.
    const qualified = view.schema === null ? view.name : `${view.schema}.${view.name}`;
    history.value = page.events.filter((e) => {
      if (e.target !== null) return e.target === qualified || e.target === view.name;
      const statement = typeof e.details === 'object' && e.details !== null ? (e.details as { statement?: unknown }).statement : undefined;
      const refreshedView = typeof statement === 'string' ? refreshedMatView(statement) : null;
      return refreshedView !== null && refreshedView.name === view.name && refreshedView.schema === view.schema;
    }).slice(0, 10);
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
        :disabled="!editor.dirty.value || (unfinished && standing !== 'both')"
        :title="unfinished && standing !== 'both' ? 'The old view is gone: finish the save with its remaining steps.' : undefined"
        @click="editor.revert"
      >
        Revert
      </button>
      <button
        v-if="editor.current.value"
        type="button"
        class="btn compact"
        :disabled="session.readOnly || editor.mode.value === 'drop' || unfinished"
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
              >Changing it recreates the view; its indexes are created again.</span>
            </div>
            <div class="code-box tall">
              <SqlCode
                v-model="form.query"
                engine="postgres"
                label="Materialized view query"
                :readonly="unfinished"
              />
            </div>
            <span
              v-if="errors.query"
              class="field-error"
              role="alert"
            >{{ errors.query }}</span>
            <p
              v-if="recreates && !unfinished"
              class="notice tone-warning"
              role="note"
            >
              <Icon name="warning" />The new query is created first as <span class="mono">{{ tempName }}</span>, with its owner and indexes, so a
              query or an index that fails changes nothing. Then the old view is dropped and the new one takes its name: grants on the old view
              are lost. If the drop fails (something came to depend on the view meanwhile), the old view stays and the editor says what exists
              and offers the remaining steps.
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
                :disabled="session.readOnly || unfinished"
                @click="planRefresh(false)"
              >
                <Icon name="refresh" />Refresh
              </button>
              <button
                type="button"
                class="btn compact"
                :disabled="session.readOnly || unfinished || concurrentProblem !== null"
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
            <!-- An unfinished recreate runs its remaining steps as planned: the fields wait. -->
            <fieldset
              class="contents"
              :disabled="unfinished"
            >
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
            </fieldset>
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

/* Groups the fields for `disabled` only; no box of its own, so they stay items of the section. */
fieldset.contents {
  display: contents;
  margin: 0;
  padding: 0;
  border: 0;
  min-width: 0;
}
</style>
