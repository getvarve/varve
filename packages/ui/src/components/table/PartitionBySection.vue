<script setup lang="ts">
import { computed, useId, watch } from 'vue';
import type { PgPartitionStrategy } from '../../api/grid';
import { defaultPartitionName } from '../../lib/gridDesigner';
import Icon from '../common/Icon.vue';
import { useDesigner } from './designerContext';

/**
 * Designer section of a new PostgreSQL table: PARTITION BY. PostgreSQL
 * cannot partition an existing table, so the choice is made here, before
 * the table is created: strategy (RANGE, LIST, HASH), the key columns or
 * expressions, and whether a DEFAULT partition is created with it.
 * Partitions are added in the Partitions section once the table exists.
 */
const ctx = useDesigner();
const uid = useId();
const by = computed(() => ctx.draft.partitionBy);
const disabled = computed(() => ctx.readOnly);
const columnNames = computed(() => ctx.draft.columns.filter((c) => !c.dropped && c.name.trim() !== '').map((c) => c.name.trim()));

const STRATEGIES: { value: PgPartitionStrategy; label: string; hint: string }[] = [
  { value: 'range', label: 'RANGE', hint: 'Each partition holds a range of key values (FOR VALUES FROM ... TO ...), for example a month of dates.' },
  { value: 'list', label: 'LIST', hint: 'Each partition holds listed values of one key (FOR VALUES IN (...)), for example regions.' },
  { value: 'hash', label: 'HASH', hint: 'Rows are spread by a hash of the key (FOR VALUES WITH (MODULUS m, REMAINDER r)); no default partition.' },
];
const strategyHint = computed(() => STRATEGIES.find((s) => s.value === by.value.strategy)?.hint ?? '');

// The designer marks a partitioned draft (indexes are never built CONCURRENTLY on one).
watch(
  () => by.value.enabled,
  (on) => {
    ctx.draft.partitioned = on;
    if (on && by.value.keys.length === 0) by.value.keys.push({ text: columnNames.value[0] ?? '', expression: false });
  },
);

watch(
  () => by.value.strategy,
  (strategy) => {
    if (strategy === 'hash') by.value.defaultPartition = false;
    if (strategy === 'list' && by.value.keys.length > 1) by.value.keys.splice(1);
  },
);

function addKey(expression: boolean): void {
  by.value.keys.push({ text: expression ? '' : (columnNames.value[0] ?? ''), expression });
}

/** Key columns a primary key or unique constraint is missing (PostgreSQL requires all of them). */
const keyProblems = computed(() => {
  if (!by.value.enabled) return [];
  const columns = by.value.keys.filter((k) => !k.expression && k.text.trim()).map((k) => k.text.trim());
  return ctx.draft.constraints
    .filter((k) => !k.dropped && (k.type === 'primaryKey' || k.type === 'unique'))
    .map((k) => ({ name: k.type === 'primaryKey' ? 'The primary key' : `Unique constraint ${k.name || '(unnamed)'}`, missing: columns.filter((c) => !k.columns.includes(c)) }))
    .concat(
      ctx.draft.indexes
        .filter((i) => !i.dropped && i.unique)
        .map((i) => ({ name: `Unique index ${i.name || '(unnamed)'}`, missing: columns.filter((c) => !i.parts.some((p) => !p.expression && p.text === c)) })),
    )
    .filter((p) => p.missing.length > 0);
});
</script>

