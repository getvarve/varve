<script setup lang="ts">
import { computed, reactive, ref, shallowRef, useId, watch } from 'vue';
import { useTabs } from '../../composables/useTabs';
import {
  redisStreamConsumers,
  redisStreamGroups,
  redisStreamInfo,
  redisStreamPending,
} from '../../api/redis';
import type { PendingEntry, StreamConsumer, StreamGroup, StreamGroupChange, StreamInfo } from '../../api/redis';
import { latestOnly } from '../../lib/async';
import { isStreamId } from '../../lib/redisEdit';
import { redisErrorView } from '../../lib/redisErrors';
import { formatCount, formatDuration } from '../../lib/values';

/**
 * Stream facts, consumer groups with their consumers, and a group's pending
 * entries (XINFO, XPENDING). Changes (create, set id, destroy, delete
 * consumer, XACK, XCLAIM, XAUTOCLAIM) are emitted as plans for the key
 * viewer to preview and apply. Bump `refresh` to read everything again.
 */
const props = defineProps<{ tabId: string; sessionId: string; keyName: string; editable: boolean; refresh: number }>();
const emit = defineEmits<{ plan: [change: StreamGroupChange] }>();

const uid = useId();
const PENDING_PAGE = 50;

const info = shallowRef<StreamInfo | null>(null);
const groups = shallowRef<StreamGroup[]>([]);
const error = ref<string | null>(null);
const loading = ref(false);
const requests = latestOnly();

function message(e: unknown): string {
  const view = redisErrorView(useTabs().reportTabError(props.tabId, e));
  return view.hint ? `${view.message} ${view.hint}` : view.message;
}

async function loadGroups(): Promise<void> {
  const token = requests.next();
  loading.value = true;
  error.value = null;
  try {
    const [streamInfo, list] = await Promise.all([
      redisStreamInfo(props.sessionId, props.keyName),
      redisStreamGroups(props.sessionId, props.keyName),
    ]);
    if (!requests.isCurrent(token)) return;
    info.value = streamInfo;
    groups.value = list;
    if (selected.value !== null && !list.some((g) => g.name === selected.value)) selected.value = null;
    if (selected.value !== null) await loadGroup(selected.value);
  } catch (e: unknown) {
    if (requests.isCurrent(token)) error.value = message(e);
  } finally {
    if (requests.isCurrent(token)) loading.value = false;
  }
}

// Selected group: its consumers and pending entries.
const selected = ref<string | null>(null);
const consumers = shallowRef<StreamConsumer[]>([]);
const pending = shallowRef<PendingEntry[]>([]);
const pendingNext = ref<string | null>(null);
const picked = reactive(new Set<string>());
const groupError = ref<string | null>(null);
const groupRequests = latestOnly();
const pendingFilter = reactive<{ consumer: string; minIdleMs: string }>({ consumer: '', minIdleMs: '' });

async function loadGroup(group: string): Promise<void> {
  const token = groupRequests.next();
  groupError.value = null;
  picked.clear();
  try {
    const [list, page] = await Promise.all([
      redisStreamConsumers(props.sessionId, props.keyName, group),
      redisStreamPending(props.sessionId, props.keyName, group, pendingQuery(null)),
    ]);
    if (!groupRequests.isCurrent(token)) return;
    consumers.value = list;
    pending.value = page.entries;
    pendingNext.value = page.nextStart;
  } catch (e: unknown) {
    if (groupRequests.isCurrent(token)) groupError.value = message(e);
  }
}

function pendingQuery(start: string | null) {
  const idle = pendingFilter.minIdleMs.trim();
  return {
    start,
    count: PENDING_PAGE,
    consumer: pendingFilter.consumer === '' ? null : pendingFilter.consumer,
    minIdleMs: /^\d+$/.test(idle) ? idle : null,
  };
}

async function morePending(): Promise<void> {
  const group = selected.value;
  if (group === null || pendingNext.value === null) return;
  const token = groupRequests.next();
  try {
    const page = await redisStreamPending(props.sessionId, props.keyName, group, pendingQuery(pendingNext.value));
    if (!groupRequests.isCurrent(token)) return;
    pending.value = [...pending.value, ...page.entries];
    pendingNext.value = page.nextStart;
  } catch (e: unknown) {
    if (groupRequests.isCurrent(token)) groupError.value = message(e);
  }
}

