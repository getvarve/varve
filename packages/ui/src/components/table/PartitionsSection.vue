<script setup lang="ts">
import { computed, reactive, ref, useId, watch } from 'vue';
import type { MyPartitionDef, MyPartitionInfo, PgPartitionEntry } from '../../api/grid';
import {
  myBoundValue,
  myPartitionChange,
  myReorganize,
  mySubpartitioning,
  hashModuli,
  nextRangePartitions,
  pgBoundProblem,
  pgAttachPartition,
  pgCreatePartition,
  pgDetachPartition,
  splitList,
} from '../../lib/gridDesigner';
import type { MyReorganizeTarget, PartitionInterval, PgBoundDraft } from '../../lib/gridDesigner';
import { formatBytes, formatCount } from '../../lib/values';
import Icon from '../common/Icon.vue';
import { useDesigner } from './designerContext';

/**
 * Designer section: partitions. Every action here is planned and applied
 * on its own right away (it is not part of Save), as the canvas
 * TableDesigner-Partitions shows.
 */
const ctx = useDesigner();
const uid = useId();
const pgInfo = computed(() => (ctx.details?.engine === 'postgres' && ctx.details.object.kind !== 'materializedView' ? ctx.details.object : null));
const myInfo = computed(() => (ctx.details?.engine === 'mysql' ? ctx.details.object : null));
const pgParts = computed<PgPartitionEntry[]>(() => pgInfo.value?.partitioning?.partitions ?? []);
const myParts = computed<MyPartitionInfo[]>(() => myInfo.value?.partitioning?.partitions ?? []);

/** The partition a tree node asked for, selected when the section opens. */
function focused(): string | null {
  const name = ctx.focus?.section === 'partitions' ? ctx.focus.name : null;
  if (name === null) return null;
  const pgPart = pgParts.value.find((p) => p.name === name);
  if (pgPart) return `${pgPart.schema}.${pgPart.name}`;
  return myParts.value.find((p) => p.name === name)?.name ?? null;
}
const selected = ref<string | null>(focused());
const strategy = computed(() => (pgInfo.value?.partitioning?.strategy ?? myInfo.value?.partitioning?.method ?? '').toLowerCase());

// --- PostgreSQL ---

const next = reactive({ count: 3, interval: 'month' as PartitionInterval, pattern: `${ctx.draft.table}_{YYYY}_{MM}`, tablespace: '' });
const proposals = computed(() =>
  strategy.value === 'range' ? nextRangePartitions(pgParts.value.map((p) => p.bound), Math.min(Math.max(next.count, 0), 48), next.interval, next.pattern) : [],
);
/** The key as `pg_get_partkeydef` writes it (`RANGE (id, at)`), without the strategy shown before it. */
const pgKeyText = computed(() => {
  const p = pgInfo.value?.partitioning;
  if (!p) return '';
  const lead = `${p.strategy.toUpperCase()} `;
  return p.key.toUpperCase().startsWith(lead) ? p.key.slice(lead.length) : p.key;
});
const defaultPartition = computed(() => pgParts.value.find((p) => p.isDefault) ?? null);

const initialKind: PgBoundDraft['kind'] = strategy.value === 'list' ? 'list' : strategy.value === 'hash' ? 'hash' : 'range';
const create = reactive<{ name: string; kind: PgBoundDraft['kind']; from: string; to: string; values: string; modulus: number; remainder: number; attach: boolean; attachSchema: string; attachTable: string }>({ name: '', kind: initialKind, from: '', to: '', values: '', modulus: 4, remainder: 0, attach: false, attachSchema: '', attachTable: '' });

function bound(): PgBoundDraft {
  switch (create.kind) {
    case 'range':
      return { kind: 'range', from: create.from, to: create.to };
    case 'list':
      return { kind: 'list', values: create.values };
    case 'hash':
      return { kind: 'hash', modulus: Number(create.modulus), remainder: Number(create.remainder) };
    case 'default':
      return { kind: 'default' };
  }
}

function createNext(): void {
  const changes = proposals.value.map((p) =>
    pgCreatePartition(ctx.draft.schema, ctx.draft.table, p.name, { kind: 'range', from: p.from, to: p.to }, next.tablespace.trim() || null),
  );
  if (changes.length > 0) ctx.runNow(`Create ${changes.length} partitions of ${ctx.draft.table}`, changes);
}

