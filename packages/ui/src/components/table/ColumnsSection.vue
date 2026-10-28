<script setup lang="ts">
import { computed, ref, useId } from 'vue';
import { isColumnModified, moveColumn, newColumn } from '../../lib/gridDesigner';
import type { ColumnDraft, DefaultKind } from '../../lib/gridDesigner';
import Icon from '../common/Icon.vue';
import { useDesigner } from './designerContext';

/** Designer section: the table's columns, edited in a grid of inputs (canvas TableDesigner-Columns). */
const ctx = useDesigner();
const pg = computed(() => ctx.draft.engine === 'postgres');
const selected = ref<string | null>(null);
const listId = useId();

const PG_TYPES = ['text', 'varchar(255)', 'integer', 'bigint', 'smallint', 'numeric(12,2)', 'boolean', 'timestamptz', 'timestamp', 'date', 'time', 'interval', 'jsonb', 'json', 'uuid', 'bytea', 'text[]', 'integer[]', 'inet', 'double precision', 'real'];
const MY_TYPES = ['varchar(255)', 'char(36)', 'text', 'longtext', 'int', 'int unsigned', 'bigint', 'bigint unsigned', 'tinyint', 'smallint', 'decimal(12,2)', 'double', 'float', 'boolean', 'datetime', 'datetime(3)', 'timestamp', 'date', 'time', 'year', 'json', 'blob', 'varbinary(255)', "enum('a','b')", 'bit(1)'];
const types = computed(() => (pg.value ? PG_TYPES : MY_TYPES));

const visible = computed(() => ctx.draft.columns);
const current = computed(() => ctx.draft.columns.find((c) => c.id === selected.value) ?? null);

/** MySQL / MariaDB ON UPDATE CURRENT_TIMESTAMP: none, or its fractional seconds precision ('0' writes no precision). */
const ON_UPDATE: { value: string; label: string }[] = [
  { value: '', label: 'none' },
  { value: '0', label: 'CURRENT_TIMESTAMP' },
  ...[1, 2, 3, 4, 5, 6].map((p) => ({ value: String(p), label: `CURRENT_TIMESTAMP(${p})` })),
];

/** ON UPDATE CURRENT_TIMESTAMP is only valid on TIMESTAMP and DATETIME columns (or a column that has it already). */
function takesOnUpdate(c: ColumnDraft): boolean {
  return /^\s*(timestamp|datetime)\b/i.test(c.dataType) || c.onUpdate !== '';
}

const DEFAULT_KINDS: { value: DefaultKind; label: string }[] = [
  { value: 'none', label: 'None' },
  { value: 'null', label: 'NULL' },
  { value: 'literal', label: 'Value' },
  { value: 'expression', label: 'Expression' },
  { value: 'currentTimestamp', label: 'CURRENT_TIMESTAMP' },
];

function state(c: ColumnDraft): 'new' | 'dropped' | 'modified' | '' {
  if (c.origName === null) return 'new';
  if (c.dropped) return 'dropped';
  return isColumnModified(ctx.original, c) ? 'modified' : '';
}

function gutter(c: ColumnDraft): string {
  return { new: '+', dropped: '−', modified: '~', '': '' }[state(c)];
}

function keyTag(c: ColumnDraft): string {
  const name = c.origName ?? c.name;
  if (ctx.draft.constraints.some((k) => k.type === 'primaryKey' && !k.dropped && k.columns.includes(name))) return 'PK';
  if (ctx.draft.constraints.some((k) => k.type === 'foreignKey' && !k.dropped && k.columns.includes(name))) return 'FK';
  return '';
}

function unusedName(): string {
  let n = 1;
  while (ctx.draft.columns.some((c) => c.name === `column_${n}`)) n += 1;
  return `column_${n}`;
}

function add(): void {
  const c = newColumn(ctx.draft.engine, unusedName());
  ctx.draft.columns.push(c);
  selected.value = c.id;
}

