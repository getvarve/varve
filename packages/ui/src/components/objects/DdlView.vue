<script setup lang="ts">
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
import { computed, onBeforeUnmount, onMounted, ref, useId, watch } from 'vue';
import { catalogDdl } from '../../api/catalog';
import { ddlApply } from '../../api/ddl';
import type { DdlApplied } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import { useDialogs } from '../../composables/useDialogs';
import { useToasts } from '../../composables/useToasts';
import { latestOnly } from '../../lib/async';
import { applyProblem } from '../../lib/ddlApply';
import type { ApplyProblem } from '../../lib/ddlApply';
import { headerCollationDatabase, outline, sessionHeader, withoutComments } from '../../lib/ddlText';
import { runWithConfirmation } from '../../lib/destructive';
import { formatDuration } from '../../lib/values';
import Icon from '../common/Icon.vue';
import { useNotices } from '../notices/useNotices';
import SqlCode from './SqlCode.vue';
import { displayName } from './types';
import type { DdlViewProps } from './types';

/**
 * Read-only DDL of one object (design canvas DDLView): dialect
 * highlighting, an outline, copy, save to a file and "Open in editor".
 * "Edit and run" makes the text editable and runs it with `ddl_apply`
 * (confirmation, read-only, open-transaction and collation-header rules of
 * the contract).
 */
const props = withDefaults(defineProps<DdlViewProps>(), { reportError: null, reportDirty: null });
const emit = defineEmits<{ openSql: [sql: string]; changed: []; reconnect: [] }>();

const toasts = useToasts();
const original = ref('');
const text = ref('');
const loading = ref(false);
const loadError = ref<string | null>(null);
const includeComments = ref(true);
const editing = ref(false);
const running = ref(false);
const result = ref<DdlApplied | null>(null);
const problem = ref<ApplyProblem | null>(null);
const cursor = ref({ line: 1, column: 1 });
const code = ref<InstanceType<typeof SqlCode> | null>(null);
const latest = latestOnly();
const outlineId = useId();

const engine = computed(() => props.session.engine);
const isPostgres = computed(() => engine.value === 'postgres');
const shown = computed(() => (editing.value || includeComments.value || !isPostgres.value ? text.value : withoutComments(text.value, engine.value)));
const entries = computed(() => outline(shown.value, engine.value));
const header = computed(() => (isPostgres.value ? null : sessionHeader(text.value)));
const collationDatabase = computed(() => (isPostgres.value ? null : headerCollationDatabase(text.value)));
const lines = computed(() => shown.value.split('\n').length);
const edited = computed(() => text.value !== original.value);
const name = computed(() => displayName(props.object, engine.value));
watch(
  () => editing.value && edited.value,
  (dirty) => props.reportDirty?.(dirty),
);
onBeforeUnmount(() => props.reportDirty?.(false));

async function load(): Promise<void> {
  const token = latest.next();
  loading.value = true;
  loadError.value = null;
  try {
    const ddl = await catalogDdl(props.session.id, props.object);
    if (!latest.isCurrent(token)) return;
    original.value = ddl;
    text.value = ddl;
    editing.value = false;
    result.value = null;
    problem.value = null;
  } catch (error: unknown) {
    props.reportError?.(error);
    if (!latest.isCurrent(token)) return;
    const p = presentError(toCmdError(error));
    loadError.value = p.detail ? `${p.message} (${p.detail})` : p.message;
  } finally {
    if (latest.isCurrent(token)) loading.value = false;
  }
}

onMounted(load);
watch(
  () => [props.session.id, props.object.kind, props.object.database, props.object.schema, props.object.parent, props.object.name, props.object.signature],
  load,
);

async function copy(): Promise<void> {
  try {
    await navigator.clipboard.writeText(shown.value);
    toasts.push('success', 'Copied the DDL');
  } catch {
    toasts.push('warning', 'Could not copy to the clipboard');
  }
}

async function saveFile(): Promise<void> {
  try {
    const path = await saveFileDialog({
      title: `Save DDL of ${name.value}`,
      defaultPath: `${props.object.name}.sql`,
      filters: [{ name: 'SQL', extensions: ['sql'] }],
    });
    if (!path) return;
    await writeTextFile(path, shown.value);
    toasts.push('success', 'Saved the DDL', path);
  } catch (error: unknown) {
    toasts.pushError(error, 'Could not save the DDL');
  }
}

function startEditing(): void {
  // Edit the text as shown, so a hidden COMMENT ON is not run unseen.
  text.value = shown.value;
  original.value = shown.value;
  includeComments.value = true;
  editing.value = true;
  result.value = null;
  problem.value = null;
}

