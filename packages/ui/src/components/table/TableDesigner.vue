<script setup lang="ts">
import { computed, nextTick, onMounted, provide, reactive, ref, watch } from 'vue';
import type { ObjectKind } from '../../api/catalog';
import { catalogRowCount, describeDesignable, runGridQuery, tableRef } from '../../api/grid';
import type { DesignerDetails, EngineChange, TableTarget } from '../../api/grid';
import { presentError, toCmdError } from '../../api/errors';
import type { SessionInfo } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import { buildDraftChanges, cloneDraft, draftFromDetails, newTableDraft, summarize } from '../../lib/gridDesigner';
import type { TableDraft } from '../../lib/gridDesigner';
import Icon from '../common/Icon.vue';
import ColumnsSection from './ColumnsSection.vue';
import ConstraintsSection from './ConstraintsSection.vue';
import { DESIGNER, invisibleColumnsOn } from './designerContext';
import type { DesignerContext } from './designerContext';
import type { DesignerFocus, DesignerSection } from './designerIntent';
import IndexesSection from './IndexesSection.vue';
import PartitionBySection from './PartitionBySection.vue';
import PartitionsSection from './PartitionsSection.vue';
import PropertiesSection from './PropertiesSection.vue';
import PlanReview from './PlanReview.vue';
import { applySteps, planSteps } from './planFlow';
import { countedNote, reviewNotes } from './reviewNotes';
import type { PendingCount } from './reviewNotes';
import type { PlanStep } from './planFlow';

/**
 * The table designer (canvas TableDesigner-Columns, -Constraints,
 * -Indexes, -Partitions): edits a working copy of the table and saves it
 * as planned, reviewed SQL.
 */
const props = withDefaults(
  defineProps<{
    /** The user session of the tab (PostgreSQL, MySQL or MariaDB). */
    session: SessionInfo;
    /** Name of the connection, for the destructive confirmation dialog. */
    connectionName: string;
    /** The table: database, schema (PostgreSQL) and name; `null` designs a new table in `newIn`. */
    target: TableTarget | null;
    /**
     * Catalog kind of the object: `table`, `partitionedTable`, `partition`, or a PostgreSQL
     * `materializedView` (its indexes only). A PostgreSQL `table` that turns out to be a
     * materialized view (an index node only names its relation) is designed as one.
     */
    objectKind?: ObjectKind;
    /** Where a new table is created (used when `target` is `null`): PostgreSQL schema and database, or the MySQL database. */
    newIn?: { database: string; schema: string | null } | null;
    /** A new PostgreSQL table starts partitioned ("New partitioned table..."). */
    startPartitioned?: boolean;
    /** Section (and index, constraint or partition) to show; a new `seq` moves there again. */
    focus?: DesignerFocus | null;
  }>(),
  { objectKind: 'table', newIn: null, startPartitioned: false, focus: null },
);
const emit = defineEmits<{
  /** Something was applied: refresh the object tree and other views of the table. */
  changed: [];
  /** The designer has unsaved changes (true) or none (false), for the tab's unsaved marker. */
  dirty: [dirty: boolean];
  /** A new table was created, or the table was renamed or moved; the designer now edits it under that name. */
  created: [target: TableTarget, kind: ObjectKind];
  /** The designer showed `focus`. */
  focused: [focus: DesignerFocus];
  /** A call failed; the parent reports it (lost sessions, reconnect needed). */
  error: [error: unknown];
}>();

/** The table being designed; set after a new table is created. */
const current = ref<TableTarget | null>(props.target);
const engine = computed<'postgres' | 'mysql'>(() => (props.session.engine === 'postgres' ? 'postgres' : 'mysql'));

type Section = DesignerSection;
const section = ref<Section>('columns');
/** The kind the table was read as (a `table` that is a materialized view becomes `materializedView`). */
const readKind = ref<ObjectKind>(props.objectKind);
/** Bumped when a focus request arrives, so the sections pick the item up again. */
const focusCount = ref(0);
const root = ref<HTMLElement | null>(null);
/** PostgreSQL: the database's default tablespace, once read. */
const databaseTablespace = ref('');
const loading = ref(true);
const loadError = ref<string | null>(null);
const banner = ref<{ tone: 'danger' | 'warning' | 'success' | 'info'; text: string } | null>(null);
const saveErrors = ref<string[]>([]);
const loadCount = ref(0);

