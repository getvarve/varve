<script setup lang="ts">
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, useId, watch } from 'vue';
import { MAX_PAYLOAD_BYTES } from '../../api/notify';
import { useToasts } from '../../composables/useToasts';
import { formatCount } from '../../lib/values';
import Icon from '../common/Icon.vue';
import {
  MAX_VIEW_NOTIFICATIONS,
  byteLength,
  channelProblem,
  countByChannel,
  exportEntries,
  matchesFilter,
  messageTime,
  needsQuotesInSql,
  notifySql,
  payloadProblem,
  statusText,
} from './notifyLog';
import type { LogEntry } from './notifyLog';
import { rate as messageRate } from './notifyPanel';
import { useNotify } from './useNotify';

/**
 * PostgreSQL LISTEN / NOTIFY for one tab: listen to channels on a dedicated
 * connection, watch notifications live (time, channel, sending backend,
 * payload), and send. The listener belongs to the tab (`tabId`), keeps running
 * while the panel is hidden, and ends with the tab's session.
 */
const props = defineProps<{ tabId: string; sessionId: string; connectionName: string; readOnly: boolean }>();

const uid = useId();
const notify = useNotify();
const toasts = useToasts();
const panel = shallowRef(notify.panel(props.tabId, props.sessionId));
const panelError = ref<string | null>(null);

// A reconnect gives the tab a new session; its old listener ended with the old one.
watch(
  () => [props.tabId, props.sessionId],
  () => {
    panel.value = notify.panel(props.tabId, props.sessionId);
    void load();
  },
);
async function load(): Promise<void> {
  panelError.value = await notify.refresh(props.tabId);
}
onMounted(load);

const state = computed(() => panel.value.state);
const channels = computed(() => state.value?.channels ?? []);
const status = computed(() => state.value?.status ?? 'stopped');

// Listen
const listenName = ref('');
const listenError = ref<string | null>(null);
const listenHint = computed(() =>
  listenName.value !== '' && needsQuotesInSql(listenName.value)
    ? `Exact name, case kept. In SQL write it quoted: NOTIFY "${listenName.value.replaceAll('"', '""')}".`
    : 'Exact channel name, case kept, listened to on a dedicated connection.',
);

async function listenTo(): Promise<void> {
  const name = listenName.value;
  listenError.value = channelProblem(name) ?? (await notify.listen(props.tabId, name));
  if (listenError.value === null) listenName.value = '';
}

async function unlisten(channel: string): Promise<void> {
  listenError.value = await notify.unlisten(props.tabId, channel);
  if (channelFilter.value === channel) channelFilter.value = null;
}

async function stopListener(): Promise<void> {
  listenError.value = await notify.stop(props.tabId);
}

async function listenAgain(): Promise<void> {
  // The channels of a stopped listener: listen to them again on a new one.
  const names = [...new Set(lastChannels.value)];
  for (const name of names) {
    const error = await notify.listen(props.tabId, name);
    if (error) {
      listenError.value = error;
      return;
    }
  }
  listenError.value = null;
}

// Channels to offer again after a stop (the backend reports none then).
const lastChannels = ref<string[]>([]);
watch(channels, (now) => {
  if (now.length > 0) lastChannels.value = [...now];
});

// Send
const sendChannel = ref('');
const sendPayload = ref('');
const sending = ref(false);
const sendError = ref<string | null>(null);
const lastSend = ref<{ channel: string; at: number; ms: number } | null>(null);
const payloadBytes = computed(() => byteLength(sendPayload.value));

async function send(): Promise<void> {
  const problem = channelProblem(sendChannel.value) ?? payloadProblem(sendPayload.value);
  if (problem) {
    sendError.value = problem;
    return;
  }
  sending.value = true;
  sendError.value = null;
  try {
    const outcome = await notify.send(props.tabId, sendChannel.value, sendPayload.value, props.connectionName);
    if ('error' in outcome) sendError.value = outcome.error;
    else if ('sent' in outcome) lastSend.value = { channel: outcome.sent.channel, at: Date.now(), ms: outcome.sent.durationMs };
  } finally {
    sending.value = false;
  }
}

