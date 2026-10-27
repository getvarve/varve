<script setup lang="ts">
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
import { computed, defineAsyncComponent, ref, useId, watch } from 'vue';
import { noteExport, utf8Size } from '../../api/activity';
import { isDriverError, presentError, toCmdError } from '../../api/errors';
import type { CmdError, PresentedError } from '../../api/errors';
import type { Engine } from '../../api/records';
import type { Column, StatementResult, Value } from '../../api/sessions';
import type { RunResult } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import { formatTime } from '../../lib/display';
import { countPlaceholders, placeholderNotice, toCsv, toJson } from '../../lib/exportResult';
import { describePartial } from '../../lib/partialRun';
import { presentRedisFailure } from '../../lib/redisErrors';
import { formatCount, formatDuration, fullText, needsViewer } from '../../lib/values';
import Icon from '../common/Icon.vue';

const props = defineProps<{
  run: RunResult | null;
  running: boolean;
  /** "line 3, column 14" for the error position, when there is one. */
  errorLocation: string | null;
  exportName: string;
  /** Engine of the tab, for what can be done with an open transaction. */
  engine: Engine;
  /** The tab's connection, for the activity log entry of an export. */
  connectionId: string | null;
}>();

// AG Grid loads with the first result set.
const ResultGrid = defineAsyncComponent(() => import('./ResultGrid.vue'));

const toasts = useToasts();
const uid = useId();
type RowsResult = Extract<StatementResult, { kind: 'rows' }>;

const output = computed(() => props.run?.output ?? null);
const rowResults = computed(() =>
  (output.value?.results ?? [])
    .map((result, index) => ({ result, index }))
    .filter((r): r is { result: RowsResult; index: number } => r.result.kind === 'rows'),
);
const active = ref<number | 'messages'>('messages');

watch(
  () => props.run,
  (run) => {
    cell.value = null;
    const first = rowResults.value[0];
    // A failed run opens on its error, even when earlier statements returned rows.
    active.value = run?.output && first && !run.error ? 0 : 'messages';
  },
);

const activeResult = computed(() => (typeof active.value === 'number' ? (rowResults.value[active.value]?.result ?? null) : null));

const failure = computed<PresentedError | null>(() => {
  const error = props.run?.error;
  if (!error) return null;
  const cmdError: CmdError = isDriverError(error) ? { kind: 'session', error: { kind: 'driver', error } } : toCmdError(error);
  // Redis consoles explain read-only refusals (KEYS, SORT_RO, EVAL_RO) and MULTI / WATCH states like the key browser does.
  return props.engine === 'redis' ? presentRedisFailure(cmdError) : presentError(cmdError);
});

/** For a failed script: which statements completed before the failure and what became of their changes. */
const completedNote = computed<string[]>(() => {
  const error = props.run?.error;
  return error && isDriverError(error) && error.partial ? describePartial(error.partial, props.engine) : [];
});

function describe(result: StatementResult): string {
  if (result.kind === 'rows') return `${formatCount(result.rows.length)} ${result.rows.length === 1 ? 'row' : 'rows'}${result.truncated ? ' (truncated)' : ''}`;
  return result.rowsAffected !== null ? `${result.tag} · ${formatCount(result.rowsAffected)} rows affected` : result.tag;
}

// Tabs keyboard

const tabKeys = computed<(number | 'messages')[]>(() => [...rowResults.value.map((_, i) => i), 'messages']);

function onTabKey(event: KeyboardEvent): void {
  const keys = tabKeys.value;
  const at = keys.indexOf(active.value);
  let next: number;
  if (event.key === 'ArrowRight') next = (at + 1) % keys.length;
  else if (event.key === 'ArrowLeft') next = (at - 1 + keys.length) % keys.length;
  else return;
  event.preventDefault();
  const key = keys[next];
  if (key === undefined) return;
  active.value = key;
  document.getElementById(`${uid}-tab-${key}`)?.focus();
}

// Value panel

const cell = ref<{ column: Column; value: Value } | null>(null);
const showViewer = computed(() => cell.value !== null && needsViewer(cell.value.value));

function onCopied(what: string): void {
  if (what === 'oversized') {
    toasts.push('warning', 'Nothing copied', 'This entry was too large to load, so there is no value to copy.');
    return;
  }
  if (what) toasts.push('success', what === 'row' ? 'Row copied' : 'Value copied', '', 2000);
  else toasts.push('warning', 'Could not copy to the clipboard');
}

