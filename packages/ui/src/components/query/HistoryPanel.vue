<script setup lang="ts">
import { ref, watch } from 'vue';
import { fetchQueryHistory, statementOf } from '../../api/activity';
import type { ActivityEvent } from '../../api/activity';
import { presentError, toCmdError } from '../../api/errors';
import { latestOnly } from '../../lib/async';
import { formatDateTime } from '../../lib/display';
import { formatDuration } from '../../lib/values';
import Icon from '../common/Icon.vue';

/** Query history of the tab's connection, from the activity log. Clicking an entry loads it into the editor. */
const props = defineProps<{ connectionId: string; refreshKey: number }>();
const emit = defineEmits<{ load: [sql: string]; close: [] }>();

const events = ref<ActivityEvent[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);

// Only the newest request may fill the list (switching connections or runs quickly).
const requests = latestOnly();

async function load(): Promise<void> {
  const token = requests.next();
  loading.value = true;
  try {
    const page = await fetchQueryHistory(props.connectionId, 200);
    if (!requests.isCurrent(token)) return;
    events.value = page.events;
    error.value = null;
  } catch (e: unknown) {
    if (requests.isCurrent(token)) error.value = presentError(toCmdError(e)).message;
  } finally {
    if (requests.isCurrent(token)) loading.value = false;
  }
}

watch(() => [props.connectionId, props.refreshKey], load, { immediate: true });

const OUTCOME_TONE = { success: 'tone-success', failure: 'tone-danger', cancelled: 'tone-neutral' } as const;
</script>

<template>
  <aside
    class="history"
    aria-label="Query history"
  >
    <header>
      <Icon name="history" />
      <strong>History</strong>
      <span class="spacer" />
      <button
        type="button"
        class="icon-btn"
        aria-label="Refresh history"
        :disabled="loading"
        @click="load"
      >
        <Icon name="refresh" />
      </button>
      <button
        type="button"
        class="icon-btn"
        aria-label="Close history"
        @click="emit('close')"
      >
        <Icon name="close" />
      </button>
    </header>
    <p
      v-if="error"
      class="notice tone-danger"
      role="alert"
    >
      {{ error }}
    </p>
    <p
      v-else-if="!loading && events.length === 0"
      class="subtle empty"
    >
      Statements you run on this connection appear here.
    </p>
    <ul class="list">
      <li
        v-for="e in events"
        :key="e.id"
      >
        <button
          type="button"
          class="entry"
          :disabled="!statementOf(e)"
          :title="statementOf(e) ? 'Load into the editor' : 'The statement text was not recorded'"
          @click="emit('load', statementOf(e) ?? '')"
        >
          <span class="meta">
            <span
              class="pill"
              :class="OUTCOME_TONE[e.outcome]"
            ><span class="dot" />{{ e.outcome }}</span>
            <span class="subtle">{{ formatDateTime(e.occurredAt).slice(0, 19) }}</span>
            <span
              v-if="e.durationMs !== null"
              class="subtle"
            >{{ formatDuration(e.durationMs) }}</span>
          </span>
          <span class="sql mono">{{ statementOf(e) ?? e.action }}</span>
        </button>
      </li>
    </ul>
  </aside>
</template>

<style scoped>
.history {
  display: flex;
  flex-direction: column;
  width: 320px;
  min-height: 0;
  border-left: 1px solid var(--border);
  background: var(--surface);
}

header {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 4px 0 12px;
  border-bottom: 1px solid var(--border-inner);
}

.spacer {
  flex: 1;
}

.empty,
.notice {
  margin: 12px;
}

.list {
  flex: 1;
  margin: 0;
  padding: 0;
  overflow: auto;
  list-style: none;
}

.entry {
  display: flex;
  flex-direction: column;
  gap: 4px;
  width: 100%;
  padding: 8px 12px;
  border: 0;
  border-bottom: 1px solid var(--border-inner);
  background: transparent;
  color: var(--text);
  text-align: left;
  cursor: pointer;
}

.entry:hover:not(:disabled) {
  background: var(--accent-tint);
}

.entry:disabled {
  cursor: default;
}

.meta {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 11px;
}

.sql {
  display: -webkit-box;
  overflow: hidden;
  font-size: 12px;
  white-space: pre-wrap;
  -webkit-line-clamp: 3;
  -webkit-box-orient: vertical;
}
</style>