function createOne(): void {
  if (create.attach) {
    ctx.runNow(`Attach ${create.attachTable} to ${ctx.draft.table}`, [
      pgAttachPartition(ctx.draft.schema, ctx.draft.table, { schema: create.attachSchema.trim() || ctx.draft.schema, name: create.attachTable.trim() }, bound()),
    ]);
  } else {
    ctx.runNow(`Create partition ${create.name}`, [pgCreatePartition(ctx.draft.schema, ctx.draft.table, create.name.trim(), bound(), null)]);
  }
}

/** Why the bound would be refused (NULL in a range, a hash modulus that does not fit the existing ones), or `null`. */
const boundProblem = computed(() => pgBoundProblem(bound(), hashModuli(pgParts.value.map((p) => p.bound))));

const createReady = computed(() => {
  if (create.attach ? !create.attachTable.trim() : !create.name.trim()) return false;
  if (boundProblem.value !== null) return false;
  if (create.kind === 'range') return splitList(create.from).length > 0 && splitList(create.to).length > 0;
  if (create.kind === 'list') return splitList(create.values).length > 0;
  if (create.kind === 'hash') return Number(create.modulus) > 0 && Number(create.remainder) >= 0 && Number(create.remainder) < Number(create.modulus);
  return true;
});

/** CONCURRENTLY needs PostgreSQL 14 and no default partition; off unless chosen. */
const detach = reactive({ concurrently: false, drop: false });
const concurrentDetachProblem = computed(() =>
  ctx.serverMajor < 14
    ? `not available on PostgreSQL ${ctx.serverMajor} (14 and later)`
    : defaultPartition.value
      ? 'not possible while a default partition exists'
      : null,
);
watch(concurrentDetachProblem, (problem) => {
  if (problem !== null) detach.concurrently = false;
});
const pgSelected = computed(() => pgParts.value.find((p) => `${p.schema}.${p.name}` === selected.value) ?? null);

function detachSelected(): void {
  const p = pgSelected.value;
  if (!p) return;
  const concurrently = detach.concurrently && concurrentDetachProblem.value === null;
  ctx.runNow(`${detach.drop ? 'Detach and drop' : 'Detach'} ${p.name}`, pgDetachPartition(ctx.draft.schema, ctx.draft.table, { schema: p.schema, name: p.name }, concurrently, detach.drop));
}

function rangeOf(bound: string): { from: string; to: string } {
  const m = /^FOR VALUES FROM \((.*)\) TO \((.*)\)$/.exec(bound);
  return m ? { from: m[1] ?? '', to: m[2] ?? '' } : { from: bound.replace(/^FOR VALUES /, ''), to: '' };
}

// --- MySQL / MariaDB ---

const mySelected = computed(() => myParts.value.find((p) => p.name === selected.value) ?? null);
const myAdd = reactive({ name: '', values: '', count: 1 });
const hashLike = computed(() => ['hash', 'key', 'linear hash', 'linear key'].includes(strategy.value));
const largest = computed(() => Math.max(1, ...myParts.value.map((p) => (p.dataLength ?? 0) + (p.indexLength ?? 0))));

function myDb(): string {
  return ctx.draft.database;
}

function myAddPartition(): void {
  if (hashLike.value) {
    ctx.runNow(`Add ${myAdd.count} partitions`, [myPartitionChange(myDb(), ctx.draft.table, { op: 'addPartitionCount', count: Number(myAdd.count) }, ctx.draft.systemVersioned)]);
    return;
  }
  const values = splitList(myAdd.values).map(myBoundValue);
  const def: MyPartitionDef = {
    name: myAdd.name.trim(),
    values: strategy.value.startsWith('range') ? { kind: 'lessThan', bounds: values } : { kind: 'in', rows: values.map((v) => [v]) },
  };
  ctx.runNow(`Add partition ${def.name}`, [myPartitionChange(myDb(), ctx.draft.table, { op: 'addPartitions', partitions: [def] }, ctx.draft.systemVersioned)]);
}

function myAction(kind: 'drop' | 'truncate' | 'coalesce' | 'remove'): void {
  const p = mySelected.value;
  const table = ctx.draft.table;
  if (kind === 'remove') {
    ctx.runNow(`Remove partitioning of ${table}`, [myPartitionChange(myDb(), table, { op: 'removePartitioning' }, ctx.draft.systemVersioned)]);
  } else if (kind === 'coalesce') {
    ctx.runNow(`Coalesce 1 partition of ${table}`, [myPartitionChange(myDb(), table, { op: 'coalescePartitions', count: 1 }, ctx.draft.systemVersioned)]);
  } else if (p) {
    const op = kind === 'drop' ? { op: 'dropPartitions' as const, names: [p.name] } : { op: 'truncatePartitions' as const, names: [p.name] };
    ctx.runNow(`${kind === 'drop' ? 'Drop' : 'Truncate'} partition ${p.name}`, [myPartitionChange(myDb(), table, op, ctx.draft.systemVersioned)]);
  }
}

