<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue';
import { toCmdError } from '../../api/errors';
import { redisDatabases } from '../../api/live';
import type { RedisDatabases } from '../../api/live';
import { redisKeyspace } from '../../api/redis';
import type { DbKeyspace } from '../../api/redis';
import { useTabs } from '../../composables/useTabs';
import type { WorkspaceTab } from '../../composables/useTabs';
import { useToasts } from '../../composables/useToasts';
import { latestOnly } from '../../lib/async';
import { isLostSession } from '../../lib/connectFlow';
import { dbNumberLimit, dbOptions } from '../../lib/redisDbOptions';
import { redisErrorView } from '../../lib/redisErrors';

/** Switches the logical database of a Redis tab's session (`redis_select_db`). */
const props = defineProps<{ tab: WorkspaceTab }>();
const emit = defineEmits<{ keyspace: [dbs: DbKeyspace[]] }>();

const tabs = useTabs();
const toasts = useToasts();
const uid = useId();
const keyspace = ref<DbKeyspace[]>([]);
/** How many databases the server has (`redis_databases`); `null` until read or when it cannot be read. */
const databases = ref<RedisDatabases | null>(null);
const busy = ref(false);
const requests = latestOnly();

async function loadKeyspace(): Promise<void> {
  const sessionId = props.tab.session?.id;
  if (!sessionId) return;
  const token = requests.next();
  try {
    const dbs = await redisKeyspace(sessionId);
    if (!requests.isCurrent(token)) return;
    keyspace.value = dbs;
    emit('keyspace', dbs);
  } catch (error: unknown) {
    if (!requests.isCurrent(token)) return;
    keyspace.value = [];
    const cmd = toCmdError(error);
    if (isLostSession(cmd)) tabs.markLost(props.tab.id, cmd);
    else toasts.pushError(error, 'Could not read the Redis databases');
  }
}

watch(() => [props.tab.session?.id, props.tab.db], loadKeyspace, { immediate: true });

/** The server's database count (once per session); without it the picker falls back to 0 to 15. */
async function loadDatabases(): Promise<void> {
  const sessionId = props.tab.session?.id;
  databases.value = null;
  if (!sessionId) return;
  try {
    const found = await redisDatabases(sessionId);
    if (props.tab.session?.id === sessionId) databases.value = found;
  } catch {
    // The keyspace call reports a lost session; the picker keeps the fallback list.
  }
}
watch(() => props.tab.session?.id, loadDatabases, { immediate: true });

/** The databases the server has, with key counts; the current one always included. */
const options = computed(() => dbOptions(databases.value, keyspace.value, props.tab.db));
/** With very many databases the list is short and a number field reaches the others. */
const numberLimit = computed(() => dbNumberLimit(databases.value));
const typed = ref('');

async function choose(event: Event): Promise<void> {
  const select = event.target as HTMLSelectElement;
  await switchTo(Number(select.value));
  // The select always shows the database the session is really on.
  select.value = String(props.tab.db ?? 0);
}

async function chooseTyped(): Promise<void> {
  const limit = numberLimit.value;
  const db = Number(typed.value.trim());
  if (limit === null || !/^\d+$/.test(typed.value.trim()) || db > limit) {
    toasts.push('warning', 'No such database', `Enter a database number from 0 to ${limit ?? 0}.`);
    return;
  }
  await switchTo(db);
  typed.value = '';
}

async function switchTo(db: number): Promise<void> {
  if (db === props.tab.db) return;
  busy.value = true;
  try {
    await tabs.selectDb(props.tab.id, db);
  } catch (error: unknown) {
    const view = redisErrorView(toCmdError(error));
    // MULTI_OPEN and other refusals say what to do; the toast stays a while so it can be read.
    if (view.refused) toasts.push('warning', `Could not switch to db${db}`, view.hint ? `${view.message} ${view.hint}` : view.message, 12_000);
    else toasts.pushError(error, `Could not switch to db${db}`);
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <span class="db-select">
    <label
      :for="`${uid}-db`"
      class="visually-hidden"
    >Redis database</label>
    <select
      :id="`${uid}-db`"
      class="select"
      :value="tab.db ?? 0"
      :disabled="!tab.session || busy"
      @change="choose"
    >
      <option
        v-for="o in options"
        :key="o.db"
        :value="o.db"
      >
        {{ o.label }}
      </option>
    </select>
    <template v-if="numberLimit !== null">
      <label
        :for="`${uid}-dbn`"
        class="visually-hidden"
      >Go to database number</label>
      <input
        :id="`${uid}-dbn`"
        v-model="typed"
        class="input number"
        inputmode="numeric"
        :placeholder="`db 0 to ${numberLimit}`"
        :disabled="!tab.session || busy"
        @keydown.enter.prevent="chooseTyped"
      >
    </template>
  </span>
</template>

<style scoped>
.select {
  width: auto;
  height: 28px;
  font-family: var(--font-mono);
  font-size: 12px;
}

.db-select {
  display: inline-flex;
  gap: 6px;
  align-items: center;
}

.number {
  width: 110px;
  height: 28px;
  font-family: var(--font-mono);
  font-size: 12px;
}
</style>
