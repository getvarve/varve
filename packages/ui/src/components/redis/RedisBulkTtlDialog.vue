<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, useId } from 'vue';
import { keyTypeName, redisBulkTtlJobs, redisBulkTtlPreview, redisBulkTtlStart, redisBulkTtlStatus, redisBulkTtlStop } from '../../api/redis';
import type { BulkCount, BulkFilter, BulkTtlAction, BulkTtlStatus } from '../../api/redis';
import { toCmdError } from '../../api/errors';
import { useDialogs } from '../../composables/useDialogs';
import { runWithConfirmation } from '../../lib/destructive';
import { redisErrorView } from '../../lib/redisErrors';
import { bulkProgressText, previewText, ttlFromSeconds } from '../../lib/redisModules';
import Modal from '../common/Modal.vue';

/**
 * Sets or removes the TTL of every key that matches the browser's filter
 * (design doc 4.2, TTL): a count first, then the change in bounded steps on
 * its own connection, with progress and Stop. Sessions that confirm
 * destructive statements (production) ask first. A job keeps running when
 * the dialog closes; opening the dialog again finds it (the tab's job list).
 */
const props = defineProps<{
  sessionId: string;
  filter: BulkFilter;
  connectionName: string;
}>();
const emit = defineEmits<{ close: [changed: boolean] }>();

/** How often a running job is asked for its progress. */
const POLL_MS = 300;
/** Failed status reads in a row before the dialog stops asking (with backoff). */
const POLL_ATTEMPTS = 5;

const uid = useId();
const preview = shallowRef<BulkCount | null>(null);
const previewing = ref(false);
const error = ref<string | null>(null);
const mode = ref<'expire' | 'persist'>('expire');
const seconds = ref('');
const status = shallowRef<BulkTtlStatus | null>(null);
const starting = ref(false);
const stopping = ref(false);
/** The last status read failed; `lostTrack` once the retries are used up. */
const pollError = ref<string | null>(null);
const lostTrack = ref(false);
let timer: ReturnType<typeof setTimeout> | null = null;
let alive = true;

const filterText = computed(() => {
  const pattern = props.filter.pattern === '' ? '*' : props.filter.pattern;
  return props.filter.keyType ? `${pattern} (type ${keyTypeName(props.filter.keyType)})` : pattern;
});

const action = computed<BulkTtlAction | null>(() => {
  if (mode.value === 'persist') return { kind: 'persist' };
  const ms = ttlFromSeconds(seconds.value);
  return ms === null ? null : { kind: 'expire', ttlMs: ms };
});
const secondsProblem = computed(() => (mode.value === 'expire' && seconds.value.trim() !== '' && action.value === null ? 'A whole number of seconds, at least 1.' : null));
/** A job runs and the dialog still follows it. */
const running = computed(() => status.value?.state === 'running' && !lostTrack.value);
const finished = computed(() => status.value !== null && status.value.state !== 'running');
/** A few matching key names, with an ellipsis when there are more. */
const sampleText = computed(() => {
  const counted = preview.value;
  if (!counted) return '';
  const shown = counted.sample.slice(0, 5);
  return counted.keys === String(shown.length) ? shown.join(', ') : `${shown.join(', ')}, …`;
});

function message(e: unknown): string {
  const view = redisErrorView(toCmdError(e));
  return view.hint ? `${view.message} ${view.hint}` : view.message;
}

async function count(): Promise<void> {
  previewing.value = true;
  error.value = null;
  try {
    const out = await redisBulkTtlPreview(props.sessionId, props.filter);
    if (alive) preview.value = out;
  } catch (e: unknown) {
    if (alive) error.value = message(e);
  } finally {
    previewing.value = false;
  }
}

function clearTimer(): void {
  if (timer !== null) clearTimeout(timer);
  timer = null;
}

/** Follows a job; a failed read is retried with backoff, then the dialog lets go (Close, Stop and Check again work). */
function poll(jobId: string, attempt = 0): void {
  clearTimer();
  timer = setTimeout(() => {
    void (async () => {
      try {
        const next = await redisBulkTtlStatus(props.sessionId, jobId);
        if (!alive) return;
        status.value = next;
        pollError.value = null;
        lostTrack.value = false;
        if (next.state === 'running') poll(jobId);
        else stopping.value = false;
      } catch (e: unknown) {
        if (!alive) return;
        pollError.value = message(e);
        if (attempt + 1 < POLL_ATTEMPTS) poll(jobId, attempt + 1);
        else {
          lostTrack.value = true;
          stopping.value = false;
        }
      }
    })();
  }, POLL_MS * 2 ** attempt);
}

/** Finds the tab's job on the server: one still running is followed again. */
async function reconcile(jobId: string | null = null): Promise<void> {
  try {
    const jobs = await redisBulkTtlJobs(props.sessionId);
    if (!alive) return;
    const job = jobId === null ? jobs.find((j) => j.state === 'running') : jobs.find((j) => j.jobId === jobId);
    if (!job) {
      if (jobId !== null) {
        pollError.value = 'The server no longer knows this job (the tab was reconnected or it ended long ago). Count the keys again to see where they are.';
        lostTrack.value = true;
        stopping.value = false;
      }
      return;
    }
    status.value = job;
    pollError.value = null;
    lostTrack.value = false;
    if (job.state === 'running') poll(job.jobId);
  } catch (e: unknown) {
    if (alive) pollError.value = message(e);
  }
}

onMounted(() => {
  void reconcile();
  void count();
});