async function copyCell(): Promise<void> {
  if (!cell.value) return;
  try {
    await navigator.clipboard.writeText(fullText(cell.value.value));
    onCopied('cell');
  } catch {
    onCopied('');
  }
}

// Export

async function exportAs(format: 'csv' | 'json'): Promise<void> {
  const result = activeResult.value;
  if (!result) return;
  try {
    const path = await saveFileDialog({
      title: format === 'csv' ? 'Export result as CSV' : 'Export result as JSON',
      defaultPath: `${props.exportName}.${format}`,
      filters: [format === 'csv' ? { name: 'CSV', extensions: ['csv'] } : { name: 'JSON', extensions: ['json'] }],
    });
    if (!path) return;
    const text = format === 'csv' ? toCsv(result.columns, result.rows) : toJson(result.columns, result.rows);
    await writeTextFile(path, text);
    toasts.push('success', `Exported ${formatCount(result.rows.length)} rows`, path);
    await noteExport({
      connectionId: props.connectionId,
      objects: [props.exportName],
      fileName: path,
      format,
      sizeBytes: utf8Size(text),
      rows: result.rows.length,
    });
    const notice = placeholderNotice(countPlaceholders(result.rows), format);
    if (notice) toasts.push('warning', notice.title, notice.message, 12_000);
  } catch (error: unknown) {
    toasts.pushError(error, 'Export failed');
  }
}
</script>

<template>
  <section
    class="results"
    aria-label="Results"
  >
    <div class="bar">
      <div
        class="tabs"
        role="tablist"
        aria-label="Result sets"
        @keydown="onTabKey"
      >
        <button
          v-for="(r, i) in rowResults"
          :id="`${uid}-tab-${i}`"
          :key="r.index"
          type="button"
          role="tab"
          class="tab"
          :aria-selected="active === i"
          :aria-controls="`${uid}-panel`"
          :tabindex="active === i ? 0 : -1"
          @click="active = i"
        >
          Result {{ rowResults.length > 1 ? i + 1 : '' }}
          <span class="count">{{ formatCount(r.result.rows.length) }}</span>
        </button>
        <button
          :id="`${uid}-tab-messages`"
          type="button"
          role="tab"
          class="tab"
          :aria-selected="active === 'messages'"
          :aria-controls="`${uid}-panel`"
          :tabindex="active === 'messages' ? 0 : -1"
          @click="active = 'messages'"
        >
          Messages
          <span
            v-if="failure"
            class="err-dot"
            role="img"
            aria-label="Error"
          />
        </button>
      </div>
      <span class="spacer" />
      <span
        v-if="running"
        class="subtle"
        role="status"
      >Running…</span>
      <template v-else-if="activeResult">
        <button
          type="button"
          class="btn ghost compact"
          @click="exportAs('csv')"
        >
          <Icon name="download" />CSV
        </button>
        <button
          type="button"
          class="btn ghost compact"
          @click="exportAs('json')"
        >
          <Icon name="download" />JSON
        </button>
      </template>
    </div>

    <div
      v-if="activeResult?.truncated"
      class="truncated notice tone-warning"
      role="note"
    >
      <Icon name="warning" />
      Showing the first {{ formatCount(activeResult.rows.length) }} rows. The server returned more; add a LIMIT or narrow the query to see the rest.
    </div>

    <div
      :id="`${uid}-panel`"
      class="panel"
      role="tabpanel"
      :aria-labelledby="`${uid}-tab-${active}`"
    >
      <template v-if="activeResult">
        <ResultGrid
          :key="`${run?.startedAt}-${active}`"
          class="grid"
          :columns="activeResult.columns"
          :rows="activeResult.rows"
          :label="`Result ${typeof active === 'number' ? active + 1 : ''}`"
          @select="cell = $event"
          @copied="onCopied"
        />
        <aside
          v-if="showViewer && cell"
          class="viewer"
          aria-label="Value"
        >
          <header>
            <span class="mono">{{ cell.column.name }}</span>
            <span class="subtle mono">{{ cell.column.typeName }}</span>
            <span class="spacer" />
            <button
              type="button"
              class="icon-btn"
              aria-label="Copy value"
              @click="copyCell"
            >
              <Icon name="copy" />
            </button>
            <button
              type="button"
              class="icon-btn"
              aria-label="Close value panel"
              @click="cell = null"
            >
              <Icon name="close" />
            </button>
          </header>
          <pre class="mono">{{ fullText(cell.value) }}</pre>
        </aside>
      </template>

      <div
        v-else
        class="messages"
      >
        <p
          v-if="!run && !running"
          class="subtle"
        >
          Run a statement with Cmd or Ctrl+Enter. Select text first to run only that part.
        </p>
        <div
          v-if="failure"
          class="error-block"
          role="alert"
        >
          <div class="error-title">
            <Icon name="warning" />
            <strong>{{ failure.title }}</strong>
            <span
              v-if="failure.code"
              class="mono code"
            >{{ failure.code }}</span>
          </div>
          <p class="mono">
            {{ failure.message }}
          </p>
          <dl>
            <template v-if="failure.detail">
              <dt>Detail</dt>
              <dd class="mono">
                {{ failure.detail }}
              </dd>
            </template>
            <template v-if="failure.hint">
              <dt>Hint</dt>
              <dd>{{ failure.hint }}</dd>
            </template>
            <template v-if="errorLocation">
              <dt>Position</dt>
              <dd>{{ errorLocation }} (marked in the editor)</dd>
            </template>
            <template v-if="completedNote.length">
              <dt>Before it</dt>
              <dd>{{ completedNote.join(' ') }}</dd>
            </template>
          </dl>
        </div>
        <ul
          v-if="output"
          class="log mono"
        >
          <li
            v-for="(n, i) in output.notices"
            :key="`n-${i}`"
          >
            <span class="sev">{{ n.severity }}</span>{{ n.message }}
          </li>
          <li
            v-for="(r, i) in output.results"
            :key="`r-${i}`"
          >
            <span class="sev">{{ r.kind === 'rows' ? 'ROWS' : 'OK' }}</span>{{ describe(r) }}
          </li>
        </ul>
        <p
          v-if="run"
          class="subtle meta"
        >
          {{ run.output ? `${run.error ? 'Failed after' : 'Finished in'} ${formatDuration(run.output.elapsedMs)}` : 'Failed' }} · started {{ formatTime(run.startedAt) }}
        </p>
      </div>
    </div>
  </section>