/**
 * REORGANIZE PARTITION (range and list): the partitions it replaces and the
 * new ones, which split, merge or move the bounds. It starts on the
 * selected partition, unchanged.
 */
const reorg = reactive<{ names: string[]; into: MyReorganizeTarget[] }>({ names: [], into: [] });
watch(
  mySelected,
  (p) => {
    reorg.names = p ? [p.name] : [];
    reorg.into = p ? [{ name: p.name, values: p.description ?? '' }] : [];
  },
  { immediate: true },
);
const reorgPlan = computed(() => myReorganize(strategy.value, reorg.names, reorg.into, myParts.value));

function toggleReorgName(name: string, on: boolean): void {
  const names = new Set(reorg.names);
  if (on) names.add(name);
  else names.delete(name);
  // In partition order, as REORGANIZE PARTITION needs adjacent range partitions listed.
  reorg.names = myParts.value.map((p) => p.name).filter((n) => names.has(n));
}

function myReorganizePartitions(): void {
  const op = reorgPlan.value.op;
  if (!op) return;
  ctx.runNow(`Reorganize partition ${reorg.names.join(', ')}`, [myPartitionChange(myDb(), ctx.draft.table, op, ctx.draft.systemVersioned)]);
}

/** The named subpartitions of a partition, for the list. */
function subpartitionNames(p: MyPartitionInfo): string {
  return p.subpartitions.map((s) => s.name).join(', ');
}

const partitionBy = reactive({
  kind: 'range' as 'range' | 'rangeColumns' | 'list' | 'listColumns' | 'hash' | 'key',
  expression: '',
  count: 4,
  firstName: 'p0',
  firstValues: 'MAXVALUE',
  /** SUBPARTITION BY (range and list only). */
  sub: 'none' as 'none' | 'hash' | 'key',
  subText: '',
  subCount: 2,
});
const subpartitionable = computed(() => partitionBy.kind !== 'hash' && partitionBy.kind !== 'key');

function myPartitionBy(): void {
  const kind = partitionBy.kind;
  const columns = splitList(partitionBy.expression);
  const method =
    kind === 'range' || kind === 'list'
      ? { kind, expression: partitionBy.expression.trim() }
      : kind === 'rangeColumns' || kind === 'listColumns'
        ? { kind, columns }
        : kind === 'hash'
          ? { kind, expression: partitionBy.expression.trim() }
          : { kind, columns };
  const values = splitList(partitionBy.firstValues).map(myBoundValue);
  const partitions: MyPartitionDef[] =
    kind === 'hash' || kind === 'key'
      ? []
      : [{ name: partitionBy.firstName.trim(), values: kind.startsWith('range') ? { kind: 'lessThan', bounds: values } : { kind: 'in', rows: values.map((v) => [v]) } }];
  ctx.runNow(`Partition ${ctx.draft.table}`, [
    myPartitionChange(
      myDb(),
      ctx.draft.table,
      {
        op: 'partitionBy',
        partitioning: {
          method,
          count: kind === 'hash' || kind === 'key' ? Number(partitionBy.count) : null,
          subpartition: subpartitionable.value ? mySubpartitioning(partitionBy.sub, partitionBy.subText, Number(partitionBy.subCount)) : null,
          partitions,
        },
      },
      ctx.draft.systemVersioned,
    ),
  ]);
}
</script>

