<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { catalogDescribe } from '../../api/ddl';
import type { MyTable, ObjectRef, PgTable } from '../../api/ddl';
import { presentError, toCmdError } from '../../api/errors';
import { buildPartitionAction, objectRef, partitionActions } from '../../lib/ddlPlan';
import type { PartitionAction } from '../../lib/ddlPlan';
import { formatBytes, formatCount } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import FormField from '../common/FormField.vue';
import Icon from '../common/Icon.vue';
import PlanPreview from './PlanPreview.vue';
import { displayName } from './types';
import type { ObjectEditorEmits, ObjectEditorProps } from './types';
import { useChangePlan } from './useChangePlan';

/**
 * Partitions of a partitioned table: PostgreSQL partitions with their
 * bounds (open one, or the table designer for attach and detach), MySQL
 * and MariaDB partitions with rows and size plus maintenance (truncate,
 * drop, coalesce, add, remove partitioning), each shown as SQL first.
 * Props and events: `ObjectEditorProps` / `ObjectEditorEmits`; `object` is
 * the partitioned table (PostgreSQL `partitionedTable`, MySQL `table`).
 */
const props = withDefaults(defineProps<ObjectEditorProps>(), { container: null, createKind: null, reportError: null, intent: null, reportDirty: null });
const emit = defineEmits<ObjectEditorEmits>();

const pg = ref<PgTable | null>(null);
const my = ref<MyTable | null>(null);
const loading = ref(false);
const loadError = ref<string | null>(null);
const selected = ref<string[]>([]);
const count = ref('1');
const countId = useId();
const action = ref<PartitionAction | null>(null);

const plan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => emit('changed'),
  reportError: (error) => props.reportError?.(error),
});

async function load(): Promise<void> {
  const object = props.object;
  if (!object) {
    loadError.value = 'Open this view on a partitioned table.';
    return;
  }
  loading.value = true;
  loadError.value = null;
  try {
    const d = await catalogDescribe(props.session.id, object);
    if (d.engine === 'postgres' && 'partitioning' in d.object) {
      pg.value = d.object;
      if (!d.object.partitioning) loadError.value = 'This table is not partitioned.';
    } else if (d.engine === 'mysql' && d.object.kind === 'table') {
      my.value = d.object;
      if (!d.object.partitioning) loadError.value = 'This table is not partitioned.';
    } else {
      loadError.value = 'This object has no partitions.';
    }
  } catch (error: unknown) {
    props.reportError?.(error);
    loadError.value = presentError(toCmdError(error)).message;
  } finally {
    loading.value = false;
  }
}

const title = computed(() => (props.object ? displayName(props.object, props.session.engine) : 'Partitions'));
const myActions = computed(() => (my.value?.partitioning ? partitionActions(my.value.partitioning.method) : null));
const countValue = computed(() => {
  const n = Number(count.value);
  return Number.isInteger(n) && n > 0 ? n : null;
});

async function planAction(next: PartitionAction): Promise<void> {
  const table = my.value;
  if (!table) return;
  action.value = next;
  await plan.plan([{ kind: 'edit', change: buildPartitionAction(table.database, table.name, next, table.systemVersioning !== null) }]);
}

async function done(): Promise<void> {
  plan.clear();
  action.value = null;
  selected.value = [];
  await load();
}

function openPartition(schema: string, name: string): void {
  const ref: ObjectRef = objectRef('partition', props.object?.database ?? null, schema, name, { parent: props.object?.name ?? null });
  emit('openObject', ref);
}

watch(
  () => props.object,
  () => void load(),
);
onMounted(load);
</script>

