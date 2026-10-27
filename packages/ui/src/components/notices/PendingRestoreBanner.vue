<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue';
import type { PendingRestore } from '../../api/notices';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import Icon from '../common/Icon.vue';
import { formatRecordedAt, restoreCandidates, restoreEngine, restoreHeadline } from './notices';
import { restoreKey, useNotices } from './useNotices';

/**
 * A persistent banner for one database whose default collation a DDL run left
 * changed. Restore runs on an open tab of that server (the recording
 * connection's tab first) or on a new session of the recording connection.
 */
const props = defineProps<{ entry: PendingRestore }>();

const notices = useNotices();
const tabs = useTabs();
const workspace = useWorkspace();
const uid = useId();

const key = computed(() => restoreKey(props.entry));
const busy = computed(() => notices.state.busy.has(key.value));
const problem = computed(() => notices.state.problems.get(key.value) ?? null);
const recording = computed(() => workspace.connection(props.entry.connectionId));
const legacy = computed(() => restoreEngine(props.entry) === null);
const candidates = computed(() => restoreCandidates(props.entry, tabs.tabs));

/** `session:<id>` for an open tab, `connect` for a new session of the recording connection. */
const choice = ref<string>('');
watch(
  [candidates, recording],
  () => {
    const valid = new Set(candidates.value.map((c) => `session:${c.session.id}`));
    if (recording.value) valid.add('connect');
    if (valid.has(choice.value)) return;
    const first = candidates.value[0];
    choice.value = first ? `session:${first.session.id}` : recording.value ? 'connect' : '';
  },
  { immediate: true },
);

const canRestore = computed(() => !legacy.value && choice.value !== '' && !busy.value);

function restore(): void {
  if (!canRestore.value) return;
  const pick = choice.value === 'connect' ? { connectionId: props.entry.connectionId } : { sessionId: choice.value.slice('session:'.length) };
  void notices.restore(props.entry, pick);
}
</script>

<template>
  <div
    class="banner tone-warning"
    role="region"
    :aria-labelledby="`${uid}-head`"
    :aria-busy="busy"
  >
    <Icon
      name="warning"
      class="icon"
    />
    <div class="body">
      <p
        :id="`${uid}-head`"
        class="head"
      >
        {{ restoreHeadline(entry) }}
      </p>
      <p class="meta">
        Recorded {{ formatRecordedAt(entry.recordedAt) }} by
        <strong>{{ recording?.name ?? 'a deleted connection' }}</strong>
        <template v-if="entry.route">
          through <span class="mono">{{ entry.route }}</span>
        </template>.
        <template v-if="!entry.verified && !legacy">
          Varve cannot prove which server this is, so restoring asks you to confirm first.
        </template>
      </p>
      <p
        v-if="legacy"
        class="meta"
      >
        This entry comes from an older Varve version and cannot be restored from here. Check the database and dismiss it.
      </p>
      <p
        v-if="problem"
        class="problem"
        role="alert"
      >
        {{ problem.message }}
      </p>
    </div>
    <div
      v-if="!legacy"
      class="actions"
    >
      <label
        class="visually-hidden"
        :for="`${uid}-on`"
      >Restore through</label>
      <select
        :id="`${uid}-on`"
        v-model="choice"
        class="select compact"
        :disabled="busy"
      >
        <option
          v-if="choice === ''"
          value=""
          disabled
        >
          Open a tab of this server first
        </option>
        <option
          v-for="c in candidates"
          :key="c.session.id"
          :value="`session:${c.session.id}`"
        >
          {{ c.label }}{{ c.recording ? ' (recording connection)' : '' }}
        </option>
        <option
          v-if="recording"
          value="connect"
        >
          New session of {{ recording.name }}
        </option>
      </select>
      <button
        type="button"
        class="btn compact"
        :disabled="!canRestore"
        @click="restore"
      >
        {{ busy ? 'Restoring…' : 'Restore' }}
      </button>
    </div>
    <button
      type="button"
      class="btn compact"
      :class="{ primary: problem?.offerDismiss }"
      :disabled="busy"
      @click="notices.dismiss(entry)"
    >
      Dismiss
    </button>
  </div>
</template>

<style scoped>
.banner {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 8px 12px;
  border-bottom: 1px solid var(--border-inner);
  font-size: 12px;
}

.icon {
  flex-shrink: 0;
  margin-top: 2px;
}

.body {
  flex: 1;
  min-width: 0;
}

.body p {
  margin: 0;
}

.head {
  font-weight: 500;
}

.meta {
  margin-top: 2px;
}

.problem {
  margin-top: 4px;
  color: var(--danger-text);
}

.actions {
  display: flex;
  align-items: center;
  gap: 6px;
}

.compact {
  height: 28px;
  font-size: 12px;
}

.select.compact {
  max-width: 260px;
}
</style>
