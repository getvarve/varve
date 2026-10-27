<script setup lang="ts">
import { save as saveFileDialog } from '@tauri-apps/plugin-dialog';
import { writeTextFile } from '@tauri-apps/plugin-fs';
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, useId, watch } from 'vue';
import { redisPubsub, redisPublish } from '../../api/redis';
import type { PubSubNameKind, PubSubSnapshot } from '../../api/redis';
import { useDialogs } from '../../composables/useDialogs';
import { useTabs } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import { runWithConfirmation } from '../../lib/destructive';
import { textValue } from '../../lib/redisEdit';
import { redisErrorView } from '../../lib/redisErrors';
import {
  exportMessages,
  gapText,
  isGap,
  isGlob,
  matchesFilter,
  messageRate,
  messageTime,
  payloadDetail,
  payloadText,
  reasonParts,
} from '../../lib/redisPubSub';
import type { ReceivedMessage, ViewEntry } from '../../lib/redisPubSub';
import { cellText, formatBytes, formatCount } from '../../lib/values';
import Icon from '../common/Icon.vue';
import { namesOf, usePubSub } from './usePubSub';
import type { PubSubName } from './usePubSub';

/**
 * Redis Pub/Sub for one tab: subscribe to channels and patterns on a
 * dedicated connection, watch messages live, publish, and see the server's
 * active channels. Subscriptions belong to the tab (`tabId`) and survive the
 * panel being hidden; they end when the tab's session closes.
 */
const props = defineProps<{ tabId: string; sessionId: string; connectionName: string; readOnly: boolean }>();

const uid = useId();
const pubsub = usePubSub();
const toasts = useToasts();
const panel = shallowRef(pubsub.panel(props.tabId, props.sessionId));
// A reconnect gives the tab a new session: its old subscription ended with the old one.
watch(
  () => [props.tabId, props.sessionId],
  () => {
    panel.value = pubsub.panel(props.tabId, props.sessionId);
    void reattach();
  },
);
// After a window reload the backend may still run this session's subscriptions: take them over.
async function reattach(): Promise<void> {
  const error = await pubsub.reattach(props.tabId);
  if (error !== null) subError.value = error;
}
onMounted(() => void reattach());

// Subscribe
const subKind = ref<PubSubNameKind>('channel');
const KIND_LABELS: Record<PubSubNameKind, string> = { channel: 'Channel', pattern: 'Pattern', shardChannel: 'Shard channel' };
const subName = ref('');
const subError = ref<string | null>(null);
const globWarning = computed(() => subKind.value === 'channel' && isGlob(subName.value));

async function subscribe(): Promise<void> {
  const name = subName.value;
  if (name === '') {
    subError.value = `Enter a ${KIND_LABELS[subKind.value].toLowerCase()}.`;
    return;
  }
  subError.value = await pubsub.subscribe(props.tabId, namesOf([{ kind: subKind.value, name }]));
  if (subError.value === null) subName.value = '';
}

async function unsubscribe(target: PubSubName): Promise<void> {
  subError.value = await pubsub.unsubscribe(props.tabId, target);
}

async function resubscribe(): Promise<void> {
  subError.value = await pubsub.subscribe(props.tabId, namesOf(panel.value.names));
}

// Publish
const pubChannel = ref('');
const pubMessage = ref('');
const publishing = ref(false);
const pubError = ref<string | null>(null);
const lastPublish = ref<{ receivers: number; at: number } | null>(null);

async function publish(): Promise<void> {
  if (pubChannel.value === '') {
    pubError.value = 'Enter a channel.';
    return;
  }
  publishing.value = true;
  pubError.value = null;
  const sessionId = props.sessionId;
  try {
    const receivers = await runWithConfirmation(
      (confirmed) => redisPublish(sessionId, pubChannel.value, textValue(pubMessage.value), confirmed),
      (request) =>
        useDialogs().askDestructive({
          connectionName: props.connectionName,
          environment: request.environment,
          statements: request.statements,
        }),
    );
    if (receivers !== null) lastPublish.value = { receivers, at: Date.now() };
  } catch (e: unknown) {
    const view = redisErrorView(useTabs().reportTabError(props.tabId, e));
    pubError.value = view.hint ? `${view.message} ${view.hint}` : view.message;
  } finally {
    publishing.value = false;
  }
}