<template>
  <div class="section">
    <div class="body">
      <form
        class="form"
        aria-label="Partitioning of the new table"
        @submit.prevent
      >
        <label class="inline check">
          <input
            v-model="by.enabled"
            type="checkbox"
            :disabled="disabled"
          >
          Partitioned table (PARTITION BY)
        </label>
        <p class="hint">
          PostgreSQL cannot partition a table after it is created. A partitioned table holds no rows itself: every row goes to one of its partitions, which you add in the Partitions section once the table exists.
        </p>

        <template v-if="by.enabled">
          <div class="field">
            <span
              :id="`${uid}-strategy`"
              class="label"
            >Strategy</span>
            <div
              class="segmented"
              role="radiogroup"
              :aria-labelledby="`${uid}-strategy`"
            >
              <label
                v-for="s in STRATEGIES"
                :key="s.value"
                class="seg"
                :class="{ on: by.strategy === s.value }"
              >
                <input
                  v-model="by.strategy"
                  type="radio"
                  :name="`${uid}-strategy`"
                  :value="s.value"
                  :disabled="disabled"
                >
                {{ s.label }}
              </label>
            </div>
            <span class="hint">{{ strategyHint }}</span>
          </div>

          <fieldset class="field group">
            <legend class="label">
              Partition key
            </legend>
            <div
              v-for="(k, n) in by.keys"
              :key="n"
              class="row"
            >
              <select
                v-if="!k.expression"
                v-model="k.text"
                class="select mono"
                :aria-label="`Key ${n + 1} column`"
                :disabled="disabled"
              >
                <option
                  v-if="k.text && !columnNames.includes(k.text)"
                  :value="k.text"
                >
                  {{ k.text }} (not a column)
                </option>
                <option
                  v-for="c in columnNames"
                  :key="c"
                  :value="c"
                >
                  {{ c }}
                </option>
              </select>
              <input
                v-else
                v-model="k.text"
                class="input mono"
                :aria-label="`Key ${n + 1} expression`"
                placeholder="date_trunc('month', created_at)"
                :disabled="disabled"
                spellcheck="false"
              >
              <span class="muted kind">{{ k.expression ? 'expression' : 'column' }}</span>
              <button
                type="button"
                class="icon-btn"
                :aria-label="`Remove key ${n + 1}`"
                :disabled="disabled || by.keys.length === 1"
                @click="by.keys.splice(n, 1)"
              >
                <Icon name="close" />
              </button>
            </div>
            <div
              v-if="by.strategy !== 'list'"
              class="row"
            >
              <button
                type="button"
                class="btn compact"
                :disabled="disabled"
                @click="addKey(false)"
              >
                Add column
              </button>
              <button
                type="button"
                class="btn compact"
                :disabled="disabled"
                @click="addKey(true)"
              >
                Add expression
              </button>
            </div>
            <span
              v-else
              class="hint"
            >List partitioning takes exactly one key.</span>
          </fieldset>

          <ul
            v-if="keyProblems.length > 0"
            class="notice tone-warning problems"
            role="note"
          >
            <li
              v-for="problem in keyProblems"
              :key="problem.name"
            >
              {{ problem.name }} must include the partition key column{{ problem.missing.length === 1 ? '' : 's' }} {{ problem.missing.join(', ') }}: PostgreSQL checks unique keys per partition. Change it in Constraints or Indexes.
            </li>
          </ul>

          <div class="field">
            <label class="inline check">
              <input
                v-model="by.defaultPartition"
                type="checkbox"
                :disabled="disabled || by.strategy === 'hash'"
              >
              Also create the default partition <span class="mono">{{ defaultPartitionName(ctx.draft.table.trim() || 'new_table') }}</span>
            </label>
            <span class="hint">{{ by.strategy === 'hash' ? 'Hash partitioning has no default partition.' : 'Takes the rows no other partition accepts. Adding a partition later scans it for rows that belong to the new one.' }}</span>
          </div>
        </template>
      </form>
    </div>
  </div>
</template>

<style scoped>
.section {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}

.body {
  flex: 1;
  min-height: 0;
  overflow: auto;
}

.form {
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 760px;
  padding: 16px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.group {
  margin: 0;
  padding: 0;
  border: 0;
}

.row {
  display: flex;
  align-items: center;
  gap: 6px;
}

.row .input,
.row .select {
  flex: 1;
  min-width: 0;
}

.kind {
  width: 72px;
  font-size: 12px;
}

.inline {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
}

.hint {
  margin: 0;
  color: var(--text-muted);
  font-size: 12px;
}

.segmented {
  display: inline-flex;
  align-self: flex-start;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  overflow: hidden;
}

.seg {
  display: inline-flex;
  align-items: center;
  height: 28px;
  padding: 0 12px;
  border-right: 1px solid var(--border);
  font-size: 12px;
  font-weight: 500;
  cursor: pointer;
}

.seg:last-child {
  border-right: 0;
}

.seg input {
  position: absolute;
  opacity: 0;
  pointer-events: none;
}

.seg.on {
  background: var(--accent-tint);
  color: var(--accent-hover);
}

.seg:has(input:focus-visible) {
  outline: 2px solid var(--accent);
  outline-offset: -2px;
}

.compact {
  height: 26px;
  font-size: 12px;
}

.problems {
  flex-direction: column;
  margin: 0;
  padding-left: 28px;
}
</style>