function duplicate(): void {
  const src = current.value;
  if (!src) return;
  const c = {
    ...newColumn(ctx.draft.engine, `${src.name}_copy`),
    dataType: src.dataType,
    nullable: src.nullable,
    defaultKind: src.defaultKind,
    defaultText: src.defaultText,
    collation: src.collation,
    comment: src.comment,
    onUpdate: src.onUpdate,
    invisible: src.invisible,
  };
  ctx.draft.columns.splice(ctx.draft.columns.indexOf(src) + 1, 0, c);
  selected.value = c.id;
}

function remove(): void {
  const c = current.value;
  if (!c) return;
  if (c.origName === null) {
    ctx.draft.columns.splice(ctx.draft.columns.indexOf(c), 1);
    selected.value = null;
  } else {
    c.dropped = !c.dropped;
  }
}

function move(by: -1 | 1): void {
  if (current.value) moveColumn(ctx.draft, current.value.id, by);
}

const atTop = computed(() => !current.value || ctx.draft.columns.indexOf(current.value) === 0);
const atBottom = computed(() => !current.value || ctx.draft.columns.indexOf(current.value) === ctx.draft.columns.length - 1);

function wasType(c: ColumnDraft): string {
  if (c.origName === null) return '';
  const was = ctx.original.columns.find((o) => o.id === c.id);
  return was && was.dataType.trim() !== c.dataType.trim() ? was.dataType : '';
}

function disabled(c: ColumnDraft): boolean {
  return ctx.readOnly || c.dropped || c.locked !== null;
}
</script>