async function copySql(): Promise<void> {
  try {
    await navigator.clipboard.writeText(notifySql(sendChannel.value, sendPayload.value));
    toasts.push('success', 'SQL copied', '', 2000);
  } catch {
    toasts.push('warning', 'Could not copy to the clipboard');
  }
}

// Notifications
const filter = ref('');
const channelFilter = ref<string | null>(null);
const selectedId = ref<number | null>(null);
const now = ref(Date.now());
let clock: ReturnType<typeof setInterval> | null = null;
onMounted(() => {
  clock = setInterval(() => {
    now.value = Date.now();
  }, 1000);
});
onBeforeUnmount(() => {
  if (clock) clearInterval(clock);
});

const shown = computed<LogEntry[]>(() => {
  void panel.value.version;
  return panel.value.entries.filter((e) => matchesFilter(e, filter.value, channelFilter.value));
});
const counts = computed(() => {
  void panel.value.version;
  return countByChannel(panel.value.entries);
});
const selected = computed(() => {
  void panel.value.version;
  const hit = panel.value.entries.find((e) => e.id === selectedId.value);
  return hit?.type === 'message' ? hit : null;
});
const rate = computed(() => {
  void panel.value.version;
  return messageRate(panel.value.times, now.value);
});

function resend(): void {
  const m = selected.value;
  if (!m) return;
  sendChannel.value = m.channel;
  sendPayload.value = m.payload;
}

async function copyPayload(): Promise<void> {
  if (!selected.value) return;
  try {
    await navigator.clipboard.writeText(selected.value.payload);
    toasts.push('success', 'Payload copied', '', 2000);
  } catch {
    toasts.push('warning', 'Could not copy to the clipboard');
  }
}

async function exportView(): Promise<void> {
  try {
    const path = await saveFileDialog({ defaultPath: 'notifications.json', filters: [{ name: 'JSON', extensions: ['json'] }] });
    if (!path) return;
    await writeTextFile(path, exportEntries(panel.value.entries));
    toasts.push('success', 'Notifications exported', path, 3000);
  } catch (e: unknown) {
    toasts.pushError(e, 'Could not export the notifications');
  }
}

function onRowKey(event: KeyboardEvent, index: number): void {
  const step = event.key === 'ArrowDown' ? 1 : event.key === 'ArrowUp' ? -1 : 0;
  if (step === 0) return;
  for (let i = index + step; i >= 0 && i < shown.value.length; i += step) {
    const target = shown.value[i];
    if (target?.type === 'message') {
      event.preventDefault();
      selectedId.value = target.id;
      document.getElementById(`${uid}-m-${target.id}`)?.focus();
      return;
    }
  }
}

function prettyPayload(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}
const pretty = ref(true);
</script>