// Server channels
const snapshot = ref<PubSubSnapshot | null>(null);
const snapshotError = ref<string | null>(null);
async function loadSnapshot(): Promise<void> {
  try {
    snapshot.value = await redisPubsub(props.sessionId, null, 200);
    snapshotError.value = null;
  } catch (e: unknown) {
    snapshotError.value = redisErrorView(useTabs().reportTabError(props.tabId, e)).message;
  }
}
watch(() => props.sessionId, loadSnapshot);
onMounted(loadSnapshot);

// Messages
const filter = ref('');
const pretty = ref(true);
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

const shown = computed<ViewEntry[]>(() => {
  void panel.value.version;
  const list = panel.value.messages;
  return filter.value.trim() === '' ? [...list] : list.filter((m) => matchesFilter(m, filter.value));
});
const selected = computed<ReceivedMessage | null>(() => {
  void panel.value.version;
  const hit = panel.value.messages.find((m) => m.id === selectedId.value);
  return hit && !isGap(hit) ? hit : null;
});
/** Why the subscription ended or is being reopened, split from a CONFIG suggestion. */
const closedParts = computed(() => (panel.value.closed ? reasonParts(panel.value.closed) : null));
const reconnectParts = computed(() => (panel.value.reconnecting ? reasonParts(panel.value.reconnecting.reason) : null));

async function copyCommand(command: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(command);
    toasts.push('success', 'Command copied', '', 2000);
  } catch {
    toasts.push('warning', 'Could not copy to the clipboard');
  }
}
const rate = computed(() => {
  void panel.value.version;
  return messageRate(panel.value.times, now.value);
});
const live = computed(() => panel.value.subscriptionId !== null);

async function copyPayload(): Promise<void> {
  if (!selected.value) return;
  try {
    await navigator.clipboard.writeText(payloadText(selected.value));
    toasts.push('success', 'Payload copied', '', 2000);
  } catch {
    toasts.push('warning', 'Could not copy to the clipboard');
  }
}

function republish(): void {
  const m = selected.value;
  if (!m || m.truncated || m.payload.t !== 'text') return;
  pubChannel.value = m.channel;
  pubMessage.value = m.payload.v;
}

async function exportView(): Promise<void> {
  try {
    const path = await saveFileDialog({ defaultPath: 'pubsub-messages.json', filters: [{ name: 'JSON', extensions: ['json'] }] });
    if (!path) return;
    await writeTextFile(path, exportMessages(panel.value.messages));
    toasts.push('success', 'Messages exported', path, 3000);
  } catch (e: unknown) {
    toasts.pushError(e, 'Could not export the messages');
  }
}

function onRowKey(event: KeyboardEvent, index: number): void {
  const step = event.key === 'ArrowDown' ? 1 : event.key === 'ArrowUp' ? -1 : 0;
  if (step === 0) return;
  // Gap markers are not selectable: skip over them.
  let next = index + step;
  while (shown.value[next] && isGap(shown.value[next] as ViewEntry)) next += step;
  const target = shown.value[next];
  if (!target) return;
  event.preventDefault();
  selectedId.value = target.id;
  document.getElementById(`${uid}-m-${target.id}`)?.focus();
}
</script>