function stopEditing(): void {
  void load();
}

async function run(): Promise<void> {
  if (running.value || props.session.readOnly) return;
  running.value = true;
  problem.value = null;
  result.value = null;
  // MySQL/MariaDB: the object's database, needed by a collation_database header.
  const database = isPostgres.value ? null : props.object.database;
  try {
    const applied = await runWithConfirmation(
      (confirmed) => ddlApply(props.session.id, text.value, database, confirmed),
      (request) => useDialogs().askDestructive({ connectionName: props.connectionName, environment: request.environment, statements: request.statements }),
    );
    if (applied === null) return;
    result.value = applied;
    original.value = text.value;
    emit('changed');
    if (applied.warnings.length > 0) void useNotices().refreshRestores();
  } catch (error: unknown) {
    props.reportError?.(error);
    problem.value = applyProblem(error);
    emit('changed');
    // A failed run can leave a collation restore pending: show its banner at once.
    void useNotices().refreshRestores();
  } finally {
    running.value = false;
  }
}

async function act(): Promise<void> {
  const action = problem.value?.action;
  if (action === 'reconnect') emit('reconnect');
  else if (action === 'refresh') await load();
  else if (action === 'retry' || action === 'replan' || action === 'rerun') await run();
}
</script>

<template>
  <div class="ddl">
    <div
      class="toolbar"
      role="toolbar"
      aria-label="DDL actions"
    >
      <button
        type="button"
        class="btn compact"
        :disabled="loading || shown === ''"
        @click="copy"
      >
        <Icon name="copy" />Copy
      </button>
      <button
        type="button"
        class="btn compact"
        :disabled="loading || shown === ''"
        @click="saveFile"
      >
        <Icon name="download" />Save as file
      </button>
      <button
        type="button"
        class="btn compact"
        :disabled="loading || shown === ''"
        @click="emit('openSql', shown)"
      >
        <Icon name="terminal" />Open in editor
      </button>
      <span
        v-if="isPostgres && !editing"
        class="divider"
      />
      <label
        v-if="isPostgres && !editing"
        class="check"
      >
        <input
          v-model="includeComments"
          type="checkbox"
        >Include comments
      </label>
      <span class="grow" />
      <button
        type="button"
        class="icon-btn"
        aria-label="Reload the DDL"
        :disabled="loading || running"
        @click="load"
      >
        <Icon name="refresh" />
      </button>
      <template v-if="!editing">
        <span class="subtle small">Read only</span>
        <button
          type="button"
          class="btn compact"
          :disabled="loading || !!loadError || session.readOnly"
          :title="session.readOnly ? 'The connection is read only' : undefined"
          @click="startEditing"
        >
          Edit and run
        </button>
      </template>
      <template v-else>
        <button
          type="button"
          class="btn compact ghost"
          :disabled="running"
          @click="stopEditing"
        >
          {{ edited ? 'Discard edits' : 'Done' }}
        </button>
        <button
          type="button"
          class="btn compact primary"
          :disabled="running || text.trim() === ''"
          @click="run"
        >
          <Icon name="play" />{{ running ? 'Running' : 'Run' }}
        </button>
      </template>
    </div>

    <div
      v-if="header && header.length > 0"
      class="notice tone-info banner"
      role="note"
    >
      <Icon name="info" />
      <div>
        <strong>Session header.</strong>
        This object keeps the settings of the session that created it; running the DDL applies them for this run only:
        <span
          v-for="[key, value] in header"
          :key="key"
          class="mono setting"
        >{{ key }}={{ value }}</span>
        <p
          v-if="collationDatabase"
          class="note"
        >
          It also changes the default collation of database <span class="mono">{{ object.database }}</span> to
          <span class="mono">{{ collationDatabase }}</span> while it runs, for every client, and restores it afterwards. You confirm
          that before it runs.
        </p>
      </div>
    </div>

    <div
      v-if="result"
      :class="['notice', 'banner', result.warnings.length > 0 ? 'tone-warning' : 'tone-success']"
      :role="result.warnings.length > 0 ? 'alert' : 'status'"
    >
      <Icon :name="result.warnings.length > 0 ? 'warning' : 'check'" />
      <div>
        <strong>Ran in {{ formatDuration(result.durationMs) }}.</strong>
        <span v-if="result.collationDatabase"> Ran with database collation {{ result.collationDatabase }}.</span>
        <p
          v-for="w in result.warnings"
          :key="w"
          class="note"
        >
          {{ w }}
        </p>
        <p
          v-for="(n, i) in result.output.notices"
          :key="i"
          class="note muted"
        >
          {{ n.severity }}: {{ n.message }}
        </p>
      </div>
    </div>

    <div
      v-if="problem"
      class="notice tone-danger banner"
      role="alert"
    >
      <Icon name="warning" />
      <div class="grow">
        <strong>{{ problem.title }}</strong>
        <p class="note">
          {{ problem.message }}
        </p>
        <p
          v-if="problem.detail"
          class="note muted"
        >
          {{ problem.detail }}
        </p>
        <p
          v-if="problem.hint"
          class="note muted"
        >
          {{ problem.hint }}
        </p>
      </div>
      <button
        v-if="problem.action !== 'none'"
        type="button"
        class="btn compact"
        :disabled="running"
        @click="act"
      >
        {{ problem.action === 'replan' ? 'Run again' : problem.actionLabel }}
      </button>
    </div>

    <div class="main">
      <p
        v-if="loading"
        class="muted state"
        role="status"
      >
        Loading the DDL
      </p>
      <div
        v-else-if="loadError"
        class="notice tone-danger state"
        role="alert"
      >
        <Icon name="warning" /><span class="grow">{{ loadError }}</span>
        <button
          type="button"
          class="btn compact"
          @click="load"
        >
          Retry
        </button>
      </div>
      <SqlCode
        v-else-if="editing"
        ref="code"
        v-model="text"
        :engine="engine"
        :label="`DDL for ${name}, editable`"
        class="code"
        @cursor="(l, c) => (cursor = { line: l, column: c })"
      />
      <SqlCode
        v-else
        ref="code"
        :model-value="shown"
        :engine="engine"
        :label="`DDL for ${name}`"
        readonly
        class="code"
        @cursor="(l, c) => (cursor = { line: l, column: c })"
      />
      <nav
        v-if="!loading && !loadError"
        :aria-labelledby="outlineId"
        class="outline"
      >
        <div
          :id="outlineId"
          class="label"
        >
          Outline
        </div>
        <button
          v-for="entry in entries"
          :key="entry.group"
          type="button"
          class="entry"
          @click="code?.goToLine(entry.line)"
        >
          <span>{{ entry.label }}<span
            v-if="entry.count > 1"
            class="muted"
          > ({{ entry.count }})</span></span>
          <span class="mono subtle">L{{ entry.line }}</span>
        </button>
        <p
          v-if="entries.length === 0"
          class="muted small"
        >
          No statements.
        </p>
      </nav>
    </div>
    <div class="status">
      <span class="mono">{{ lines }} lines · Ln {{ cursor.line }}, Col {{ cursor.column }}</span>
    </div>
  </div>