const ctx = reactive<DesignerContext>({
  draft: null as unknown as TableDraft,
  original: null as unknown as TableDraft,
  details: null,
  readOnly: props.session.readOnly,
  serverMajor: props.session.server.major,
  invisibleColumns: invisibleColumnsOn(props.session.server),
  runNow: (title, changes) => void runNow(title, changes),
  focus: null,
  sessionId: props.session.id,
});
provide(DESIGNER, ctx);
const ready = ref(false);

async function load(): Promise<void> {
  const target = current.value;
  if (!target) {
    const where = props.newIn ?? { database: props.session.database ?? '', schema: engine.value === 'postgres' ? 'public' : null };
    ctx.details = null;
    ctx.original = newTableDraft(engine.value, where.schema ?? where.database, where.database, props.startPartitioned && engine.value === 'postgres');
    ctx.draft = cloneDraft(ctx.original);
    ready.value = true;
    loading.value = false;
    loadCount.value += 1;
    return;
  }
  loading.value = true;
  loadError.value = null;
  try {
    const details = await describe(target);
    ctx.details = details;
    ctx.original = draftFromDetails(details);
    ctx.original.props.databaseTablespace = databaseTablespace.value;
    ctx.draft = cloneDraft(ctx.original);
    saveErrors.value = [];
    ready.value = true;
    loadCount.value += 1;
  } catch (error: unknown) {
    emit('error', error);
    loadError.value = presentError(toCmdError(error)).message;
  } finally {
    loading.value = false;
  }
}

/** True for a catalog `notFound` error. */
function isNotFound(error: unknown): boolean {
  const cmd = toCmdError(error);
  return cmd.kind === 'catalog' && cmd.error.kind === 'notFound';
}

/** Reads the designed object; a PostgreSQL `table` that is not one is tried as a materialized view. */
async function describe(target: TableTarget): Promise<DesignerDetails> {
  try {
    return await describeDesignable(props.session.id, tableRef(target, readKind.value));
  } catch (error: unknown) {
    if (engine.value !== 'postgres' || readKind.value !== 'table' || !isNotFound(error)) throw error;
    const details = await describeDesignable(props.session.id, tableRef(target, 'materializedView'));
    readKind.value = 'materializedView';
    return details;
  }
}

onMounted(async () => {
  await load();
  applyFocus(props.focus);
  await readDatabaseTablespace();
});

/** Shows the requested section and item. */
function applyFocus(focus: DesignerFocus | null): void {
  if (!focus || !ready.value) return;
  let target: Section = focus.section;
  const name = focus.name;
  if (name !== null && (target === 'constraints' || target === 'indexes')) {
    // A MySQL unique key is listed as a constraint by the catalog but designed as an index (and the other way round).
    const inConstraints = ctx.draft.constraints.some((k) => k.origName === name);
    const inIndexes = ctx.draft.indexes.some((i) => i.origName === name);
    if (target === 'constraints' && !inConstraints && inIndexes) target = 'indexes';
    if (target === 'indexes' && !inIndexes && inConstraints) target = 'constraints';
  }
  if (!SECTIONS.value.some((s) => s.id === target)) target = SECTIONS.value[0]?.id ?? 'columns';
  section.value = target;
  ctx.focus = { section: target, name };
  focusCount.value += 1;
  emit('focused', focus);
  void nextTick(() => root.value?.querySelector('.panel tr[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' }));
}

watch(
  () => props.focus?.seq,
  () => applyFocus(props.focus),
);

const summary = computed(() => (ready.value ? summarize(ctx.original, ctx.draft) : null));
const dirty = computed(() => (summary.value?.total ?? 0) > 0);
watch(dirty, (d) => emit('dirty', d));

const counts = computed<Record<Section, number | null>>(() => {
  if (!ready.value) return { columns: 0, indexes: 0, constraints: 0, partitions: 0, properties: null };
  const d = ctx.details;
  const partitions = d && d.object.kind !== 'materializedView' ? (d.object.partitioning?.partitions.length ?? 0) : 0;
  return {
    columns: ctx.draft.columns.filter((c) => !c.dropped).length,
    indexes: ctx.draft.indexes.filter((i) => !i.dropped).length,
    constraints: ctx.draft.constraints.filter((k) => !k.dropped).length,
    partitions: ctx.draft.isNew ? null : partitions,
    properties: null,
  };
});