<template>
  <div class="section">
    <div class="toolbar">
      <button
        type="button"
        class="btn compact"
        :disabled="ctx.readOnly"
        @click="add"
      >
        <Icon name="plus" />Add column
      </button>
      <button
        type="button"
        class="btn compact"
        :disabled="ctx.readOnly || !current || current.dropped"
        @click="duplicate"
      >
        Duplicate
      </button>
      <button
        type="button"
        class="btn compact"
        :disabled="ctx.readOnly || !current || current.locked !== null"
        @click="remove"
      >
        <Icon name="close" />{{ current?.dropped ? 'Keep' : 'Remove' }}
      </button>
      <span class="sep" />
      <button
        type="button"
        class="icon-btn"
        aria-label="Move column up"
        :disabled="ctx.readOnly || pg || atTop"
        @click="move(-1)"
      >
        <Icon
          name="chevronDown"
          class="flip"
        />
      </button>
      <button
        type="button"
        class="icon-btn"
        aria-label="Move column down"
        :disabled="ctx.readOnly || pg || atBottom"
        @click="move(1)"
      >
        <Icon name="chevronDown" />
      </button>
      <span class="subtle note">{{
        pg
          ? 'PostgreSQL appends new columns at the end and cannot reorder them.'
          : 'Moved columns are written with FIRST or AFTER; moving can rebuild the table.'
      }}</span>
    </div>

    <datalist :id="listId">
      <option
        v-for="t in types"
        :key="t"
        :value="t"
      />
    </datalist>

    <div class="scroll">
      <table
        class="cols"
        :aria-label="`Columns of ${ctx.draft.schema}.${ctx.draft.table}`"
      >
        <thead>
          <tr>
            <th scope="col">
              <span class="visually-hidden">Change</span>
            </th>
            <th scope="col">
              Name
            </th>
            <th scope="col">
              Type
            </th>
            <th scope="col">
              Nullable
            </th>
            <th scope="col">
              Default
            </th>
            <th scope="col">
              {{ pg ? 'Identity' : 'Auto increment' }}
            </th>
            <th scope="col">
              Generated
            </th>
            <th
              v-if="!pg"
              scope="col"
            >
              On update
            </th>
            <th
              v-if="!pg && ctx.invisibleColumns"
              scope="col"
            >
              Invisible
            </th>
            <th scope="col">
              Collation
            </th>
            <th scope="col">
              Comment
            </th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="c in visible"
            :key="c.id"
            :class="[state(c), { selected: selected === c.id }]"
            :aria-selected="selected === c.id"
            :title="c.locked ?? undefined"
            @click="selected = c.id"
            @focusin="selected = c.id"
          >
            <td class="gutter mono">
              {{ gutter(c) }}
            </td>
            <td class="name">
              <input
                v-model="c.name"
                class="cell-input mono"
                :aria-label="`Name of ${c.origName ?? 'new column'}`"
                :disabled="disabled(c)"
                spellcheck="false"
              >
              <span
                v-if="keyTag(c)"
                class="tag"
                :class="keyTag(c) === 'PK' ? 'tone-warning' : 'tone-info'"
              >{{ keyTag(c) }}</span>
              <span
                v-if="state(c) === 'new'"
                class="tag tone-success"
              >NEW</span>
            </td>
            <td>
              <input
                v-model="c.dataType"
                class="cell-input mono"
                :list="listId"
                :aria-label="`Type of ${c.name}`"
                :disabled="disabled(c)"
                spellcheck="false"
              >
              <span
                v-if="wasType(c)"
                class="was mono"
              >was {{ wasType(c) }}</span>
            </td>
            <td class="center">
              <input
                v-model="c.nullable"
                type="checkbox"
                :aria-label="`${c.name} nullable`"
                :disabled="disabled(c)"
              >
            </td>
            <td class="default">
              <select
                v-if="!pg"
                v-model="c.defaultKind"
                class="cell-select"
                :aria-label="`Default kind of ${c.name}`"
                :disabled="disabled(c)"
              >
                <option
                  v-for="k in DEFAULT_KINDS"
                  :key="k.value"
                  :value="k.value"
                >
                  {{ k.label }}
                </option>
              </select>
              <input
                v-if="pg || ['literal', 'expression', 'currentTimestamp'].includes(c.defaultKind)"
                v-model="c.defaultText"
                class="cell-input mono"
                :aria-label="pg ? `Default expression of ${c.name}` : `Default of ${c.name}`"
                :placeholder="pg ? 'none' : c.defaultKind === 'currentTimestamp' ? 'precision' : ''"
                :disabled="disabled(c) || (pg && (c.identity !== 'none' || c.generated.trim() !== ''))"
                spellcheck="false"
                @input="pg && (c.defaultKind = c.defaultText.trim() ? 'expression' : 'none')"
              >
            </td>
            <td>
              <select
                v-if="pg"
                v-model="c.identity"
                class="cell-select"
                :aria-label="`Identity of ${c.name}`"
                :disabled="disabled(c)"
              >
                <option value="none">
                  none
                </option>
                <option value="always">
                  ALWAYS
                </option>
                <option value="byDefault">
                  BY DEFAULT
                </option>
              </select>
              <input
                v-else
                v-model="c.autoIncrement"
                type="checkbox"
                :aria-label="`${c.name} auto increment`"
                :disabled="disabled(c)"
              >
            </td>
            <td>
              <input
                v-model="c.generated"
                class="cell-input mono"
                :aria-label="`Generation expression of ${c.name}`"
                placeholder="none"
                :disabled="disabled(c)"
                spellcheck="false"
              >
              <label
                v-if="c.generated.trim()"
                class="inline"
              >
                <input
                  v-model="c.generatedStored"
                  type="checkbox"
                  :disabled="disabled(c)"
                >stored
              </label>
            </td>
            <td v-if="!pg">
              <select
                v-if="takesOnUpdate(c)"
                v-model="c.onUpdate"
                class="cell-select"
                :aria-label="`On update of ${c.name}`"
                :disabled="disabled(c)"
              >
                <option
                  v-for="o in ON_UPDATE"
                  :key="o.value"
                  :value="o.value"
                >
                  {{ o.label }}
                </option>
              </select>
            </td>
            <td
              v-if="!pg && ctx.invisibleColumns"
              class="center"
            >
              <input
                v-model="c.invisible"
                type="checkbox"
                :aria-label="`${c.name} invisible`"
                title="INVISIBLE: SELECT * leaves the column out; name it to read it."
                :disabled="disabled(c)"
              >
            </td>
            <td>
              <input
                v-model="c.collation"
                class="cell-input mono"
                :aria-label="`Collation of ${c.name}`"
                placeholder="default"
                :disabled="disabled(c)"
                spellcheck="false"
              >
            </td>
            <td>
              <input
                v-model="c.comment"
                class="cell-input"
                :aria-label="`Comment of ${c.name}`"
                :disabled="disabled(c)"
              >
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <div
      v-if="current && pg && current.origName !== null && !current.dropped && current.dataType.trim() !== (ctx.original.columns.find((o) => o.id === current?.id)?.dataType ?? '').trim()"
      class="detail"
    >
      <label
        class="label"
        :for="`${listId}-using`"
      >USING for the type change of {{ current.name }}</label>
      <input
        :id="`${listId}-using`"
        v-model="current.using"
        class="input mono"
        :placeholder="`${current.name}::${current.dataType}`"
        :disabled="ctx.readOnly"
        spellcheck="false"
      >
      <span class="subtle hint">Needed when PostgreSQL has no automatic cast between the old and the new type. A type change can rewrite the table.</span>
    </div>
    <p
      v-if="current?.locked"
      class="notice tone-neutral locked"
    >
      {{ current.name }}: {{ current.locked }}
    </p>
  </div>