function select(group: string): void {
  selected.value = selected.value === group ? null : group;
  if (selected.value !== null) void loadGroup(selected.value);
}

watch(() => [props.sessionId, props.keyName, props.refresh], loadGroups, { immediate: true });

const selectedGroup = computed(() => groups.value.find((g) => g.name === selected.value) ?? null);

// Create group
const create = reactive<{ name: string; id: string; mkStream: boolean }>({ name: '', id: '$', mkStream: false });
const createError = ref<string | null>(null);
function submitCreate(): void {
  const id = create.id.trim();
  if (create.name === '') createError.value = 'Enter a group name.';
  else if (id !== '$' && id !== '0' && !isStreamId(id)) createError.value = 'Start at $ (new entries only), 0 (everything) or an entry id.';
  else {
    createError.value = null;
    emit('plan', { op: 'groupCreate', group: create.name, id, mkStream: create.mkStream, entriesRead: null });
  }
}

// Set id of the selected group
const setId = ref('$');
const setIdError = ref<string | null>(null);
function submitSetId(): void {
  const group = selectedGroup.value;
  const id = setId.value.trim();
  if (!group) return;
  if (id !== '$' && id !== '0' && !isStreamId(id)) {
    setIdError.value = 'Use $, 0 or an entry id.';
    return;
  }
  setIdError.value = null;
  emit('plan', { op: 'groupSetId', group: group.name, id, entriesRead: null });
}

// Claim
const claim = reactive<{ consumer: string; minIdleMs: string; justId: boolean }>({ consumer: '', minIdleMs: '0', justId: false });
const claimError = ref<string | null>(null);

function claimTarget(): string | null {
  if (claim.consumer.trim() === '') {
    claimError.value = 'Choose or type the consumer that takes the entries over.';
    return null;
  }
  if (!/^\d{1,15}$/.test(claim.minIdleMs.trim())) {
    claimError.value = 'Enter the minimum idle time in milliseconds (0 for any).';
    return null;
  }
  claimError.value = null;
  return claim.consumer;
}

function submitClaim(): void {
  const group = selectedGroup.value;
  const consumer = claimTarget();
  if (!group || consumer === null || picked.size === 0) return;
  emit('plan', { op: 'claim', group: group.name, consumer, minIdleMs: claim.minIdleMs.trim(), ids: [...picked], justId: claim.justId });
}

function submitAutoClaim(): void {
  const group = selectedGroup.value;
  const consumer = claimTarget();
  if (!group || consumer === null) return;
  emit('plan', { op: 'autoClaim', group: group.name, consumer, minIdleMs: claim.minIdleMs.trim(), start: '0-0', count: 100, justId: claim.justId });
}

function ack(): void {
  const group = selectedGroup.value;
  if (group && picked.size > 0) emit('plan', { op: 'ack', group: group.name, ids: [...picked] });
}

function toggle(id: string): void {
  if (picked.has(id)) picked.delete(id);
  else picked.add(id);
}

function ms(text: string | null): string {
  if (text === null || !/^\d+$/.test(text)) return '';
  return formatDuration(Number(text));
}
</script>

