<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from 'vue';
import { isDriverError } from '../../api/errors';
import { useQuerySave } from '../../composables/useQuerySave';
import { registerTabCommands } from '../../composables/useTabCommands';
import { useTabs } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import type { QueryTab } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { errorMark } from '../../lib/errorPosition';
import { isMacPlatform, READ_ONLY_BADGE_TIP, withKeys } from '../../lib/tips';
import EnvBadge from '../common/EnvBadge.vue';
import TabConnectionState from '../common/TabConnectionState.vue';
import RedisDbSelect from '../redis/RedisDbSelect.vue';
import Icon from '../common/Icon.vue';
import HistoryPanel from './HistoryPanel.vue';
import RedisCommandHelp from './RedisCommandHelp.vue';
import ResultsPanel from './ResultsPanel.vue';
import SqlEditor from './SqlEditor.vue';
import { canRun, runOnTab, runStep } from './runOnTab';

const props = defineProps<{ tab: QueryTab }>();

const tabs = useTabs();
const workspace = useWorkspace();

const profile = computed(() => workspace.connection(props.tab.connectionId));
const engine = computed(() => profile.value?.engine ?? props.tab.session?.engine ?? 'postgres');
const isRedis = computed(() => engine.value === 'redis');
/** The database the tab runs on: its session's, the one it was opened on ("run from another database"), else the profile's. */
const database = computed(() => props.tab.session?.database ?? props.tab.database ?? profile.value?.database ?? null);
const editor = ref<InstanceType<typeof SqlEditor> | null>(null);
const showHistory = ref(false);

/** Redis console help: the command on the cursor's line (F1 or the Help button). */
const help = ref<{ command: string | null } | null>(null);
function showHelp(command: string | null = editor.value?.commandAtCursor() ?? null): void {
  help.value = { command };
}

const sql = computed({
  get: () => props.tab.sql,
  set: (value: string) => {
    // The tab object lives in shared reactive state; the editor writes its text back here.
    const tab = tabs.tabs.find((t) => t.id === props.tab.id);
    if (tab?.kind === 'query') tab.sql = value;
  },
});

/** Cancel asks the server to stop; when that fails, say so instead of leaving the user waiting. */
async function cancelRun(): Promise<void> {
  try {
    await tabs.cancelQuery(props.tab.id);
  } catch (error: unknown) {
    useToasts().pushError(error, 'Could not cancel the statement');
  }
}

/**
 * Runs the selection (else the whole editor). A tab that is not connected
 * connects again first and then runs it, once (`runOnTab`).
 */
function run(text?: string, offset?: number): void {
  const target = text === undefined ? editor.value?.runTarget() : { text, offset: offset ?? 0 };
  if (!target) return;
  const deps = {
    find: (id: string) => {
      const tab = tabs.tabs.find((t) => t.id === id);
      return tab?.kind === 'query' ? tab : undefined;
    },
    reconnect: tabs.reconnect,
    runQuery: tabs.runQuery,
  };
  void runOnTab(deps, props.tab.id, target, () => editor.value?.runTarget() ?? null);
}

/** Run is off only while it cannot start: a statement is running, the tab is connecting, or its connection is gone. */
const runEnabled = computed(() => canRun(props.tab, profile.value !== null));

// Error position: a 1-based character position in the submitted text.

/** Computed from the run alone, so typing never re-marks the error or moves the cursor. */
const errorRange = computed(() => {
  const run = props.tab.run;
  const error = run?.error;
  if (!run || !error || !isDriverError(error)) return null;
  return errorMark(run.id, error, run.doc, run.text, run.offset);
});

const errorLocation = computed(() => (errorRange.value ? `line ${errorRange.value.line}, column ${errorRange.value.column}` : null));

// Saving

const dirty = computed(() => props.tab.savedQuery !== null && props.tab.savedQuery.sql !== props.tab.sql);

const querySave = useQuerySave();
const saving = computed(() => querySave.saving.has(props.tab.id));

/** Save and Save as share one flight per tab: a second Cmd+S while saving joins the first. */
function save(): void {
  void querySave.saveQueryTab(props.tab.id, 'save');
}

function saveAs(): void {
  void querySave.saveQueryTab(props.tab.id, 'saveAs');
}

/** Tooltips of the toolbar; Cancel has its shortcut only in the macOS menu bar. */
const runTip = computed(() => withKeys(runStep(props.tab) === 'connect' ? 'Connect again and run' : 'Run', 'CmdOrCtrl+Enter'));
const cancelTip = withKeys('Cancel the statement', isMacPlatform() ? 'CmdOrCtrl+.' : null);
const saveAsTip = withKeys('Save as a new saved query', 'CmdOrCtrl+Shift+S');
const saveTip = computed(() => (props.tab.savedQuery ? withKeys('Save', 'CmdOrCtrl+S') : withKeys('Save as a saved query', 'CmdOrCtrl+S')));

function onKeydown(event: KeyboardEvent): void {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 's') {
    event.preventDefault();
    if (event.shiftKey) saveAs();
    else save();
  }
}

// The Query menu of the menu bar runs these same functions.
onBeforeUnmount(
  registerTabCommands(props.tab.id, {
    run: () => run(),
    cancel: () => void cancelRun(),
    save,
    saveAs,
    toggleHistory: () => {
      showHistory.value = !showHistory.value;
    },
  }),
);

function loadFromHistory(text: string): void {
  tabs.loadText(props.tab.id, text);
  editor.value?.focus();
}

// Editor and results split

const split = ref(45);
const container = ref<HTMLElement | null>(null);