<template>
  <div class="notify">
    <aside class="side">
      <section class="block">
        <h2>Listen</h2>
        <form
          class="row"
          novalidate
          @submit.prevent="listenTo"
        >
          <label
            class="visually-hidden"
            :for="`${uid}-listen`"
          >Channel</label>
          <input
            :id="`${uid}-listen`"
            v-model="listenName"
            class="input mono grow"
            spellcheck="false"
            placeholder="order_events"
            :aria-invalid="!!listenError"
            :aria-describedby="`${uid}-listen-help`"
          >
          <button
            type="submit"
            class="btn"
            :disabled="panel.busy"
          >
            Listen
          </button>
        </form>
        <p
          :id="`${uid}-listen-help`"
          class="help"
        >
          {{ listenHint }}
        </p>
        <p
          v-if="listenError"
          class="error"
          role="alert"
        >
          {{ listenError }}
        </p>
      </section>

      <section class="block">
        <div class="label-row label">
          <span>Channels ({{ channels.length }})</span><span class="plain">received</span>
        </div>
        <ul class="channels">
          <li
            v-for="c in channels"
            :key="c"
            class="channel"
            :class="{ active: channelFilter === c }"
          >
            <button
              type="button"
              class="channel-name mono grow"
              :aria-pressed="channelFilter === c"
              :title="channelFilter === c ? 'Show every channel' : 'Show only this channel'"
              @click="channelFilter = channelFilter === c ? null : c"
            >
              {{ c }}
            </button>
            <span class="mono subtle">{{ formatCount(String(counts.get(c) ?? 0)) }}</span>
            <button
              type="button"
              class="icon-btn"
              :aria-label="`Stop listening to ${c}`"
              @click="unlisten(c)"
            >
              <Icon name="close" />
            </button>
          </li>
          <li
            v-if="channels.length === 0"
            class="subtle empty-list"
          >
            Not listening to any channel.
          </li>
        </ul>
        <div
          v-if="status === 'reconnecting'"
          class="notice tone-warning"
          role="status"
        >
          <span>{{ statusText(state) }}<template v-if="state?.error">: {{ state.error.message }}</template>. Channels are listened to again once connected; notifications sent meanwhile are lost.</span>
        </div>
        <div
          v-else-if="status === 'stopped' && state?.error"
          class="notice tone-warning stopped"
          role="alert"
        >
          <span>The listener stopped: {{ state.error.message }}</span>
          <button
            v-if="lastChannels.length > 0"
            type="button"
            class="btn compact"
            :disabled="panel.busy"
            @click="listenAgain"
          >
            Listen again
          </button>
        </div>
        <p
          v-if="panelError"
          class="error"
          role="alert"
        >
          {{ panelError }}
        </p>
        <button
          v-if="status !== 'stopped'"
          type="button"
          class="btn ghost compact"
          @click="stopListener"
        >
          <Icon name="stop" />Stop listening
        </button>
      </section>

      <hr>

      <form
        class="block"
        novalidate
        @submit.prevent="send"
      >
        <h2>Send</h2>
        <label
          class="field-label"
          :for="`${uid}-sc`"
        >Channel</label>
        <input
          :id="`${uid}-sc`"
          v-model="sendChannel"
          class="input mono"
          spellcheck="false"
        >
        <label
          class="field-label"
          :for="`${uid}-sp`"
        >Payload <span class="subtle">({{ formatCount(String(payloadBytes)) }} of {{ formatCount(String(MAX_PAYLOAD_BYTES)) }} bytes)</span></label>
        <textarea
          :id="`${uid}-sp`"
          v-model="sendPayload"
          class="input mono payload-input"
          rows="3"
          spellcheck="false"
          :aria-invalid="payloadBytes > MAX_PAYLOAD_BYTES"
        />
        <p class="help">
          Sent with <span class="mono">pg_notify</span> on the listener's connection, outside this tab's transaction, so it is delivered at once.
        </p>
        <div class="row">
          <span
            v-if="lastSend"
            class="help"
          ><span class="dot" />Sent to {{ lastSend.channel }} at {{ messageTime(lastSend.at).slice(0, 8) }}</span>
          <span class="grow" />
          <button
            type="button"
            class="btn ghost compact"
            :disabled="sendChannel === ''"
            @click="copySql"
          >
            <Icon name="copy" />Copy SQL
          </button>
          <button
            type="submit"
            class="btn primary"
            :disabled="sending || readOnly"
            :title="readOnly ? 'This connection is read-only.' : undefined"
          >
            Send
          </button>
        </div>
        <p
          v-if="sendError"
          class="error"
          role="alert"
        >
          {{ sendError }}
        </p>
      </form>
    </aside>

    <section
      class="main"
      aria-label="Notifications"
    >
      <div class="toolbar">
        <h2>Notifications</h2>
        <span
          v-if="status === 'connected'"
          class="pill tone-success"
          :title="statusText(state)"
        ><span class="dot" />{{ panel.paused ? 'Paused' : 'Live' }} · {{ rate }}/s</span>
        <span
          v-else-if="status === 'reconnecting'"
          class="pill tone-warning"
        >Reconnecting</span>
        <span
          v-else
          class="pill tone-neutral"
        >Not listening</span>
        <span
          v-if="state?.backendPid"
          class="subtle small"
        >backend {{ state.backendPid }}</span>
        <span class="grow" />
        <label
          class="filter"
          :for="`${uid}-filter`"
        >
          <Icon name="search" />
          <input
            :id="`${uid}-filter`"
            v-model="filter"
            aria-label="Filter notifications"
            placeholder="Filter channel or payload"
          >
        </label>
        <button
          type="button"
          class="btn compact"
          @click="notify.pause(tabId, !panel.paused)"
        >
          <Icon :name="panel.paused ? 'play' : 'stop'" />{{ panel.paused ? `Resume (${panel.held.length})` : 'Pause' }}
        </button>
        <button
          type="button"
          class="btn ghost compact"
          @click="notify.clear(tabId); selectedId = null"
        >
          Clear
        </button>
        <button
          type="button"
          class="btn ghost compact"
          :disabled="panel.entries.length === 0"
          @click="exportView"
        >
          Export
        </button>
      </div>

      <div
        class="grid"
        role="table"
        aria-label="Received notifications"
      >
        <div
          class="row head"
          role="row"
        >
          <span role="columnheader">Time</span><span role="columnheader">Channel</span><span
            role="columnheader"
            class="num"
          >PID</span><span role="columnheader">Payload</span>
        </div>
        <div class="rows">
          <template
            v-for="(e, i) in shown"
            :key="e.id"
          >
            <div
              v-if="e.type === 'gap'"
              class="gap"
              role="row"
            >
              <span role="cell"><Icon name="warning" /> {{ messageTime(e.at) }} · {{ e.text }}</span>
            </div>
            <div
              v-else
              :id="`${uid}-m-${e.id}`"
              class="row mono"
              role="row"
              tabindex="0"
              :aria-selected="e.id === selectedId"
              @click="selectedId = e.id"
              @keydown.enter="selectedId = e.id"
              @keydown="onRowKey($event, i)"
            >
              <span
                role="cell"
                class="subtle"
              >{{ messageTime(e.receivedAt) }}</span>
              <span
                role="cell"
                class="ellipsis"
              >{{ e.channel }}</span>
              <span
                role="cell"
                class="num muted"
                :title="e.own ? 'Sent from this panel' : undefined"
              >{{ e.pid }}<template v-if="e.own"> (own)</template></span>
              <span
                role="cell"
                class="ellipsis muted"
                :title="e.lossy ? 'Not valid UTF-8: invalid bytes are shown as \uFFFD' : undefined"
              ><template v-if="e.lossy">[not UTF-8] </template>{{ e.payload === '' ? '(no payload)' : e.payload }}</span>
            </div>
          </template>
          <p
            v-if="shown.length === 0"
            class="empty subtle"
          >
            {{ panel.entries.length === 0 ? (status === 'connected' && channels.length > 0 ? 'Waiting for notifications…' : 'Listen to a channel to see its notifications.') : 'No notification matches the filter.' }}
          </p>
        </div>
      </div>

      <div
        v-if="selected"
        class="detail"
      >
        <div class="detail-bar">
          <span class="label">Selected notification</span>
          <span class="mono">{{ selected.channel }}</span>
          <span class="subtle">backend {{ selected.pid }}<template v-if="selected.own"> (this panel)</template> · {{ messageTime(selected.receivedAt) }} · {{ formatCount(String(byteLength(selected.payload))) }} bytes<template v-if="selected.lossy"> · not valid UTF-8, shown with replacement characters</template></span>
          <span class="grow" />
          <label class="check"><input
            v-model="pretty"
            type="checkbox"
          > Pretty JSON</label>
          <button
            type="button"
            class="btn compact"
            @click="copyPayload"
          >
            Copy payload
          </button>
          <button
            type="button"
            class="btn ghost compact"
            :disabled="readOnly"
            @click="resend"
          >
            Send again
          </button>
        </div>
        <pre class="mono payload">{{ pretty ? prettyPayload(selected.payload) : selected.payload }}</pre>
      </div>

      <div class="foot">
        Keeping the latest {{ formatCount(String(MAX_VIEW_NOTIFICATIONS)) }} of {{ formatCount(String(panel.received)) }} received notifications in view. Older ones are dropped, not stored.<template v-if="panel.dropped > 0">
          {{ formatCount(String(panel.dropped)) }} were dropped because the view fell behind.
        </template>
      </div>
    </section>
  </div>