<template>
  <div class="section">
    <!-- PostgreSQL -->
    <template v-if="pgInfo">
      <div
        v-if="pgInfo.partitionOf"
        class="empty"
      >
        <p>
          This table is a partition of <span class="mono">{{ pgInfo.partitionOf.parent.schema }}.{{ pgInfo.partitionOf.parent.name }}</span>:
          <span class="mono">{{ pgInfo.partitionOf.bound }}</span>.
        </p>
        <p class="subtle">
          Open the parent table to attach or detach partitions.
        </p>
      </div>
      <div
        v-else-if="!pgInfo.partitioning"
        class="empty"
      >
        <p>This table is not partitioned.</p>
        <p class="subtle">
          PostgreSQL cannot partition an existing table. Create a partitioned table, then attach this table to it or copy the rows over.
        </p>
      </div>
      <template v-else>
        <div class="toolbar">
          <span>Partition key <span class="key mono"><span class="strat">{{ pgInfo.partitioning.strategy.toUpperCase() }}</span> {{ pgKeyText }}</span></span>
          <span class="subtle">{{ pgParts.length }} partitions{{ defaultPartition ? ' · default partition present' : '' }}</span>
        </div>
        <div class="split">
          <div class="scroll">
            <table
              class="list"
              :aria-label="`Partitions of ${ctx.draft.schema}.${ctx.draft.table}`"
            >
              <thead>
                <tr>
                  <th scope="col">
                    Partition
                  </th>
                  <th scope="col">
                    {{ strategy === 'range' ? 'FROM' : 'Bound' }}
                  </th>
                  <th
                    v-if="strategy === 'range'"
                    scope="col"
                  >
                    TO
                  </th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="p in pgParts"
                  :key="`${p.schema}.${p.name}`"
                  :class="{ selected: selected === `${p.schema}.${p.name}` }"
                  :aria-selected="selected === `${p.schema}.${p.name}`"
                  tabindex="0"
                  @click="selected = `${p.schema}.${p.name}`"
                  @keydown.enter="selected = `${p.schema}.${p.name}`"
                >
                  <td class="mono">
                    {{ p.name }}
                    <span
                      v-if="p.isDefault"
                      class="tag tone-warning"
                    >DEFAULT</span>
                    <span
                      v-if="p.partitioned"
                      class="tag tone-info"
                    >PARTITIONED</span>
                    <span
                      v-if="p.detachPending"
                      class="tag tone-danger"
                    >DETACH PENDING</span>
                  </td>
                  <template v-if="strategy === 'range' && !p.isDefault">
                    <td class="mono">
                      {{ rangeOf(p.bound).from }}
                    </td>
                    <td class="mono">
                      {{ rangeOf(p.bound).to }}
                    </td>
                  </template>
                  <td
                    v-else
                    class="mono"
                    :colspan="strategy === 'range' ? 2 : 1"
                  >
                    {{ p.isDefault ? 'DEFAULT' : p.bound.replace(/^FOR VALUES /, '') }}
                  </td>
                </tr>
              </tbody>
            </table>
          </div>

          <aside
            class="actions"
            aria-label="Partition actions"
          >
            <template v-if="strategy === 'range'">
              <div class="head">
                <span class="strong">Create next partitions</span>
              </div>
              <div class="grid2">
                <div class="field">
                  <label
                    class="label"
                    :for="`${uid}-count`"
                  >Count</label>
                  <input
                    :id="`${uid}-count`"
                    v-model.number="next.count"
                    class="input"
                    type="number"
                    min="1"
                    max="48"
                  >
                </div>
                <div class="field">
                  <label
                    class="label"
                    :for="`${uid}-int`"
                  >Interval</label>
                  <select
                    :id="`${uid}-int`"
                    v-model="next.interval"
                    class="select"
                  >
                    <option value="month">
                      1 month
                    </option>
                    <option value="week">
                      1 week
                    </option>
                    <option value="day">
                      1 day
                    </option>
                  </select>
                </div>
              </div>
              <div class="grid2">
                <div class="field">
                  <label
                    class="label"
                    :for="`${uid}-pat`"
                  >Name pattern</label>
                  <input
                    :id="`${uid}-pat`"
                    v-model="next.pattern"
                    class="input mono"
                    spellcheck="false"
                  >
                </div>
                <div class="field">
                  <label
                    class="label"
                    :for="`${uid}-ts`"
                  >Tablespace</label>
                  <input
                    :id="`${uid}-ts`"
                    v-model="next.tablespace"
                    class="input mono"
                    placeholder="default"
                  >
                </div>
              </div>
              <div
                v-if="proposals.length > 0"
                class="preview"
              >
                <span class="label">Preview</span>
                <div
                  v-for="p in proposals"
                  :key="p.name"
                  class="prop mono"
                >
                  <span class="plus">+ {{ p.name }}</span><span class="subtle">{{ p.from }} to {{ p.to }}</span>
                </div>
              </div>
              <p
                v-else
                class="subtle small"
              >
                Next partitions are proposed for a single date or timestamp key whose bounds Varve can read. Use the form below otherwise.
              </p>
              <p
                v-if="defaultPartition && proposals.length > 0"
                class="notice tone-info"
              >
                Each CREATE scans {{ defaultPartition.name }} for rows in the new range and fails when one is there.
              </p>
              <button
                type="button"
                class="btn primary"
                :disabled="ctx.readOnly || proposals.length === 0"
                @click="createNext"
              >
                Create {{ proposals.length }} partitions
              </button>
              <hr>
            </template>

            <div class="head">
              <span class="strong">{{ create.attach ? 'Attach a table' : 'Create a partition' }}</span>
              <label class="inline small"><input
                v-model="create.attach"
                type="checkbox"
              >Attach an existing table</label>
            </div>
            <div
              v-if="create.attach"
              class="grid2"
            >
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-as`"
                >Schema</label>
                <input
                  :id="`${uid}-as`"
                  v-model="create.attachSchema"
                  class="input mono"
                  :placeholder="ctx.draft.schema"
                >
              </div>
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-at`"
                >Table</label>
                <input
                  :id="`${uid}-at`"
                  v-model="create.attachTable"
                  class="input mono"
                >
              </div>
            </div>
            <div
              v-else
              class="field"
            >
              <label
                class="label"
                :for="`${uid}-cn`"
              >Name</label>
              <input
                :id="`${uid}-cn`"
                v-model="create.name"
                class="input mono"
                spellcheck="false"
              >
            </div>
            <div class="field">
              <label
                class="label"
                :for="`${uid}-ck`"
              >Bound</label>
              <select
                :id="`${uid}-ck`"
                v-model="create.kind"
                class="select"
              >
                <option
                  v-if="strategy === 'range'"
                  value="range"
                >
                  FROM ... TO
                </option>
                <option
                  v-if="strategy === 'list'"
                  value="list"
                >
                  IN (...)
                </option>
                <option
                  v-if="strategy === 'hash'"
                  value="hash"
                >
                  MODULUS, REMAINDER
                </option>
                <option
                  v-if="strategy !== 'hash'"
                  value="default"
                >
                  DEFAULT
                </option>
              </select>
            </div>
            <div
              v-if="create.kind === 'range'"
              class="grid2"
            >
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-from`"
                >From</label>
                <input
                  :id="`${uid}-from`"
                  v-model="create.from"
                  class="input mono"
                  placeholder="'2027-01-01' or MINVALUE"
                >
              </div>
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-to`"
                >To</label>
                <input
                  :id="`${uid}-to`"
                  v-model="create.to"
                  class="input mono"
                  placeholder="'2027-02-01' or MAXVALUE"
                >
              </div>
            </div>
            <div
              v-if="create.kind === 'list'"
              class="field"
            >
              <label
                class="label"
                :for="`${uid}-vals`"
              >Values, separated by commas</label>
              <input
                :id="`${uid}-vals`"
                v-model="create.values"
                class="input mono"
                placeholder="'nl', 'be'"
              >
            </div>
            <div
              v-if="create.kind === 'hash'"
              class="grid2"
            >
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-mod`"
                >Modulus</label>
                <input
                  :id="`${uid}-mod`"
                  v-model.number="create.modulus"
                  class="input"
                  type="number"
                  min="1"
                >
              </div>
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-rem`"
                >Remainder</label>
                <input
                  :id="`${uid}-rem`"
                  v-model.number="create.remainder"
                  class="input"
                  type="number"
                  min="0"
                >
              </div>
            </div>
            <p class="subtle small">
              Bound values are written as literals; multi-column keys take one value per column, separated by commas.
            </p>
            <p
              v-if="boundProblem"
              class="notice tone-warning"
              role="alert"
            >
              {{ boundProblem }}
            </p>
            <button
              type="button"
              class="btn"
              :disabled="ctx.readOnly || !createReady"
              @click="createOne"
            >
              {{ create.attach ? 'Attach table' : 'Create partition' }}
            </button>

            <template v-if="pgSelected">
              <hr>
              <div class="head">
                <span class="strong">Detach <span class="mono">{{ pgSelected.name }}</span></span>
              </div>
              <label class="inline">
                <input
                  v-model="detach.concurrently"
                  type="checkbox"
                  :disabled="concurrentDetachProblem !== null"
                >
                CONCURRENTLY <span class="subtle">{{ concurrentDetachProblem ? `(${concurrentDetachProblem})` : '(no lock on readers; runs outside a transaction)' }}</span>
              </label>
              <div
                role="radiogroup"
                aria-label="After detaching"
                class="radios"
              >
                <label class="inline"><input
                  v-model="detach.drop"
                  type="radio"
                  :value="false"
                >Keep as standalone table</label>
                <label class="inline"><input
                  v-model="detach.drop"
                  type="radio"
                  :value="true"
                >Drop it</label>
              </div>
              <button
                type="button"
                class="btn"
                :class="{ danger: detach.drop }"
                :disabled="ctx.readOnly"
                @click="detachSelected"
              >
                {{ detach.drop ? 'Detach and drop' : 'Detach partition' }}
              </button>
            </template>
          </aside>
        </div>
      </template>
    </template>

    <!-- MySQL / MariaDB -->
    <template v-else-if="myInfo">
      <div
        v-if="!myInfo.partitioning"
        class="empty form"
      >
        <p>This table is not partitioned. Partitioning it rebuilds the table.</p>
        <div class="grid3">
          <div class="field">
            <label
              class="label"
              :for="`${uid}-pm`"
            >Method</label>
            <select
              :id="`${uid}-pm`"
              v-model="partitionBy.kind"
              class="select"
            >
              <option value="range">
                RANGE (expression)
              </option>
              <option value="rangeColumns">
                RANGE COLUMNS
              </option>
              <option value="list">
                LIST (expression)
              </option>
              <option value="listColumns">
                LIST COLUMNS
              </option>
              <option value="hash">
                HASH (expression)
              </option>
              <option value="key">
                KEY (columns)
              </option>
            </select>
          </div>
          <div class="field">
            <label
              class="label"
              :for="`${uid}-pe`"
            >{{ partitionBy.kind.endsWith('olumns') || partitionBy.kind === 'key' ? 'Columns' : 'Expression' }}</label>
            <input
              :id="`${uid}-pe`"
              v-model="partitionBy.expression"
              class="input mono"
              spellcheck="false"
            >
          </div>
          <div
            v-if="partitionBy.kind === 'hash' || partitionBy.kind === 'key'"
            class="field"
          >
            <label
              class="label"
              :for="`${uid}-pc`"
            >Partitions</label>
            <input
              :id="`${uid}-pc`"
              v-model.number="partitionBy.count"
              class="input"
              type="number"
              min="1"
            >
          </div>
        </div>
        <div
          v-if="partitionBy.kind !== 'hash' && partitionBy.kind !== 'key'"
          class="grid3"
        >
          <div class="field">
            <label
              class="label"
              :for="`${uid}-fn`"
            >First partition</label>
            <input
              :id="`${uid}-fn`"
              v-model="partitionBy.firstName"
              class="input mono"
            >
          </div>
          <div class="field">
            <label
              class="label"
              :for="`${uid}-fv`"
            >{{ partitionBy.kind.startsWith('range') ? 'LESS THAN' : 'IN' }}</label>
            <input
              :id="`${uid}-fv`"
              v-model="partitionBy.firstValues"
              class="input mono"
            >
          </div>
        </div>
        <div
          v-if="subpartitionable"
          class="grid3"
        >
          <div class="field">
            <label
              class="label"
              :for="`${uid}-sm`"
            >Subpartition by</label>
            <select
              :id="`${uid}-sm`"
              v-model="partitionBy.sub"
              class="select"
            >
              <option value="none">
                No subpartitions
              </option>
              <option value="hash">
                HASH (expression)
              </option>
              <option value="key">
                KEY (columns)
              </option>
            </select>
          </div>
          <template v-if="partitionBy.sub !== 'none'">
            <div class="field">
              <label
                class="label"
                :for="`${uid}-se`"
              >{{ partitionBy.sub === 'key' ? 'Subpartition columns' : 'Subpartition expression' }}</label>
              <input
                :id="`${uid}-se`"
                v-model="partitionBy.subText"
                class="input mono"
                spellcheck="false"
                :placeholder="partitionBy.sub === 'key' ? 'all primary key columns' : ''"
              >
            </div>
            <div class="field">
              <label
                class="label"
                :for="`${uid}-sc`"
              >Subpartitions per partition</label>
              <input
                :id="`${uid}-sc`"
                v-model.number="partitionBy.subCount"
                class="input"
                type="number"
                min="1"
              >
            </div>
          </template>
        </div>
        <button
          type="button"
          class="btn primary"
          :disabled="ctx.readOnly || (partitionBy.kind !== 'key' && !partitionBy.expression.trim()) || (subpartitionable && partitionBy.sub === 'hash' && !partitionBy.subText.trim())"
          @click="myPartitionBy"
        >
          Partition table
        </button>
      </div>
      <template v-else>
        <div class="toolbar">
          <span>Partitioned by <span class="key mono"><span class="strat">{{ myInfo.partitioning.method.toUpperCase() }}</span> ({{ myInfo.partitioning.expression }})</span></span>
          <span class="subtle">{{ myParts.length }} partitions</span>
          <span
            v-if="myInfo.partitioning.subpartitionMethod"
            class="subtle"
          >subpartitioned by <span class="mono">{{ myInfo.partitioning.subpartitionMethod.toUpperCase() }} ({{ myInfo.partitioning.subpartitionExpression ?? '' }})</span></span>
          <span class="spacer" />
          <button
            type="button"
            class="btn compact"
            :disabled="ctx.readOnly"
            @click="myAction('remove')"
          >
            Remove partitioning
          </button>
        </div>
        <div class="split">
          <div class="scroll">
            <table
              class="list"
              :aria-label="`Partitions of ${ctx.draft.database}.${ctx.draft.table}`"
            >
              <thead>
                <tr>
                  <th scope="col">
                    Partition
                  </th>
                  <th scope="col">
                    Values
                  </th>
                  <th
                    v-if="myInfo.partitioning.subpartitionMethod"
                    scope="col"
                  >
                    Subpartitions
                  </th>
                  <th scope="col">
                    Rows (est.)
                  </th>
                  <th scope="col">
                    Size
                  </th>
                  <th scope="col">
                    Size vs largest
                  </th>
                </tr>
              </thead>
              <tbody>
                <tr
                  v-for="p in myParts"
                  :key="p.name"
                  :class="{ selected: selected === p.name }"
                  :aria-selected="selected === p.name"
                  tabindex="0"
                  @click="selected = p.name"
                  @keydown.enter="selected = p.name"
                >
                  <td class="mono">
                    {{ p.name }}
                  </td>
                  <td class="mono">
                    {{ p.description ?? '' }}
                  </td>
                  <td
                    v-if="myInfo.partitioning.subpartitionMethod"
                    class="mono"
                  >
                    {{ subpartitionNames(p) }}
                  </td>
                  <td class="mono">
                    {{ p.rows === null ? '' : formatCount(p.rows) }}
                  </td>
                  <td class="mono">
                    {{ p.dataLength === null ? '' : formatBytes((p.dataLength ?? 0) + (p.indexLength ?? 0)) }}
                  </td>
                  <td>
                    <span class="bar"><span :style="{ width: `${(((p.dataLength ?? 0) + (p.indexLength ?? 0)) / largest) * 100}%` }" /></span>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
          <aside
            class="actions"
            aria-label="Partition actions"
          >
            <div class="head">
              <span class="strong">Add {{ hashLike ? 'partitions' : 'a partition' }}</span>
            </div>
            <template v-if="hashLike">
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-ac`"
                >How many</label>
                <input
                  :id="`${uid}-ac`"
                  v-model.number="myAdd.count"
                  class="input"
                  type="number"
                  min="1"
                >
              </div>
            </template>
            <template v-else>
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-an`"
                >Name</label>
                <input
                  :id="`${uid}-an`"
                  v-model="myAdd.name"
                  class="input mono"
                >
              </div>
              <div class="field">
                <label
                  class="label"
                  :for="`${uid}-av`"
                >{{ strategy.startsWith('range') ? 'LESS THAN' : 'IN' }}</label>
                <input
                  :id="`${uid}-av`"
                  v-model="myAdd.values"
                  class="input mono"
                  :placeholder="strategy.startsWith('range') ? 'MAXVALUE' : '1, 2, 3'"
                >
              </div>
            </template>
            <button
              type="button"
              class="btn"
              :disabled="ctx.readOnly || (!hashLike && (!myAdd.name.trim() || !myAdd.values.trim()))"
              @click="myAddPartition"
            >
              Add
            </button>
            <template v-if="hashLike">
              <button
                type="button"
                class="btn"
                :disabled="ctx.readOnly || myParts.length < 2"
                @click="myAction('coalesce')"
              >
                Coalesce one partition
              </button>
            </template>
            <template v-if="mySelected">
              <hr>
              <div class="head">
                <span class="strong mono">{{ mySelected.name }}</span>
              </div>
              <button
                type="button"
                class="btn"
                :disabled="ctx.readOnly"
                @click="myAction('truncate')"
              >
                Truncate partition
              </button>
              <button
                v-if="!hashLike"
                type="button"
                class="btn danger"
                :disabled="ctx.readOnly"
                @click="myAction('drop')"
              >
                Drop partition and its rows
              </button>
              <template v-if="!hashLike">
                <hr>
                <div class="head">
                  <span class="strong">Reorganize</span>
                </div>
                <p class="subtle small">
                  Splits, merges or moves the bounds of partitions; their rows are copied into the new partitions.
                  {{ strategy.startsWith('list')
                    ? 'On LIST partitioning the server deletes rows whose value no new partition lists, so every old value must be in a new partition.'
                    : 'The new partitions must cover the same range.' }}
                </p>
                <fieldset class="field plain">
                  <legend class="label">
                    Partitions to replace
                  </legend>
                  <label
                    v-for="p in myParts"
                    :key="p.name"
                    class="inline"
                  >
                    <input
                      type="checkbox"
                      :checked="reorg.names.includes(p.name)"
                      @change="toggleReorgName(p.name, ($event.target as HTMLInputElement).checked)"
                    >
                    <span class="mono">{{ p.name }}</span>
                  </label>
                </fieldset>
                <div
                  v-for="(t, i) in reorg.into"
                  :key="i"
                  class="grid2 reorg-row"
                >
                  <input
                    v-model="t.name"
                    class="input mono"
                    :aria-label="`Name of new partition ${i + 1}`"
                    placeholder="name"
                    spellcheck="false"
                  >
                  <span class="reorg-values">
                    <input
                      v-model="t.values"
                      class="input mono"
                      :aria-label="`${strategy.startsWith('range') ? 'LESS THAN' : 'IN'} of new partition ${i + 1}`"
                      :placeholder="strategy.startsWith('range') ? 'LESS THAN' : 'IN'"
                      spellcheck="false"
                    >
                    <button
                      type="button"
                      class="icon-btn"
                      :aria-label="`Remove new partition ${i + 1}`"
                      :disabled="reorg.into.length < 2"
                      @click="reorg.into.splice(i, 1)"
                    >
                      <Icon name="close" />
                    </button>
                  </span>
                </div>
                <button
                  type="button"
                  class="btn compact"
                  @click="reorg.into.push({ name: '', values: '' })"
                >
                  Add new partition
                </button>
                <p
                  v-if="reorgPlan.errors.length > 0"
                  class="subtle small"
                >
                  {{ reorgPlan.errors[0] }}
                </p>
                <button
                  type="button"
                  class="btn"
                  :disabled="ctx.readOnly || reorgPlan.op === null"
                  @click="myReorganizePartitions"
                >
                  Reorganize partitions
                </button>
              </template>
            </template>
          </aside>
        </div>
      </template>
    </template>
  </div>