<template>
  <section
    class="groups"
    :aria-labelledby="`${uid}-title`"
  >
    <dl
      v-if="info"
      class="facts"
    >
      <div>
        <dt>Length</dt><dd class="mono">
          {{ formatCount(info.length) }}
        </dd>
      </div>
      <div>
        <dt>Groups</dt><dd class="mono">
          {{ formatCount(info.groups) }}
        </dd>
      </div>
      <div v-if="info.firstEntryId">
        <dt>First</dt><dd class="mono">
          {{ info.firstEntryId }}
        </dd>
      </div>
      <div v-if="info.lastEntryId">
        <dt>Last</dt><dd class="mono">
          {{ info.lastEntryId }}
        </dd>
      </div>
      <div v-if="info.entriesAdded">
        <dt>Added ever</dt><dd class="mono">
          {{ formatCount(info.entriesAdded) }}
        </dd>
      </div>
    </dl>

    <div class="head">
      <h3
        :id="`${uid}-title`"
        class="label"
      >
        Consumer groups
      </h3>
      <span class="spacer" />
      <button
        type="button"
        class="btn ghost compact"
        :disabled="loading"
        @click="loadGroups"
      >
        Refresh
      </button>
    </div>
    <p
      v-if="error"
      class="notice tone-danger"
      role="alert"
    >
      {{ error }}
    </p>
    <p
      v-else-if="groups.length === 0 && !loading"
      class="subtle empty"
    >
      No consumer groups.
    </p>
    <table
      v-else
      class="table"
    >
      <thead>
        <tr>
          <th scope="col">
            group
          </th>
          <th
            scope="col"
            class="num"
          >
            consumers
          </th>
          <th
            scope="col"
            class="num"
          >
            pending
          </th>
          <th scope="col">
            last delivered
          </th>
          <th
            scope="col"
            class="num"
          >
            lag
          </th>
          <th
            v-if="editable"
            scope="col"
            class="actions"
          >
            <span class="visually-hidden">Actions</span>
          </th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="g in groups"
          :key="g.name"
          :class="{ selected: g.name === selected }"
        >
          <td class="mono">
            <button
              type="button"
              class="link mono"
              :aria-expanded="g.name === selected"
              @click="select(g.name)"
            >
              {{ g.name }}
            </button>
          </td>
          <td class="mono num">
            {{ formatCount(g.consumers) }}
          </td>
          <td class="mono num">
            {{ formatCount(g.pending) }}
          </td>
          <td class="mono">
            {{ g.lastDeliveredId }}
          </td>
          <td class="mono num">
            {{ g.lag === null ? '' : formatCount(g.lag) }}
          </td>
          <td
            v-if="editable"
            class="actions"
          >
            <button
              type="button"
              class="btn ghost compact"
              :aria-label="`Destroy group ${g.name}`"
              @click="emit('plan', { op: 'groupDestroy', group: g.name, expectedPending: g.pending })"
            >
              Destroy
            </button>
          </td>
        </tr>
      </tbody>
    </table>

    <template v-if="selectedGroup">
      <p
        v-if="groupError"
        class="notice tone-danger"
        role="alert"
      >
        {{ groupError }}
      </p>
      <h4 class="label">
        Consumers of {{ selectedGroup.name }}
      </h4>
      <table class="table">
        <thead>
          <tr>
            <th scope="col">
              consumer
            </th>
            <th
              scope="col"
              class="num"
            >
              pending
            </th>
            <th scope="col">
              idle
            </th>
            <th
              v-if="editable"
              scope="col"
              class="actions"
            >
              <span class="visually-hidden">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="c in consumers"
            :key="c.name"
          >
            <td class="mono">
              {{ c.name }}
            </td>
            <td class="mono num">
              {{ formatCount(c.pending) }}
            </td>
            <td class="mono">
              {{ ms(c.idleMs) }}
            </td>
            <td
              v-if="editable"
              class="actions"
            >
              <button
                type="button"
                class="btn ghost compact"
                :aria-label="`Delete consumer ${c.name}`"
                @click="emit('plan', { op: 'groupDelConsumer', group: selectedGroup.name, consumer: c.name, expectedPending: c.pending })"
              >
                Delete
              </button>
            </td>
          </tr>
        </tbody>
      </table>

      <div class="head">
        <h4 class="label">
          Pending entries
        </h4>
        <span class="subtle">{{ formatCount(selectedGroup.pending) }} pending<template v-if="picked.size > 0"> · {{ picked.size }} selected</template></span>
        <span class="spacer" />
        <label :for="`${uid}-pc`">consumer</label>
        <select
          :id="`${uid}-pc`"
          v-model="pendingFilter.consumer"
          class="select compact"
        >
          <option value="">
            all
          </option>
          <option
            v-for="c in consumers"
            :key="c.name"
            :value="c.name"
          >
            {{ c.name }}
          </option>
        </select>
        <label :for="`${uid}-pidle`">min idle ms</label>
        <input
          :id="`${uid}-pidle`"
          v-model="pendingFilter.minIdleMs"
          class="input compact mono idle"
          inputmode="numeric"
        >
        <button
          type="button"
          class="btn compact"
          @click="loadGroup(selectedGroup.name)"
        >
          Apply
        </button>
      </div>
      <table class="table">
        <thead>
          <tr>
            <th
              v-if="editable"
              scope="col"
              class="pick"
            >
              <span class="visually-hidden">Select</span>
            </th>
            <th scope="col">
              ID
            </th>
            <th scope="col">
              consumer
            </th>
            <th scope="col">
              idle
            </th>
            <th
              scope="col"
              class="num"
            >
              deliveries
            </th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="p in pending"
            :key="p.id"
          >
            <td
              v-if="editable"
              class="pick"
            >
              <input
                type="checkbox"
                :checked="picked.has(p.id)"
                :aria-label="`Select ${p.id}`"
                @change="toggle(p.id)"
              >
            </td>
            <td class="mono">
              {{ p.id }}
            </td>
            <td class="mono">
              {{ p.consumer }}
            </td>
            <td class="mono">
              {{ ms(p.idleMs) }}
            </td>
            <td class="mono num">
              {{ formatCount(p.deliveries) }}
            </td>
          </tr>
        </tbody>
      </table>
      <button
        v-if="pendingNext !== null"
        type="button"
        class="btn compact more"
        @click="morePending"
      >
        Load more pending
      </button>

      <form
        v-if="editable"
        class="row"
        novalidate
        @submit.prevent="submitClaim"
      >
        <button
          type="button"
          class="btn compact"
          :disabled="picked.size === 0"
          @click="ack"
        >
          XACK {{ picked.size }} selected
        </button>
        <label :for="`${uid}-claim`">XCLAIM to</label>
        <input
          :id="`${uid}-claim`"
          v-model="claim.consumer"
          class="input compact mono"
          :list="`${uid}-consumers`"
          spellcheck="false"
        >
        <datalist :id="`${uid}-consumers`">
          <option
            v-for="c in consumers"
            :key="c.name"
            :value="c.name"
          />
        </datalist>
        <label :for="`${uid}-idle`">min idle ms</label>
        <input
          :id="`${uid}-idle`"
          v-model="claim.minIdleMs"
          class="input compact mono idle"
          inputmode="numeric"
        >
        <label class="check"><input
          v-model="claim.justId"
          type="checkbox"
        > JUSTID</label>
        <button
          type="submit"
          class="btn compact"
          :disabled="picked.size === 0"
        >
          Claim selected
        </button>
        <button
          type="button"
          class="btn ghost compact"
          @click="submitAutoClaim"
        >
          Auto-claim idle (XAUTOCLAIM)
        </button>
        <span
          v-if="claimError"
          class="error"
          role="alert"
        >{{ claimError }}</span>
      </form>

      <form
        v-if="editable"
        class="row"
        novalidate
        @submit.prevent="submitSetId"
      >
        <label :for="`${uid}-setid`">Set last delivered id (XGROUP SETID)</label>
        <input
          :id="`${uid}-setid`"
          v-model="setId"
          class="input compact mono"
          spellcheck="false"
        >
        <button
          type="submit"
          class="btn compact"
        >
          Set id
        </button>
        <span
          v-if="setIdError"
          class="error"
          role="alert"
        >{{ setIdError }}</span>
      </form>
    </template>

    <form
      v-if="editable"
      class="row create"
      novalidate
      @submit.prevent="submitCreate"
    >
      <label :for="`${uid}-gname`">Create group</label>
      <input
        :id="`${uid}-gname`"
        v-model="create.name"
        class="input compact mono"
        placeholder="name"
        spellcheck="false"
      >
      <label :for="`${uid}-gid`">from</label>
      <input
        :id="`${uid}-gid`"
        v-model="create.id"
        class="input compact mono idle"
        spellcheck="false"
      >
      <label class="check"><input
        v-model="create.mkStream"
        type="checkbox"
      > MKSTREAM</label>
      <button
        type="submit"
        class="btn compact"
      >
        Create group
      </button>
      <span
        v-if="createError"
        class="error"
        role="alert"
      >{{ createError }}</span>
    </form>
  </section>
</template>

<style scoped src="./editorTable.css"></style>
<style scoped>
.groups {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin-top: 16px;
}

.facts {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 24px;
  margin: 0;
  font-size: 12px;
}

.facts div {
  display: flex;
  gap: 6px;
}

.facts dt {
  color: var(--text-muted);
}

.facts dd {
  margin: 0;
}

.head,
.row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  font-size: 12px;
  color: var(--text-muted);
}

h3,
h4 {
  margin: 0;
}

.spacer {
  flex: 1;
}

.idle {
  width: 96px;
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.pick {
  width: 28px;
}

.link {
  padding: 0;
  border: 0;
  background: none;
  color: var(--accent);
  cursor: pointer;
}

tr.selected td {
  background: var(--accent-tint);
}

.empty,
.notice {
  margin: 0;
  font-size: 12px;
}

.more {
  align-self: flex-start;
}
</style>