<template>
  <div class="overview">
    <header class="header">
      <h1 class="mono">
        {{ title }}
      </h1>
      <span class="subtle">Partitions</span>
      <EnvBadge :environment="session.environment" />
      <span class="grow" />
      <button
        type="button"
        class="icon-btn"
        aria-label="Reload partitions"
        :disabled="loading"
        @click="load"
      >
        <Icon name="refresh" />
      </button>
    </header>
    <p
      v-if="loading"
      class="muted state"
      role="status"
    >
      Loading
    </p>
    <p
      v-else-if="loadError"
      class="notice tone-danger state"
      role="alert"
    >
      <Icon name="warning" />{{ loadError }}
    </p>

    <div
      v-else-if="pg?.partitioning"
      class="layout single"
    >
      <section
        class="section"
        aria-label="Partitions"
      >
        <div class="section-head">
          <h2>PARTITION BY {{ pg.partitioning.strategy.toUpperCase() }} ({{ pg.partitioning.key }})</h2>
          <span class="grow" />
          <span class="hint">{{ pg.partitioning.partitions.length }} partitions</span>
        </div>
        <table class="table">
          <thead>
            <tr>
              <th>Partition</th>
              <th>Bound</th>
              <th>Kind</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="p in pg.partitioning.partitions"
              :key="`${p.schema}.${p.name}`"
            >
              <td>
                <button
                  type="button"
                  class="btn compact ghost mono"
                  @click="openPartition(p.schema, p.name)"
                >
                  {{ p.schema }}.{{ p.name }}
                </button>
              </td>
              <td class="mono">
                {{ p.bound }}
              </td>
              <td>
                <span
                  v-if="p.isDefault"
                  class="pill tone-info"
                >default</span>
                <span
                  v-if="p.partitioned"
                  class="pill tone-neutral"
                >sub-partitioned</span>
                <span
                  v-if="p.foreign"
                  class="pill tone-neutral"
                >foreign</span>
                <span
                  v-if="p.detachPending"
                  class="pill tone-warning"
                >detach pending</span>
              </td>
            </tr>
          </tbody>
        </table>
        <p
          v-if="!pg.partitioning.defaultPartition"
          class="hint"
        >
          No default partition: rows outside every bound are rejected.
        </p>
        <p class="hint">
          Attach, detach and create partitions in the table designer.
        </p>
      </section>
    </div>

    <div
      v-else-if="my?.partitioning"
      class="layout"
    >
      <section
        class="section"
        aria-label="Partitions"
      >
        <div class="section-head">
          <h2>PARTITION BY {{ my.partitioning.linear ? 'LINEAR ' : '' }}{{ my.partitioning.method }} ({{ my.partitioning.expression ?? '' }})</h2>
        </div>
        <table class="table">
          <thead>
            <tr>
              <th><span class="visually-hidden">Select</span></th>
              <th>#</th>
              <th>Partition</th>
              <th>Values</th>
              <th>Rows</th>
              <th>Data</th>
              <th>Index</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="p in my.partitioning.partitions"
              :key="p.name"
            >
              <td>
                <input
                  v-model="selected"
                  type="checkbox"
                  :value="p.name"
                  :aria-label="`Select partition ${p.name}`"
                >
              </td>
              <td class="mono subtle">
                {{ p.position }}
              </td>
              <td class="mono">
                {{ p.name }}<span
                  v-if="p.subpartitions.length > 0"
                  class="hint"
                > ({{ p.subpartitions.length }} sub)</span>
              </td>
              <td class="mono">
                {{ p.description ?? '' }}
              </td>
              <td class="mono">
                {{ p.rows === null ? '' : formatCount(p.rows) }}
              </td>
              <td class="mono">
                {{ p.dataLength === null ? '' : formatBytes(p.dataLength) }}
              </td>
              <td class="mono">
                {{ p.indexLength === null ? '' : formatBytes(p.indexLength) }}
              </td>
            </tr>
          </tbody>
        </table>
        <p class="hint">
          Row counts and sizes are the server's estimates.
        </p>
      </section>
      <div class="column">
        <section
          class="section"
          aria-label="Maintenance"
        >
          <h2>Maintenance</h2>
          <div class="actions">
            <button
              type="button"
              class="btn compact"
              :disabled="session.readOnly || selected.length === 0"
              @click="planAction({ op: 'truncate', names: [...selected] })"
            >
              Truncate selected
            </button>
            <button
              v-if="myActions?.drop"
              type="button"
              class="btn compact"
              :disabled="session.readOnly || selected.length === 0"
              @click="planAction({ op: 'drop', names: [...selected] })"
            >
              Drop selected
            </button>
          </div>
          <template v-if="myActions?.coalesce || myActions?.addCount">
            <FormField
              label="Number of partitions"
              :input-id="countId"
              :error="countValue === null ? 'Enter a positive whole number.' : ''"
            >
              <input
                :id="countId"
                v-model="count"
                class="input mono"
                inputmode="numeric"
              >
            </FormField>
            <div class="actions">
              <button
                type="button"
                class="btn compact"
                :disabled="session.readOnly || countValue === null"
                @click="countValue !== null && planAction({ op: 'addCount', count: countValue })"
              >
                Add partitions
              </button>
              <button
                type="button"
                class="btn compact"
                :disabled="session.readOnly || countValue === null"
                @click="countValue !== null && planAction({ op: 'coalesce', count: countValue })"
              >
                Coalesce
              </button>
            </div>
          </template>
          <button
            type="button"
            class="btn compact"
            :disabled="session.readOnly"
            @click="planAction({ op: 'remove' })"
          >
            Remove partitioning
          </button>
          <p class="hint">
            Truncate and drop delete the rows of those partitions. Removing partitioning keeps the rows in one table and rebuilds it.
          </p>
        </section>
        <PlanPreview
          v-if="action !== null"
          :plan="plan"
          :engine="session.engine"
          :read-only="session.readOnly"
          apply-label="Run"
          @applied="done"
          @reconnect="emit('reconnect')"
          @refresh="load"
        />
      </div>
    </div>
  </div>
</template>

<style scoped src="./editor.css"></style>
<style scoped>
.overview {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: auto;
  background: var(--ground);
}

.header {
  display: flex;
  align-items: center;
  gap: 12px;
  min-height: 48px;
  padding: 8px 16px;
  border-bottom: 1px solid var(--border);
  background: var(--surface);
}

h1 {
  margin: 0;
  font-size: 15px;
  font-weight: 500;
}

.state {
  margin: 16px;
}

.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}
</style>