const ALL_SECTIONS: { id: Section; label: string }[] = [
  { id: 'columns', label: 'Columns' },
  { id: 'indexes', label: 'Indexes' },
  { id: 'constraints', label: 'Constraints' },
  { id: 'partitions', label: 'Partitions' },
  { id: 'properties', label: 'Properties' },
];
/**
 * A materialized view has only its indexes here; a new MySQL / MariaDB table is
 * partitioned after it exists (its Partitions section); a new PostgreSQL table
 * chooses PARTITION BY there, because PostgreSQL cannot partition it later.
 */
const SECTIONS = computed(() => {
  if (!ready.value) return ALL_SECTIONS;
  if (ctx.draft.kind === 'materializedView') return ALL_SECTIONS.filter((s) => s.id === 'indexes');
  if (ctx.draft.isNew && ctx.draft.engine === 'mysql') return ALL_SECTIONS.filter((s) => s.id !== 'partitions');
  if (ctx.draft.isNew) return ALL_SECTIONS.map((s) => (s.id === 'partitions' ? { id: s.id, label: 'Partitioning' } : s));
  return ALL_SECTIONS;
});

/** What the designer calls the object in titles and messages. */
const what = computed(() => (ready.value && ctx.draft.kind === 'materializedView' ? 'materialized view' : 'table'));

function sectionChanges(id: Section): number {
  if (!summary.value || id === 'partitions') return 0;
  return summary.value[id].total;
}

const footerText = computed(() => {
  const s = summary.value;
  if (!s || s.total === 0) return '';
  const parts: string[] = [];
  const added = s.columns.added + s.indexes.added + s.constraints.added;
  const modified = s.columns.modified + s.indexes.modified + s.constraints.modified + s.properties.modified;
  const dropped = s.columns.dropped + s.indexes.dropped + s.constraints.dropped;
  if (added) parts.push(`${added} added`);
  if (modified) parts.push(`${modified} modified`);
  if (dropped) parts.push(`${dropped} dropped`);
  return parts.join(', ');
});

// --- review and apply ---

const review = reactive<{
  open: boolean;
  title: string;
  steps: PlanStep[];
  busy: boolean;
  message: { tone: 'danger' | 'warning' | 'success'; text: string } | null;
  /** Save reloads the whole designer afterwards; a single action only refreshes the details. */
  kind: 'save' | 'now';
  ran: boolean;
}>({ open: false, title: '', steps: [], busy: false, message: null, kind: 'save', ran: false });

const planning = ref(false);

/** The first value of the first row of a read-only query, as text; `null` for NULL or no row. */
async function scalar(sql: string): Promise<string | null> {
  const out = await runGridQuery(props.session.id, sql, 1);
  const first = out.results[0];
  const value = first?.kind === 'rows' ? first.rows[0]?.[0] : undefined;
  if (!value || value.t === 'null') return null;
  return 'v' in value ? String(value.v) : null;
}

/** PostgreSQL: where an emptied tablespace moves the table (the database's own default). */
async function readDatabaseTablespace(): Promise<void> {
  if (engine.value !== 'postgres' || ctx.draft.isNew || ctx.draft.kind === 'materializedView') return;
  try {
    const name = await scalar(
      'SELECT t.spcname::text FROM pg_catalog.pg_database d JOIN pg_catalog.pg_tablespace t ON t.oid = d.dattablespace WHERE d.datname = pg_catalog.current_database()',
    );
    if (name) {
      databaseTablespace.value = name;
      ctx.original.props.databaseTablespace = name;
      ctx.draft.props.databaseTablespace = name;
    }
  } catch {
    // pg_default is the fallback; the hint says so.
  }
}

/** Counts the rows an engine change loses (metadata session, capped on the server) and updates the open review's note. */
async function countRows(steps: PlanStep[], pending: PendingCount): Promise<void> {
  let counted: number | null;
  try {
    counted = await catalogRowCount(props.session.id, ctx.draft.database, pending.table);
  } catch {
    return;
  }
  const step = steps[pending.step];
  if (step && review.steps === steps && step.status === 'pending') step.notes[pending.note] = countedNote(pending, counted);
}