</template>

<style scoped>
.notify {
  display: flex;
  height: 100%;
  min-height: 0;
}

.side {
  display: flex;
  flex-direction: column;
  flex-shrink: 0;
  gap: 14px;
  width: 360px;
  padding: 16px;
  overflow: auto;
  border-right: 1px solid var(--border);
  background: var(--surface);
}

h2 {
  margin: 0;
  font-size: 15px;
  font-weight: 500;
}

hr {
  width: 100%;
  margin: 0;
  border: 0;
  border-top: 1px solid var(--border-inner);
}

.block {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.grow {
  flex: 1;
  min-width: 0;
}

.help {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 0;
  font-size: 12px;
  color: var(--text-subtle);
}

.dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--success);
}

.error {
  margin: 0;
  font-size: 12px;
  color: var(--danger-text);
}

.label-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.plain {
  text-transform: none;
  letter-spacing: 0;
  font-weight: 400;
}

.channels {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.channel {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 36px;
  padding: 0 4px 0 8px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
  font-size: 12px;
}

.channel.active {
  border-color: var(--accent);
  background: var(--accent-tint);
}

.channel-name {
  overflow: hidden;
  padding: 0;
  border: 0;
  background: transparent;
  color: inherit;
  font-size: 12px;
  text-align: left;
  text-overflow: ellipsis;
  white-space: nowrap;
  cursor: pointer;
}

.empty-list {
  font-size: 12px;
}

.stopped {
  flex-direction: column;
  align-items: flex-start;
}

.field-label {
  font-size: 12px;
  color: var(--text-muted);
}

.payload-input {
  height: 72px;
  padding: 8px 10px;
  resize: vertical;
}

.main {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  background: var(--surface);
}

.toolbar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 10px;
  height: 44px;
  padding: 0 12px 0 16px;
  border-bottom: 1px solid var(--border-inner);
}