async function start(): Promise<void> {
  const chosen = action.value;
  const counted = preview.value;
  if (!chosen || !counted || starting.value || running.value) return;
  starting.value = true;
  error.value = null;
  try {
    const job = await runWithConfirmation(
      (confirmed) => redisBulkTtlStart(props.sessionId, { filter: props.filter, action: chosen, db: counted.db }, confirmed),
      (request) =>
        useDialogs().askDestructive({
          connectionName: props.connectionName,
          environment: request.environment,
          statements: request.statements,
          warnings: [
            previewText(counted),
            chosen.kind === 'expire' ? 'Every matching key expires and is deleted when its time is up.' : 'Every matching key stops expiring and stays until it is deleted.',
          ],
        }),
    );
    if (!job || !alive) return;
    status.value = {
      jobId: job.jobId,
      state: 'running',
      counts: { steps: '0', matched: '0', changed: '0', skipped: '0' },
      error: null,
      elapsedMs: 0,
      db: counted.db,
      action: chosen,
      filter: props.filter,
    };
    lostTrack.value = false;
    pollError.value = null;
    poll(job.jobId);
  } catch (e: unknown) {
    if (alive) error.value = message(e);
  } finally {
    starting.value = false;
  }
}

async function stop(): Promise<void> {
  const job = status.value;
  if (!job || job.state !== 'running' || stopping.value) return;
  stopping.value = true;
  try {
    status.value = await redisBulkTtlStop(props.sessionId, job.jobId);
    // Following again ends `stopping` once the job reports its end.
    lostTrack.value = false;
    poll(job.jobId);
  } catch (e: unknown) {
    stopping.value = false;
    pollError.value = message(e);
  }
}

function close(): void {
  // A running job goes on on the server; opening the dialog again finds it.
  emit('close', status.value !== null && status.value.counts.changed !== '0');
}

onBeforeUnmount(() => {
  alive = false;
  clearTimer();
});
</script>

<template>
  <Modal
    title="Set or remove TTL in bulk"
    :width="520"
    @close="close"
  >
    <div class="content">
      <p>
        Keys matching <span class="mono">{{ filterText }}</span>, the browser's current filter.
      </p>
      <p
        v-if="previewing"
        class="subtle"
        role="status"
      >
        Counting keys…
      </p>
      <template v-else-if="preview">
        <p role="status">
          {{ previewText(preview) }}
        </p>
        <p
          v-if="preview.sample.length > 0"
          class="subtle mono sample"
        >
          {{ sampleText }}
        </p>
      </template>

      <fieldset
        class="mode"
        :disabled="running || finished"
      >
        <legend class="visually-hidden">
          Change
        </legend>
        <label class="check"><input
          v-model="mode"
          type="radio"
          value="expire"
        > Expire in</label>
        <input
          :id="`${uid}-seconds`"
          v-model="seconds"
          class="input compact mono"
          inputmode="numeric"
          aria-label="Seconds"
          :disabled="mode !== 'expire'"
          :aria-invalid="!!secondsProblem"
        >
        <span>seconds (PEXPIRE)</span>
        <label class="check"><input
          v-model="mode"
          type="radio"
          value="persist"
        > Remove the TTL (PERSIST)</label>
      </fieldset>
      <span
        v-if="secondsProblem"
        class="error"
        role="alert"
      >{{ secondsProblem }}</span>
      <p
        v-if="preview?.cluster"
        class="notice tone-warning"
      >
        This server runs in cluster mode: the count and the change only reach the keys of the node this connection is on.
      </p>
      <p class="subtle">
        Runs in steps of at most 500 keys on its own connection, so the console stays free. Stop keeps the keys changed so far.
      </p>

      <p
        v-if="status"
        class="progress"
        role="status"
        :class="status.state === 'failed' ? 'tone-danger' : ''"
      >
        {{ bulkProgressText(status) }}
      </p>
      <p
        v-if="error"
        class="notice tone-danger"
        role="alert"
      >
        {{ error }}
      </p>
      <div
        v-if="pollError"
        class="notice tone-warning"
        role="alert"
      >
        <p>{{ lostTrack ? 'Lost track of the job:' : 'Could not read the progress, trying again:' }} {{ pollError }}</p>
        <button
          v-if="lostTrack && status"
          type="button"
          class="btn compact"
          @click="reconcile(status.jobId)"
        >
          Check again
        </button>
      </div>
    </div>
    <template #footer>
      <button
        v-if="status?.state === 'running'"
        type="button"
        class="btn"
        :disabled="stopping"
        @click="stop"
      >
        {{ stopping ? 'Stopping…' : 'Stop' }}
      </button>
      <button
        type="button"
        class="btn"
        :title="running ? 'The change goes on; open Bulk TTL again to follow it' : undefined"
        @click="close"
      >
        {{ running ? 'Close (keeps running)' : finished ? 'Close' : 'Cancel' }}
      </button>
      <button
        v-if="!finished"
        type="button"
        class="btn primary"
        :disabled="!preview || !action || status?.state === 'running' || starting || previewing || preview.keys === '0'"
        @click="start"
      >
        {{ running ? 'Running…' : mode === 'expire' ? 'Set TTL' : 'Remove TTL' }}
      </button>
    </template>
  </Modal>
</template>

<style scoped>
.content {
  display: flex;
  flex-direction: column;
  gap: 8px;
  font-size: 13px;
}

.content p {
  margin: 0;
}

.mode {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  border: 0;
  padding: 0;
  margin: 0;
}

.mode .input {
  width: 96px;
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.sample {
  overflow-wrap: anywhere;
}

.compact {
  height: 28px;
  font-size: 12px;
}

.error {
  font-size: 11px;
  color: var(--danger-text);
}
</style>