</template>

<style scoped>
.results {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--surface);
}

.bar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 4px;
  height: 32px;
  padding: 0 8px;
  border-bottom: 1px solid var(--border);
  font-size: 12px;
}

.tabs {
  display: flex;
  gap: 2px;
  min-width: 0;
  overflow-x: auto;
}

.tab {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 24px;
  padding: 0 10px;
  border: 0;
  border-radius: var(--radius-control);
  background: transparent;
  color: var(--text-muted);
  font-size: 12px;
  white-space: nowrap;
  cursor: pointer;
}

.tab[aria-selected='true'] {
  background: var(--surface-raised);
  color: var(--text);
}

.count {
  font-family: var(--font-mono);
  color: var(--text-subtle);
}

.err-dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--danger);
}

.spacer {
  flex: 1;
}

.truncated {
  flex-shrink: 0;
  align-items: center;
  margin: 0;
  border-radius: 0;
}

.panel {
  display: flex;
  flex: 1;
  min-height: 0;
}

.grid {
  flex: 1;
  min-width: 0;
}

.viewer {
  display: flex;
  flex-direction: column;
  width: 360px;
  border-left: 1px solid var(--border);
}

.viewer header {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 32px;
  padding: 0 4px 0 12px;
  border-bottom: 1px solid var(--border-inner);
  font-size: 12px;
}

.viewer pre {
  flex: 1;
  margin: 0;
  padding: 12px;
  overflow: auto;
  font-size: 12px;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.messages {
  flex: 1;
  padding: 12px 16px;
  overflow: auto;
}

.messages p {
  margin: 0 0 8px;
}

.error-block {
  margin-bottom: 12px;
  padding: 12px;
  border-radius: var(--radius-panel);
  background: var(--danger-tint);
  color: var(--danger-text);
}

.error-title {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 6px;
}

.code {
  padding: 0 6px;
  border: 1px solid currentColor;
  border-radius: var(--radius-control);
  font-size: 11px;
}

.error-block p {
  margin: 0 0 6px;
  color: var(--text);
  white-space: pre-wrap;
}

.error-block dl {
  display: grid;
  grid-template-columns: 80px 1fr;
  gap: 4px 12px;
  margin: 0;
  font-size: 12px;
}

.error-block dd {
  margin: 0;
  color: var(--text);
  white-space: pre-wrap;
}

.log {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin: 0 0 8px;
  padding: 0;
  list-style: none;
  font-size: 12px;
}

.sev {
  display: inline-block;
  width: 72px;
  color: var(--text-subtle);
}

.meta {
  font-size: 12px;
}
</style>