</template>

<style scoped>
.ddl {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--surface);
}

.toolbar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  min-height: 48px;
  padding: 8px 16px;
  border-bottom: 1px solid var(--border);
}

.divider {
  width: 1px;
  height: 20px;
  margin: 0 4px;
  background: var(--border-inner);
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
}

.check input {
  margin: 0;
  accent-color: var(--accent);
}

.grow {
  flex-grow: 1;
}

.small {
  font-size: 12px;
}

.banner {
  margin: 8px 16px 0;
  align-items: flex-start;
}

.note {
  margin: 4px 0 0;
  overflow-wrap: anywhere;
}

.setting {
  margin-left: 6px;
}

.main {
  display: flex;
  flex-grow: 1;
  min-height: 0;
  margin-top: 8px;
}

.code {
  flex-grow: 1;
  min-width: 0;
  border: 0;
  border-top: 1px solid var(--border-inner);
  border-radius: 0;
}

.outline {
  display: flex;
  flex-direction: column;
  gap: 2px;
  width: 232px;
  flex-shrink: 0;
  padding: 12px;
  border-left: 1px solid var(--border-inner);
  border-top: 1px solid var(--border-inner);
  overflow-y: auto;
}

.outline .label {
  display: flex;
  align-items: center;
  height: 24px;
}

.entry {
  display: flex;
  align-items: center;
  justify-content: space-between;
  height: 28px;
  padding: 0 8px;
  border: 0;
  border-radius: var(--radius-control);
  background: transparent;
  color: var(--text);
  font-size: 13px;
  text-align: left;
  cursor: pointer;
}

.entry:hover {
  background: var(--accent-tint);
}

.state {
  margin: 16px;
  flex-grow: 1;
  align-self: flex-start;
}

.status {
  display: flex;
  justify-content: flex-end;
  height: 24px;
  align-items: center;
  padding: 0 12px;
  border-top: 1px solid var(--border-inner);
  font-size: 12px;
  color: var(--text-muted);
}

@media (max-width: 720px) {
  .outline {
    display: none;
  }
}
</style>