async function openReview(title: string, changes: EngineChange[], kind: 'save' | 'now'): Promise<void> {
  planning.value = true;
  banner.value = null;
  try {
    const found = kind === 'save' ? await reviewNotes(ctx.draft, ctx.details, changes, scalar) : { notes: [], counts: [] };
    const steps = await planSteps(props.session.id, changes, found.notes);
    Object.assign(review, { open: true, title, steps, busy: false, message: null, kind, ran: false });
    // The review shows the estimate at once; exact counts replace it when they arrive (never awaited).
    for (const pending of found.counts) void countRows(steps, pending);
  } catch (error: unknown) {
    emit('error', error);
    banner.value = { tone: 'danger', text: error instanceof Error ? error.message : String(error) };
  } finally {
    planning.value = false;
  }
}

async function preview(): Promise<void> {
  const details = ctx.details;
  const { changes, errors } = buildDraftChanges(ctx.original, ctx.draft, details?.engine === 'mysql' ? details.object.columns : [], props.session.server.major);
  saveErrors.value = errors;
  if (errors.length > 0) return;
  if (changes.length === 0) {
    banner.value = { tone: 'info', text: 'Nothing to save: the draft matches the table.' };
    return;
  }
  const title = ctx.draft.isNew
    ? `Create ${ctx.draft.partitionBy.enabled ? 'partitioned table ' : ''}${ctx.draft.schema}.${ctx.draft.table.trim()}`
    : `Save changes to ${ctx.draft.schema}.${ctx.draft.table}`;
  await openReview(title, changes, 'save');
}

async function runNow(title: string, changes: EngineChange[]): Promise<void> {
  if (changes.length === 0) return;
  await openReview(title, changes, 'now');
}

async function apply(): Promise<void> {
  review.busy = true;
  review.message = null;
  try {
    const result = await applySteps(props.session.id, review.steps, { connectionName: props.connectionName, environment: props.session.environment });
    if (review.steps.some((s) => s.status === 'applied' || s.status === 'failed')) review.ran = true;
    switch (result.kind) {
      case 'done':
        review.message = { tone: 'success', text: 'Applied.' };
        break;
      case 'declined':
        review.message = { tone: 'warning', text: 'Not confirmed: the remaining steps did not run. Apply again to confirm.' };
        break;
      case 'openTransaction':
        review.message = { tone: 'warning', text: `${result.message} Commit or roll back in the console first, then apply again; the plan is kept.` };
        break;
      case 'failed':
        review.message = { tone: 'danger', text: `${result.message} Steps before it stay applied; the table is read again when you close this.` };
        break;
    }
  } finally {
    review.busy = false;
  }
}

async function closeReview(): Promise<void> {
  const ran = review.ran;
  const kind = review.kind;
  const failed = review.steps.find((s) => s.status === 'failed');
  review.open = false;
  if (!ran) return;
  emit('changed');
  if (kind === 'save') {
    if (ctx.draft.isNew && review.steps[0]?.status === 'applied') {
      const created: TableTarget = {
        database: ctx.draft.database || (props.newIn?.database ?? props.session.database ?? ''),
        schema: engine.value === 'postgres' ? ctx.draft.schema : null,
        table: ctx.draft.table.trim(),
      };
      const kind: ObjectKind = ctx.draft.partitionBy.enabled ? 'partitionedTable' : 'table';
      readKind.value = kind;
      current.value = created;
      emit('created', created, kind);
    } else if (!ctx.draft.isNew) {
      const moved = movedTo(review.steps);
      if (moved) {
        current.value = moved;
        emit('created', moved, readKind.value);
      }
    }
    await load();
    banner.value = failed
      ? { tone: 'danger', text: `${failed.label} failed: ${failed.error ?? ''} The table was read again; redo the changes that did not run.` }
      : { tone: 'success', text: 'Saved.' };
  } else {
    await refreshDetails();
  }
}