</template>

<style scoped>
.section {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}

.toolbar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 12px;
  height: 44px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border-inner);
  font-size: 13px;
}

.spacer {
  flex: 1;
}

.key {
  margin-left: 6px;
  padding: 2px 6px;
  border-radius: var(--radius-control);
  background: var(--surface-raised);
  font-size: 12px;
}

.strat {
  color: var(--code-keyword);
}

.split {
  display: flex;
  flex: 1;
  min-height: 0;
}

.scroll {
  flex: 1;
  min-width: 0;
  overflow: auto;
}

.list {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.list th {
  position: sticky;
  top: 0;
  height: 28px;
  padding: 0 8px;
  border-bottom: 1px solid var(--border);
  background: var(--surface-raised);
  color: var(--text-muted);
  font-weight: 500;
  text-align: left;
}

.list td {
  height: 26px;
  padding: 0 8px;
  border-bottom: 1px solid var(--border-inner);
  cursor: pointer;
}

.list tr.selected td {
  background: var(--accent-tint);
  font-weight: 500;
}

.list tr.selected td:first-child {
  box-shadow: inset 2px 0 0 var(--accent);
}

.tag {
  margin-left: 6px;
  padding: 0 5px;
  border-radius: 3px;
  font-family: var(--font-sans);
  font-size: 10px;
  font-weight: 600;
}

.bar {
  display: block;
  width: 120px;
  height: 6px;
  border-radius: 3px;
  background: var(--surface-raised);
  overflow: hidden;
}

.bar span {
  display: block;
  height: 100%;
  background: var(--accent);
}

.actions {
  display: flex;
  flex-direction: column;
  flex-shrink: 0;
  gap: 10px;
  width: 320px;
  padding: 16px;
  overflow: auto;
  border-left: 1px solid var(--border);
}

.actions hr {
  width: 100%;
  margin: 6px 0;
  border: 0;
  border-top: 1px solid var(--border-inner);
}

.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}

.strong {
  font-weight: 600;
}

.grid2 {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 10px;
}

.grid3 {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 10px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.preview {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.prop {
  display: flex;
  justify-content: space-between;
  font-size: 12px;
}

.plus {
  color: var(--success-text);
}

.inline {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
}

.radios {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.small {
  font-size: 12px;
}

.empty {
  padding: 24px;
  color: var(--text-muted);
}

.empty.form {
  display: flex;
  flex-direction: column;
  gap: 12px;
  max-width: 760px;
}

.empty p {
  margin: 0 0 6px;
}

.notice {
  margin: 0;
}

.plain {
  margin: 0;
  padding: 0;
  border: 0;
}

.reorg-row {
  align-items: center;
}

.reorg-values {
  display: flex;
  align-items: center;
  gap: 4px;
}
</style>