.small {
  font-size: 12px;
}

.filter {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 230px;
  height: 28px;
  padding: 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  color: var(--text-subtle);
}

.filter input {
  flex: 1;
  min-width: 0;
  border: 0;
  background: transparent;
  font-size: 12px;
  outline: none;
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
}

.compact {
  height: 28px;
  font-size: 12px;
}

.grid {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
  font-size: 12px;
}

.grid .row {
  display: grid;
  grid-template-columns: 104px 180px 96px minmax(0, 1fr);
  flex-shrink: 0;
  height: 24px;
  gap: 0;
}

.grid .row > span {
  padding: 0 10px;
}

.grid .head {
  height: 28px;
  border-bottom: 1px solid var(--border);
  background: var(--surface-raised);
  color: var(--text-muted);
}

.rows {
  flex: 1;
  min-height: 0;
  overflow: auto;
}

.rows .row {
  cursor: default;
}

.rows .row:focus {
  outline: none;
}

.rows .row:focus-visible,
.rows .row[aria-selected='true'] {
  background: var(--accent-tint);
  box-shadow: inset 2px 0 0 var(--accent);
}

.gap {
  display: flex;
  align-items: center;
  height: 24px;
  padding: 0 10px;
  border-top: 1px dashed var(--border);
  border-bottom: 1px dashed var(--border);
  color: var(--warning-text);
}

.ellipsis {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.num {
  text-align: right;
}

.empty {
  padding: 16px;
}

.detail {
  display: flex;
  flex-shrink: 0;
  flex-direction: column;
  height: 196px;
  border-top: 1px solid var(--border);
}

.detail-bar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 10px;
  height: 36px;
  padding: 0 12px 0 16px;
  border-bottom: 1px solid var(--border-inner);
  font-size: 12px;
}

.payload {
  flex: 1;
  min-height: 0;
  margin: 0;
  padding: 10px 16px;
  overflow: auto;
  background: var(--ground);
  font-size: 12px;
  line-height: 18px;
  white-space: pre-wrap;
  word-break: break-word;
}

.foot {
  flex-shrink: 0;
  padding: 4px 16px;
  border-top: 1px solid var(--border-inner);
  font-size: 11px;
  color: var(--text-subtle);
}
</style>