<template>
  <div class="pubsub">
    <aside class="side">
      <section class="block">
        <h2>Subscribe</h2>
        <div
          class="segmented"
          role="radiogroup"
          aria-label="Subscription type"
        >
          <button
            type="button"
            role="radio"
            :aria-checked="subKind === 'channel'"
            @click="subKind = 'channel'"
          >
            Channel
          </button>
          <button
            type="button"
            role="radio"
            :aria-checked="subKind === 'pattern'"
            @click="subKind = 'pattern'"
          >
            Pattern
          </button>
          <button
            type="button"
            role="radio"
            :aria-checked="subKind === 'shardChannel'"
            @click="subKind = 'shardChannel'"
          >
            Shard channel
          </button>
        </div>
        <form
          class="row"
          novalidate
          @submit.prevent="subscribe"
        >
          <label
            class="visually-hidden"
            :for="`${uid}-sub`"
          >{{ KIND_LABELS[subKind] }}</label>
          <input
            :id="`${uid}-sub`"
            v-model="subName"
            class="input mono grow"
            spellcheck="false"
            :placeholder="subKind === 'pattern' ? 'orders.*' : 'orders.created'"
            :aria-invalid="!!subError"
            :aria-describedby="`${uid}-sub-help`"
          >
          <button
            type="submit"
            class="btn"
            :disabled="panel.busy"
          >
            Subscribe
          </button>
        </form>
        <p
          :id="`${uid}-sub-help`"
          class="help"
        >
          <template v-if="subKind === 'shardChannel'">
            Sharded channel, sent as <span class="mono">SSUBSCRIBE</span> (Redis 7.0 and newer). Only
            <span class="mono">SPUBLISH</span> reaches it, never a plain <span class="mono">PUBLISH</span>.
          </template>
          <template v-else-if="subKind === 'pattern'">
            Glob syntax <span class="mono">* ? [abc]</span>, sent as <span class="mono">PSUBSCRIBE</span>.
          </template>
          <template v-else-if="globWarning">
            This name has glob characters but is subscribed literally. Choose Pattern to match many channels.
          </template>
          <template v-else>
            Exact channel name, sent as <span class="mono">SUBSCRIBE</span> on a dedicated connection.
          </template>
        </p>
        <p
          v-if="subError"
          class="error"
          role="alert"
        >
          {{ subError }}
        </p>
      </section>

      <section class="block">
        <div class="label-row label">
          <span>Active subscriptions ({{ panel.names.length }})</span><span class="plain">messages</span>
        </div>
        <ul class="subs">
          <li
            v-for="n in panel.names"
            :key="`${n.kind}:${n.name}`"
            class="sub"
          >
            <span class="kind">{{ n.kind === 'shardChannel' ? 'shard' : n.kind }}</span>
            <span
              class="mono grow name"
              :class="{ pending: !n.confirmed }"
              :title="n.confirmed ? undefined : 'Not confirmed by the server (yet)'"
            >{{ n.name }}</span>
            <span class="mono subtle">{{ formatCount(String(n.messages)) }}</span>
            <button
              type="button"
              class="icon-btn"
              :aria-label="`Unsubscribe from ${n.name}`"
              @click="unsubscribe(n)"
            >
              <Icon name="close" />
            </button>
          </li>
        </ul>
        <div
          v-if="panel.reconnecting && reconnectParts"
          class="notice tone-warning closed"
          role="status"
        >
          <strong>Reconnecting (attempt {{ panel.reconnecting.attempt }} of 5)</strong>
          <span>{{ reconnectParts.text }}</span>
          <span
            v-if="reconnectParts.command"
            class="command"
          ><code class="mono">{{ reconnectParts.command }}</code><button
            type="button"
            class="btn ghost compact"
            @click="copyCommand(reconnectParts.command)"
          >Copy</button></span>
          <span>Messages published until it is back are missed; the list marks the gap.</span>
        </div>
        <div
          v-if="panel.closed && closedParts"
          class="notice tone-warning closed"
          role="alert"
        >
          <strong>The subscription ended</strong>
          <span>{{ closedParts.text }}</span>
          <span
            v-if="closedParts.command"
            class="command"
          ><code class="mono">{{ closedParts.command }}</code><button
            type="button"
            class="btn ghost compact"
            @click="copyCommand(closedParts.command)"
          >Copy</button></span>
          <span>Messages sent since then are lost (Redis keeps none).</span>
          <button
            v-if="panel.names.length > 0"
            type="button"
            class="btn compact"
            :disabled="panel.busy"
            @click="resubscribe"
          >
            Subscribe again
          </button>
        </div>
        <p
          v-for="(e, i) in panel.errors"
          :key="i"
          class="error"
          role="alert"
        >
          {{ e }}
        </p>
      </section>

      <hr>

      <form
        class="block"
        novalidate
        @submit.prevent="publish"
      >
        <h2>Publish</h2>
        <label
          class="field-label"
          :for="`${uid}-pc`"
        >Channel</label>
        <input
          :id="`${uid}-pc`"
          v-model="pubChannel"
          class="input mono"
          spellcheck="false"
        >
        <label
          class="field-label"
          :for="`${uid}-pm`"
        >Message</label>
        <textarea
          :id="`${uid}-pm`"
          v-model="pubMessage"
          class="input mono message"
          rows="3"
          spellcheck="false"
        />
        <div class="row">
          <span
            v-if="lastPublish"
            class="help"
          ><span class="dot" />Last publish reached {{ lastPublish.receivers }} receiver{{ lastPublish.receivers === 1 ? '' : 's' }}, {{ messageTime(lastPublish.at).slice(0, 8) }}</span>
          <span class="grow" />
          <button
            type="submit"
            class="btn primary"
            :disabled="publishing || readOnly"
            :title="readOnly ? 'This connection is read-only.' : undefined"
          >
            Publish
          </button>
        </div>
        <p
          v-if="pubError"
          class="error"
          role="alert"
        >
          {{ pubError }}
        </p>
      </form>

      <hr>

      <section class="block">
        <div class="label-row label">
          <span>Server channels</span>
          <button
            type="button"
            class="btn ghost compact plain"
            @click="loadSnapshot"
          >
            <Icon name="refresh" />PUBSUB NUMSUB
          </button>
        </div>
        <p
          v-if="snapshotError"
          class="error"
        >
          {{ snapshotError }}
        </p>
        <ul
          v-else-if="snapshot"
          class="channels"
        >
          <li
            v-for="(c, i) in snapshot.channels"
            :key="i"
          >
            <span class="mono grow">{{ cellText(c.channel) }}</span><span class="subtle">{{ c.subscribers }}</span>
          </li>
          <li
            v-if="snapshot.channels.length === 0"
            class="subtle"
          >
            No channel has subscribers.
          </li>
          <li class="subtle">
            {{ snapshot.patternSubscriptions }} pattern subscription{{ snapshot.patternSubscriptions === 1 ? '' : 's' }}<template v-if="snapshot.truncated">
              · first 200 channels shown
            </template>
          </li>
          <template v-if="snapshot.shardChannels && snapshot.shardChannels.length > 0">
            <li class="label">
              Shard channels
            </li>
            <li
              v-for="(c, i) in snapshot.shardChannels"
              :key="`s${i}`"
            >
              <span class="mono grow">{{ cellText(c.channel) }}</span><span class="subtle">{{ c.subscribers }}</span>
            </li>
          </template>
        </ul>
      </section>
    </aside>

    <section
      class="main"
      aria-label="Messages"
    >
      <div class="toolbar">
        <h2>Messages</h2>
        <span
          v-if="panel.reconnecting"
          class="pill tone-warning"
        ><span class="dot" />Reconnecting · attempt {{ panel.reconnecting.attempt }} of 5</span>
        <span
          v-else-if="live"
          class="pill tone-success"
        ><span class="dot" />{{ panel.paused ? 'Paused' : 'Live' }} · {{ rate }} msg/s</span>
        <span
          v-else
          class="pill tone-neutral"
        >Not subscribed</span>
        <span class="grow" />
        <label
          class="filter"
          :for="`${uid}-filter`"
        >
          <Icon name="search" />
          <input
            :id="`${uid}-filter`"
            v-model="filter"
            aria-label="Filter messages"
            placeholder="Filter channel or payload"
          >
        </label>
        <label class="check"><input
          v-model="pretty"
          type="checkbox"
        > Pretty JSON</label>
        <button
          type="button"
          class="btn compact"
          @click="pubsub.setPaused(tabId, !panel.paused)"
        >
          <Icon :name="panel.paused ? 'play' : 'stop'" />{{ panel.paused ? `Resume (${panel.held.length})` : 'Pause' }}
        </button>
        <button
          type="button"
          class="btn ghost compact"
          @click="pubsub.clear(tabId); selectedId = null"
        >
          Clear
        </button>
        <button
          type="button"
          class="btn ghost compact"
          :disabled="panel.messages.length === 0"
          @click="exportView"
        >
          Export
        </button>
      </div>

      <div
        class="grid"
        role="table"
        aria-label="Received messages"
      >
        <div
          class="row head"
          role="row"
        >
          <span role="columnheader">Time</span><span role="columnheader">Channel</span><span role="columnheader">Pattern</span><span role="columnheader">Payload</span><span
            role="columnheader"
            class="num"
          >Size</span>
        </div>
        <div class="rows">
          <template
            v-for="(m, i) in shown"
            :key="m.id"
          >
            <div
              v-if="isGap(m)"
              class="gap"
              role="row"
            >
              <span
                role="cell"
                class="gap-text"
              ><Icon name="warning" />{{ gapText(m) }} {{ reasonParts(m.reason).text }}</span>
            </div>
            <div
              v-else
              :id="`${uid}-m-${m.id}`"
              class="row mono"
              role="row"
              tabindex="0"
              :aria-selected="m.id === selectedId"
              @click="selectedId = m.id"
              @keydown.enter="selectedId = m.id"
              @keydown="onRowKey($event, i)"
            >
              <span
                role="cell"
                class="subtle"
              >{{ messageTime(m.receivedAt) }}</span>
              <span
                role="cell"
                class="ellipsis"
              >{{ m.channel }}</span>
              <span
                role="cell"
                class="subtle ellipsis"
              >{{ m.pattern ?? '' }}</span>
              <span
                role="cell"
                class="ellipsis muted"
              >{{ payloadText(m) }}</span>
              <span
                role="cell"
                class="num muted"
              >{{ formatBytes(m.size) }}</span>
            </div>
          </template>
          <p
            v-if="shown.length === 0"
            class="empty subtle"
          >
            {{ panel.messages.length === 0 ? (live ? 'Waiting for messages…' : 'Subscribe to a channel or pattern to see messages.') : 'No message matches the filter.' }}
          </p>
        </div>
      </div>

      <div
        v-if="selected"
        class="detail"
      >
        <div class="detail-bar">
          <span class="label">Selected message</span>
          <span class="mono">{{ selected.channel }}</span>
          <span class="subtle"><template v-if="selected.pattern">via <span class="mono">{{ selected.pattern }}</span> · </template>{{ messageTime(selected.receivedAt) }} · {{ formatBytes(selected.size) }}<template v-if="selected.truncated"> (shortened)</template></span>
          <span class="grow" />
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
            :disabled="selected.truncated || selected.payload.t !== 'text'"
            @click="republish"
          >
            Republish
          </button>
        </div>
        <pre class="mono payload">{{ payloadDetail(selected, pretty) }}</pre>
      </div>

      <div class="foot">
        Keeping the latest {{ formatCount(String(pubsub.MAX_VIEW_MESSAGES)) }} of {{ formatCount(String(panel.received)) }} received messages in view. Older ones are dropped, not stored.<template v-if="panel.droppedByBackend > 0">
          {{ formatCount(String(panel.droppedByBackend)) }} were dropped because the view fell behind.
        </template>
      </div>
    </section>
  </div>