/** Where an applied rename or SET SCHEMA moved the table, or `null` when none ran. */
function movedTo(steps: PlanStep[]): TableTarget | null {
  const target = current.value;
  if (!target) return null;
  let moved: TableTarget | null = null;
  for (const step of steps) {
    if (step.status !== 'applied') continue;
    const c = step.change;
    if (c.engine === 'postgres' && c.change.object === 'table' && c.change.action === 'alter') {
      for (const op of c.change.operations) {
        if (op.op === 'rename') moved = { ...(moved ?? target), table: op.newName };
        if (op.op === 'setSchema') moved = { ...(moved ?? target), schema: op.schema };
      }
    } else if (c.engine === 'mysql' && c.change.change === 'alterTable') {
      for (const op of c.change.operations) {
        if (op.op === 'rename' && 'name' in op) moved = { ...(moved ?? target), database: op.database ?? target.database, table: op.name };
      }
    }
  }
  return moved;
}

/** Reads the table again for partition lists and validation flags without dropping the draft. */
async function refreshDetails(): Promise<void> {
  const target = current.value;
  if (!target) return;
  try {
    const details = await describe(target);
    ctx.details = details;
    const fresh = draftFromDetails(details);
    for (const list of [ctx.original.constraints, ctx.draft.constraints]) {
      for (const k of list) {
        const now = fresh.constraints.find((f) => f.origName === k.origName);
        if (now && k.origName !== null) k.validated = now.validated;
      }
    }
    loadCount.value += 1;
  } catch (error: unknown) {
    emit('error', error);
    banner.value = { tone: 'danger', text: presentError(toCmdError(error)).message };
  }
}

async function discard(): Promise<void> {
  const { askConfirm } = useDialogs();
  const ok = await askConfirm({
    title: 'Discard designer changes?',
    message: 'Every unsaved change to columns, indexes, constraints and properties is dropped.',
    confirmLabel: 'Discard',
    danger: true,
  });
  if (!ok) return;
  ctx.draft = cloneDraft(ctx.original);
  saveErrors.value = [];
}

async function reload(): Promise<void> {
  if (dirty.value) {
    const { askConfirm } = useDialogs();
    const ok = await askConfirm({
      title: `Read the ${what.value} again?`,
      message: 'Your unsaved designer changes are dropped.',
      confirmLabel: 'Reload',
      danger: true,
    });
    if (!ok) return;
  }
  await load();
}
</script>