</template>

<style scoped>
.section {
  display: flex;
  flex-direction: column;
  min-height: 0;
  height: 100%;
}

.toolbar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 44px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border-inner);
}

.sep {
  width: 1px;
  height: 20px;
  background: var(--border-inner);
}

.flip {
  transform: rotate(180deg);
}

.note {
  font-size: 12px;
}

.scroll {
  flex: 1;
  min-height: 0;
  overflow: auto;
}

.cols {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.cols th {
  position: sticky;
  top: 0;
  z-index: 1;
  height: 28px;
  padding: 0 8px;
  border-bottom: 1px solid var(--border);
  background: var(--sticky);
  color: var(--text-muted);
  font-weight: 500;
  text-align: left;
}

.cols td {
  height: 30px;
  padding: 0 6px;
  border-bottom: 1px solid var(--border-inner);
  vertical-align: middle;
}

.cols tr.selected td {
  background: var(--accent-tint);
}

.cols tr.selected td:first-child {
  box-shadow: inset 2px 0 0 var(--accent);
}

.cols tr.modified td {
  background: var(--warning-tint);
}

.cols tr.modified td:first-child {
  box-shadow: inset 2px 0 0 var(--warning);
}

.cols tr.new td {
  background: var(--success-tint);
}

.cols tr.new td:first-child {
  box-shadow: inset 2px 0 0 var(--success);
}

.cols tr.dropped td {
  background: var(--danger-tint);
  color: var(--danger-text);
  text-decoration: line-through;
}

.gutter {
  width: 20px;
  color: var(--text-subtle);
  text-align: center;
}

.modified .gutter {
  color: var(--warning-text);
}

.new .gutter {
  color: var(--success-text);
}

.name {
  display: flex;
  align-items: center;
  gap: 6px;
  height: 30px;
}

.center {
  text-align: center;
}

.cell-input,
.cell-select {
  width: 100%;
  min-width: 80px;
  height: 24px;
  padding: 0 6px;
  border: 1px solid transparent;
  border-radius: var(--radius-control);
  background: transparent;
  color: inherit;
  font-size: 12px;
}

.cell-input:hover:not(:disabled),
.cell-select:hover:not(:disabled) {
  border-color: var(--border);
}

.cell-input:focus,
.cell-select:focus {
  outline: none;
  border-color: var(--accent);
  background: var(--surface);
  box-shadow: 0 0 0 3px var(--focus-ring);
}

.default {
  display: flex;
  align-items: center;
  gap: 4px;
  height: 30px;
}

.default .cell-select {
  min-width: 96px;
  width: auto;
}

.tag {
  flex-shrink: 0;
  padding: 0 5px;
  border-radius: 3px;
  font-size: 10px;
  font-weight: 600;
}

.was {
  display: block;
  color: var(--warning-text);
  font-size: 11px;
}

.inline {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  color: var(--text-muted);
}

.detail {
  display: flex;
  flex-shrink: 0;
  flex-direction: column;
  gap: 6px;
  padding: 12px;
  border-top: 1px solid var(--border-inner);
}

.hint {
  font-size: 12px;
}

.locked {
  flex-shrink: 0;
  margin: 8px 12px;
}
</style>