</template>

<style scoped>
.pubsub {
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

.segmented button {
  flex: 1;
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

.subs,
.channels {
  display: flex;
  flex-direction: column;
  gap: 6px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.sub {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 36px;
  padding: 0 4px 0 8px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
  font-size: 12px;
}

.kind {
  padding: 0 6px;
  border: 1px solid var(--border);
  border-radius: 4px;
  font-size: 11px;
  color: var(--text-muted);
}

.name.pending {
  color: var(--text-subtle);
  font-style: italic;
}

.channels li {
  display: flex;
  gap: 8px;
  font-size: 12px;
}

.closed {
  flex-direction: column;
  align-items: flex-start;
}

.command {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
}

.command code {
  padding: 2px 6px;
  border-radius: 4px;
  background: var(--surface);
  font-size: 11px;
  overflow-wrap: anywhere;
}

.gap {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  min-height: 24px;
  padding: 2px 10px;
  border-top: 1px dashed var(--warning);
  border-bottom: 1px dashed var(--warning);
  background: var(--warning-tint);
  color: var(--warning-text);
  font-size: 12px;
}

.gap-text {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.field-label {
  font-size: 12px;
  color: var(--text-muted);
}

.message {
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
  grid-template-columns: 104px 136px 84px minmax(0, 1fr) 64px;
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
}

.foot {
  flex-shrink: 0;
  padding: 4px 16px;
  border-top: 1px solid var(--border-inner);
  font-size: 11px;
  color: var(--text-subtle);
}
</style>