<template>
  <div
    ref="root"
    class="designer"
  >
    <div
      v-if="loading && !ready"
      class="state muted"
    >
      Reading the {{ objectKind === 'materializedView' ? 'materialized view' : 'table' }}…
    </div>
    <div
      v-else-if="loadError && !ready"
      class="state"
    >
      <p
        class="notice tone-danger"
        role="alert"
      >
        {{ loadError }}
      </p>
      <button
        type="button"
        class="btn"
        @click="load"
      >
        Try again
      </button>
    </div>
    <template v-else-if="ready">
      <div
        role="tablist"
        aria-label="Designer sections"
        class="sections"
      >
        <button
          v-for="s in SECTIONS"
          :key="s.id"
          type="button"
          role="tab"
          :aria-selected="section === s.id"
          class="section-tab"
          @click="section = s.id"
        >
          {{ s.label }}
          <span
            v-if="sectionChanges(s.id) > 0"
            class="badge changed"
            :aria-label="`${sectionChanges(s.id)} unsaved`"
          >{{ sectionChanges(s.id) }}</span>
          <span
            v-else-if="counts[s.id] !== null"
            class="badge"
          >{{ counts[s.id] }}</span>
        </button>
        <span class="spacer" />
        <label
          v-if="ctx.draft.isNew"
          class="new-name"
        >
          <span class="label">Table name</span>
          <input
            v-model="ctx.draft.table"
            class="input compact mono"
            spellcheck="false"
            data-autofocus
          >
          <span class="subtle">in {{ ctx.draft.schema }}</span>
        </label>
        <button
          v-else
          type="button"
          class="icon-btn"
          :aria-label="`Read the ${what} again`"
          :title="`Read the ${what} again`"
          @click="reload"
        >
          <Icon name="refresh" />
        </button>
      </div>

      <p
        v-if="ctx.readOnly"
        class="notice tone-neutral bar"
      >
        This connection is read-only: the design is shown, nothing can be saved.
      </p>
      <p
        v-if="banner"
        class="notice bar"
        :class="`tone-${banner.tone}`"
        role="status"
      >
        {{ banner.text }}
        <button
          type="button"
          class="icon-btn dismiss"
          aria-label="Dismiss"
          @click="banner = null"
        >
          <Icon name="close" />
        </button>
      </p>
      <ul
        v-if="saveErrors.length > 0"
        class="notice tone-danger bar errors"
        role="alert"
        aria-label="Fix before saving"
      >
        <li
          v-for="e in saveErrors"
          :key="e"
        >
          {{ e }}
        </li>
      </ul>

      <div
        class="panel"
        role="tabpanel"
      >
        <ColumnsSection
          v-if="section === 'columns'"
          :key="`c${loadCount}`"
        />
        <IndexesSection
          v-else-if="section === 'indexes'"
          :key="`i${loadCount}.${focusCount}`"
        />
        <ConstraintsSection
          v-else-if="section === 'constraints'"
          :key="`k${loadCount}.${focusCount}`"
        />
        <PropertiesSection
          v-else-if="section === 'properties'"
          :key="`o${loadCount}`"
        />
        <PartitionBySection
          v-else-if="ctx.draft.isNew"
          :key="`b${loadCount}`"
        />
        <PartitionsSection
          v-else
          :key="`p${loadCount}.${focusCount}`"
        />
      </div>

      <footer
        v-if="section !== 'partitions' || dirty || ctx.draft.isNew"
        class="footer"
      >
        <span
          class="pill"
          :class="dirty ? 'tone-warning' : 'tone-neutral'"
        ><span class="dot" />{{ summary?.total ?? 0 }} {{ summary?.total === 1 ? 'change' : 'changes' }}</span>
        <span class="muted">{{ footerText || 'No unsaved changes' }}</span>
        <span class="spacer" />
        <button
          type="button"
          class="btn compact"
          :disabled="!dirty"
          @click="discard"
        >
          Discard
        </button>
        <button
          type="button"
          class="btn compact"
          :disabled="!dirty || planning"
          @click="preview"
        >
          Preview SQL
        </button>
        <button
          type="button"
          class="btn compact primary"
          :disabled="!dirty || planning || ctx.readOnly"
          @click="preview"
        >
          {{ ctx.draft.isNew ? (ctx.draft.partitionBy.enabled ? 'Create partitioned table' : 'Create table') : 'Save' }}
        </button>
      </footer>
    </template>

    <PlanReview
      v-if="review.open"
      :title="review.title"
      :steps="review.steps"
      :busy="review.busy"
      :message="review.message"
      apply-label="Apply"
      @apply="apply"
      @close="closeReview"
    />
  </div>
</template>

<style scoped>
.designer {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--surface);
}

.state {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 12px;
  padding: 24px;
}

.sections {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 4px;
  height: 40px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border);
}

.section-tab {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 40px;
  padding: 0 10px;
  border: 0;
  border-bottom: 2px solid transparent;
  background: transparent;
  color: var(--text-muted);
  font-size: 13px;
  cursor: pointer;
}

.section-tab[aria-selected='true'] {
  border-bottom-color: var(--accent);
  color: var(--text);
  font-weight: 500;
}

.badge {
  min-width: 18px;
  padding: 0 5px;
  border-radius: 9px;
  background: var(--surface-raised);
  color: var(--text-muted);
  font-size: 11px;
  text-align: center;
}

.badge.changed {
  background: var(--warning-tint);
  color: var(--warning-text);
}

.spacer {
  flex: 1;
}

.bar {
  flex-shrink: 0;
  align-items: center;
  margin: 8px 12px 0;
}

.dismiss {
  width: 20px;
  height: 20px;
  margin-left: auto;
}

.errors {
  flex-direction: column;
  padding-left: 28px;
}

.panel {
  flex: 1;
  min-height: 0;
}

.new-name {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12px;
}

.new-name .input {
  width: 220px;
  height: 28px;
}

.footer {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 10px;
  height: 48px;
  padding: 0 12px;
  border-top: 1px solid var(--border);
  font-size: 12px;
}
</style>