function onSplitPointer(event: PointerEvent): void {
  const target = event.currentTarget as HTMLElement;
  target.setPointerCapture(event.pointerId);
  const move = (e: PointerEvent): void => {
    const rect = container.value?.getBoundingClientRect();
    if (!rect) return;
    split.value = Math.min(85, Math.max(15, ((e.clientY - rect.top) / rect.height) * 100));
  };
  const up = (): void => {
    target.removeEventListener('pointermove', move);
    target.removeEventListener('pointerup', up);
    target.removeEventListener('pointercancel', up);
    target.removeEventListener('lostpointercapture', up);
  };
  target.addEventListener('pointermove', move);
  target.addEventListener('pointerup', up);
  // A cancelled pointer (touch interrupted, capture lost) ends the drag too.
  target.addEventListener('pointercancel', up);
  target.addEventListener('lostpointercapture', up);
}

function onSplitKey(event: KeyboardEvent): void {
  if (event.key === 'ArrowUp') split.value = Math.max(15, split.value - 5);
  else if (event.key === 'ArrowDown') split.value = Math.min(85, split.value + 5);
  else return;
  event.preventDefault();
}

const exportName = computed(() => (props.tab.savedQuery?.name ?? profile.value?.name ?? 'result').replace(/[^\w.-]+/g, '_'));
</script>

<template>
  <div
    class="query"
    @keydown="onKeydown"
  >
    <div
      class="toolbar"
      role="toolbar"
      :aria-label="`${tab.title} actions`"
    >
      <button
        v-if="!tab.running"
        type="button"
        class="btn primary compact"
        :disabled="!runEnabled"
        :title="runTip"
        @click="run()"
      >
        <Icon
          name="play"
          :size="12"
        />Run
      </button>
      <button
        v-else
        type="button"
        class="btn compact"
        :title="cancelTip"
        @click="cancelRun"
      >
        <Icon
          name="stop"
          :size="12"
        />Cancel
      </button>
      <button
        type="button"
        class="btn compact"
        :disabled="saving || !profile"
        :title="saveTip"
        @click="save"
      >
        <Icon name="save" />{{ tab.savedQuery ? 'Save' : 'Save as…' }}
      </button>
      <button
        v-if="tab.savedQuery"
        type="button"
        class="btn compact"
        :disabled="saving || !profile"
        :title="saveAsTip"
        @click="saveAs"
      >
        Save as…
      </button>
      <button
        type="button"
        class="btn compact"
        :aria-pressed="showHistory"
        @click="showHistory = !showHistory"
      >
        <Icon name="history" />History
      </button>
      <button
        v-if="isRedis"
        type="button"
        class="btn compact"
        title="Help for the command on the cursor's line (F1)"
        :aria-pressed="help !== null"
        @click="help === null ? showHelp() : (help = null)"
      >
        <Icon name="info" />Help
      </button>
      <span class="spacer" />
      <span
        v-if="dirty"
        class="subtle"
      >Unsaved changes</span>
      <span
        v-if="tab.session?.readOnly"
        class="pill tone-neutral"
        :title="READ_ONLY_BADGE_TIP"
      ><Icon
        name="lock"
        :size="11"
      />Read only</span>
      <span
        v-if="database && !isRedis"
        class="subtle mono"
      >{{ database }}</span>
      <RedisDbSelect
        v-if="isRedis"
        :tab="tab"
      />
      <EnvBadge
        v-if="profile"
        :environment="profile.environment"
      />
    </div>

    <div class="body">
      <div
        ref="container"
        class="main"
      >
        <div
          class="editor-pane"
          :style="{ height: `${split}%` }"
        >
          <SqlEditor
            ref="editor"
            v-model="sql"
            :engine="engine"
            :revision="tab.revision"
            :error-range="errorRange"
            :label="isRedis ? `Redis commands for ${tab.title}, one per line` : `SQL for ${tab.title}`"
            @run="run"
            @help="showHelp"
          />
        </div>
        <div
          class="splitter"
          role="separator"
          aria-orientation="horizontal"
          aria-label="Resize editor and results"
          :aria-valuenow="Math.round(split)"
          aria-valuemin="15"
          aria-valuemax="85"
          tabindex="0"
          @pointerdown="onSplitPointer"
          @keydown="onSplitKey"
        />
        <div class="results-pane">
          <TabConnectionState
            v-if="tab.status !== 'ready'"
            :tab="tab"
            compact
          />
          <ResultsPanel
            v-else
            :run="tab.run"
            :running="tab.running"
            :error-location="errorLocation"
            :export-name="exportName"
            :engine="engine"
            :connection-id="tab.connectionId"
          />
        </div>
      </div>
      <RedisCommandHelp
        v-if="isRedis && help"
        :session-id="tab.session?.id ?? null"
        :command="help.command"
        @close="help = null"
      />
      <HistoryPanel
        v-if="showHistory"
        :connection-id="tab.connectionId"
        :refresh-key="tab.run?.startedAt ?? 0"
        @load="loadFromHistory"
        @close="showHistory = false"
      />
    </div>
  </div>
</template>

<style scoped>
.query {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}

.toolbar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border);
  background: var(--surface);
  font-size: 12px;
}

.spacer {
  flex: 1;
}

.body {
  display: flex;
  flex: 1;
  min-height: 0;
}

.main {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}

.editor-pane {
  min-height: 0;
}

.splitter {
  flex-shrink: 0;
  height: 5px;
  border-top: 1px solid var(--border);
  background: var(--surface-raised);
  cursor: row-resize;
}

.splitter:focus-visible {
  outline: none;
  background: var(--accent);
}

.results-pane {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
}
</style>
