<script setup lang="ts">
import { computed, ref, useId } from 'vue';
import { isConstraintModified, newConstraint, pgValidateConstraint } from '../../lib/gridDesigner';
import type { ConstraintDraft, ConstraintType, RefAction } from '../../lib/gridDesigner';
import Icon from '../common/Icon.vue';
import { useDesigner } from './designerContext';

/** Designer section: constraints, with an editor for a new or redefined one (canvas TableDesigner-Constraints). */
const ctx = useDesigner();
const pg = computed(() => ctx.draft.engine === 'postgres');
/** The constraint a tree node asked for, selected when the section opens. */
const selected = ref<string | null>(
  ctx.focus?.section === 'constraints' && ctx.focus.name !== null ? (ctx.draft.constraints.find((k) => k.origName === ctx.focus?.name)?.id ?? null) : null,
);
const addType = ref<ConstraintType>('foreignKey');
const uid = useId();

const TYPE_LABELS: Record<ConstraintType, string> = {
  primaryKey: 'PRIMARY KEY',
  unique: 'UNIQUE',
  check: 'CHECK',
  foreignKey: 'FOREIGN KEY',
  exclusion: 'EXCLUDE',
  notNull: 'NOT NULL',
};
const addable = computed<ConstraintType[]>(() =>
  pg.value ? ['primaryKey', 'unique', 'check', 'foreignKey', 'exclusion'] : ['primaryKey', 'check', 'foreignKey'],
);
const ACTIONS: { value: RefAction; label: string }[] = [
  { value: 'noAction', label: 'NO ACTION' },
  { value: 'restrict', label: 'RESTRICT' },
  { value: 'cascade', label: 'CASCADE' },
  { value: 'setNull', label: 'SET NULL' },
  { value: 'setDefault', label: 'SET DEFAULT' },
];

const current = computed(() => ctx.draft.constraints.find((k) => k.id === selected.value) ?? null);
const editable = computed(() => !!current.value && !ctx.readOnly && !current.value.dropped && (current.value.origName === null || current.value.changed));
const columnNames = computed(() => ctx.draft.columns.filter((c) => !c.dropped).map((c) => c.name));
const hasPrimary = computed(() => ctx.draft.constraints.some((k) => k.type === 'primaryKey' && !k.dropped));

function state(k: ConstraintDraft): 'new' | 'dropped' | 'modified' | '' {
  if (k.origName === null) return 'new';
  if (k.dropped) return 'dropped';
  return isConstraintModified(ctx.original, k) ? 'modified' : '';
}

function definition(k: ConstraintDraft): string {
  if (k.origName !== null && !k.changed) return k.definition;
  switch (k.type) {
    case 'primaryKey':
      return `PRIMARY KEY (${k.columns.join(', ')})`;
    case 'unique':
      return `UNIQUE (${k.columns.join(', ')})`;
    case 'check':
      return `CHECK (${k.expression})`;
    case 'foreignKey':
      return `(${k.columns.join(', ')}) REFERENCES ${k.refSchema ? `${k.refSchema}.` : ''}${k.refTable} (${k.refColumns.join(', ')})`;
    case 'exclusion':
      return `USING ${k.exclusionMethod} (${k.exclusion.map((e) => `${e.text} WITH ${e.operator}`).join(', ')})`;
    case 'notNull':
      return 'NOT NULL';
  }
}

function deferText(k: ConstraintDraft): string {
  if (k.type === 'check') return 'n/a';
  if (!k.deferrable) return 'No';
  return k.initiallyDeferred ? 'Yes, initially deferred' : 'Yes';
}

function add(): void {
  const type = addType.value;
  const base = `${ctx.draft.table}_${{ primaryKey: 'pkey', unique: 'key', check: 'check', foreignKey: 'fkey', exclusion: 'excl', notNull: 'not_null' }[type]}`;
  let name = base;
  let n = 1;
  while (ctx.draft.constraints.some((k) => k.name === name)) {
    n += 1;
    name = `${base}${n}`;
  }
  const k = newConstraint(type, type === 'primaryKey' && !pg.value ? 'PRIMARY' : name);
  if (!pg.value) k.refSchema = ctx.draft.database;
  else k.refSchema = ctx.draft.schema;
  ctx.draft.constraints.push(k);
  selected.value = k.id;
}

function toggleDrop(): void {
  const k = current.value;
  if (!k) return;
  if (k.origName === null) {
    ctx.draft.constraints.splice(ctx.draft.constraints.indexOf(k), 1);
    selected.value = null;
  } else {
    k.dropped = !k.dropped;
  }
}

function toggleColumn(k: ConstraintDraft, name: string): void {
  const at = k.columns.indexOf(name);
  if (at >= 0) {
    k.columns.splice(at, 1);
    k.refColumns.splice(at, 1);
  } else {
    k.columns.push(name);
    k.refColumns.push('');
  }
}

function validate(): void {
  const k = current.value;
  if (k?.origName) ctx.runNow(`Validate ${k.origName}`, [pgValidateConstraint(ctx.draft.schema, ctx.draft.table, k.origName)]);
}

const canRedefine = computed(() => {
  const k = current.value;
  return !!k && k.origName !== null && !k.changed && !k.dropped && !k.locked && k.type !== 'exclusion' && !ctx.readOnly;
});
</script>

<template>
  <div class="section">
    <div class="toolbar">
      <label
        class="visually-hidden"
        :for="`${uid}-type`"
      >Constraint type to add</label>
      <select
        :id="`${uid}-type`"
        v-model="addType"
        class="select compact type"
        :disabled="ctx.readOnly"
      >
        <option
          v-for="t in addable"
          :key="t"
          :value="t"
          :disabled="t === 'primaryKey' && hasPrimary"
        >
          {{ TYPE_LABELS[t] }}
        </option>
      </select>
      <button
        type="button"
        class="btn compact"
        :disabled="ctx.readOnly || (addType === 'primaryKey' && hasPrimary)"
        @click="add"
      >
        <Icon name="plus" />Add constraint
      </button>
      <button
        v-if="pg"
        type="button"
        class="btn compact"
        :disabled="ctx.readOnly || !current || current.origName === null || current.validated || current.dropped"
        @click="validate"
      >
        Validate
      </button>
      <button
        type="button"
        class="btn compact"
        :disabled="ctx.readOnly || !current || current.locked !== null"
        @click="toggleDrop"
      >
        <Icon name="close" />{{ current?.dropped ? 'Keep' : 'Drop' }}
      </button>
      <span
        v-if="!pg"
        class="subtle note"
      >Unique keys are indexes in MySQL and MariaDB: change them under Indexes.</span>
    </div>

    <div class="body">
      <table
        class="list"
        :aria-label="`Constraints on ${ctx.draft.schema}.${ctx.draft.table}`"
      >
        <thead>
          <tr>
            <th scope="col">
              Type
            </th>
            <th scope="col">
              Name
            </th>
            <th scope="col">
              Definition
            </th>
            <th
              v-if="pg"
              scope="col"
            >
              Deferrable
            </th>
            <th scope="col">
              {{ pg ? 'Validated' : 'Enforced' }}
            </th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="k in ctx.draft.constraints"
            :key="k.id"
            :class="[state(k), { selected: selected === k.id }]"
            :aria-selected="selected === k.id"
            :title="k.locked ?? undefined"
            tabindex="0"
            @click="selected = k.id"
            @keydown.enter="selected = k.id"
          >
            <td>
              <span
                class="type"
                :class="`t-${k.type}`"
              >{{ TYPE_LABELS[k.type] }}</span>
            </td>
            <td class="mono">
              {{ k.name }}
              <span
                v-if="state(k) === 'modified'"
                class="tag tone-warning"
              >MODIFIED</span>
              <span
                v-if="state(k) === 'new'"
                class="tag tone-success"
              >NEW</span>
            </td>
            <td
              class="mono def"
              :title="definition(k)"
            >
              {{ definition(k) }}
            </td>
            <td v-if="pg">
              {{ deferText(k) }}
            </td>
            <td>
              <span
                v-if="pg"
                class="pill"
                :class="k.validated ? 'tone-success' : 'tone-warning'"
              ><span class="dot" />{{ k.validated ? 'Valid' : 'Not validated' }}</span>
              <span v-else>{{ k.type === 'check' ? (k.enforced ? 'Yes' : 'No') : 'n/a' }}</span>
            </td>
          </tr>
        </tbody>
      </table>

      <section
        v-if="current"
        class="editor"
        :aria-label="`Constraint ${current.name}`"
      >
        <header class="editor-head">
          <span class="strong">{{ TYPE_LABELS[current.type] }}</span>
          <span class="mono muted">{{ current.name }}</span>
          <span class="spacer" />
          <button
            v-if="canRedefine"
            type="button"
            class="btn compact"
            @click="current.changed = true"
          >
            Redefine
          </button>
        </header>
        <p
          v-if="current.locked"
          class="notice tone-neutral"
        >
          {{ current.locked }}
        </p>
        <p
          v-if="current.changed"
          class="notice tone-warning"
        >
          Saved as DROP CONSTRAINT and ADD CONSTRAINT in one ALTER TABLE{{ pg ? ' transaction' : '' }}. Adding it checks the rows again unless it is NOT VALID.
        </p>

        <div class="grid2">
          <div class="field">
            <label
              class="label"
              :for="`${uid}-name`"
            >Name</label>
            <input
              :id="`${uid}-name`"
              v-model="current.name"
              class="input mono"
              :disabled="ctx.readOnly || current.dropped || current.locked !== null || (!pg && current.origName !== null) || (!pg && current.type === 'primaryKey')"
              spellcheck="false"
            >
          </div>
          <div
            v-if="current.type === 'foreignKey'"
            class="field"
          >
            <label
              class="label"
              :for="`${uid}-ref`"
            >Referenced table ({{ pg ? 'schema' : 'database' }}.table)</label>
            <div class="row">
              <input
                v-model="current.refSchema"
                class="input mono"
                :aria-label="pg ? 'Referenced schema' : 'Referenced database'"
                :disabled="!editable"
                spellcheck="false"
              >
              <input
                :id="`${uid}-ref`"
                v-model="current.refTable"
                class="input mono"
                aria-label="Referenced table"
                :disabled="!editable"
                spellcheck="false"
              >
            </div>
          </div>
        </div>

        <div
          v-if="['primaryKey', 'unique', 'foreignKey'].includes(current.type)"
          class="field"
        >
          <span class="label">{{ current.type === 'foreignKey' ? 'Column mapping' : 'Columns' }}</span>
          <div class="chips">
            <button
              v-for="c in columnNames"
              :key="c"
              type="button"
              class="chip mono"
              :aria-pressed="current.columns.includes(c)"
              :disabled="!editable"
              @click="toggleColumn(current, c)"
            >
              {{ c }}
            </button>
          </div>
          <table
            v-if="current.type === 'foreignKey' && current.columns.length > 0"
            class="mapping"
          >
            <thead>
              <tr>
                <th scope="col">
                  {{ ctx.draft.table }}
                </th>
                <th scope="col">
                  <span class="visually-hidden">references</span>
                </th>
                <th scope="col">
                  {{ current.refTable || 'referenced table' }}
                </th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(c, n) in current.columns"
                :key="c"
              >
                <td class="mono">
                  {{ c }}
                </td>
                <td class="subtle">
                  <Icon name="chevronRight" />
                </td>
                <td>
                  <input
                    v-model="current.refColumns[n]"
                    class="input compact mono"
                    :aria-label="`Referenced column for ${c}`"
                    :disabled="!editable"
                    spellcheck="false"
                  >
                </td>
              </tr>
            </tbody>
          </table>
        </div>

        <div
          v-if="current.type === 'check'"
          class="field"
        >
          <label
            class="label"
            :for="`${uid}-expr`"
          >Expression</label>
          <input
            :id="`${uid}-expr`"
            v-model="current.expression"
            class="input mono"
            :disabled="!editable"
            spellcheck="false"
          >
        </div>

        <div
          v-if="current.type === 'exclusion'"
          class="field"
        >
          <span class="label">Elements (column or expression WITH operator)</span>
          <div
            v-for="(e, n) in current.exclusion"
            :key="n"
            class="row"
          >
            <input
              v-model="e.text"
              class="input compact mono"
              :aria-label="`Element ${n + 1}`"
              :placeholder="e.expression ? 'tstzrange(starts, ends)' : 'column'"
              :disabled="!editable"
              spellcheck="false"
            >
            <label class="inline"><input
              v-model="e.expression"
              type="checkbox"
              :disabled="!editable"
            >expression</label>
            <input
              v-model="e.operator"
              class="input compact mono op"
              :aria-label="`Operator of element ${n + 1}`"
              placeholder="&&"
              :disabled="!editable"
              spellcheck="false"
            >
            <button
              type="button"
              class="icon-btn"
              :aria-label="`Remove element ${n + 1}`"
              :disabled="!editable"
              @click="current.exclusion.splice(n, 1)"
            >
              <Icon name="close" />
            </button>
          </div>
          <div class="row">
            <button
              type="button"
              class="btn compact ghost"
              :disabled="!editable"
              @click="current.exclusion.push({ text: '', expression: false, operator: '=' })"
            >
              + Element
            </button>
            <label
              class="visually-hidden"
              :for="`${uid}-xm`"
            >Index method</label>
            <select
              :id="`${uid}-xm`"
              v-model="current.exclusionMethod"
              class="select compact type"
              :disabled="!editable"
            >
              <option value="gist">
                gist
              </option>
              <option value="spgist">
                spgist
              </option>
              <option value="btree">
                btree
              </option>
              <option value="hash">
                hash
              </option>
            </select>
          </div>
        </div>

        <div
          v-if="current.type === 'foreignKey'"
          class="grid3"
        >
          <div class="field">
            <label
              class="label"
              :for="`${uid}-del`"
            >ON DELETE</label>
            <select
              :id="`${uid}-del`"
              v-model="current.onDelete"
              class="select"
              :disabled="!editable"
            >
              <option
                v-for="a in ACTIONS"
                :key="a.value"
                :value="a.value"
              >
                {{ a.label }}
              </option>
            </select>
          </div>
          <div class="field">
            <label
              class="label"
              :for="`${uid}-upd`"
            >ON UPDATE</label>
            <select
              :id="`${uid}-upd`"
              v-model="current.onUpdate"
              class="select"
              :disabled="!editable"
            >
              <option
                v-for="a in ACTIONS"
                :key="a.value"
                :value="a.value"
              >
                {{ a.label }}
              </option>
            </select>
          </div>
          <div
            v-if="pg"
            class="field"
          >
            <label
              class="label"
              :for="`${uid}-match`"
            >MATCH</label>
            <select
              :id="`${uid}-match`"
              v-model="current.matchFull"
              class="select"
              :disabled="!editable"
            >
              <option :value="false">
                SIMPLE
              </option>
              <option :value="true">
                FULL
              </option>
            </select>
          </div>
        </div>

        <div
          v-if="pg && current.type !== 'check'"
          class="row checks"
        >
          <input
            :id="`${uid}-defer`"
            v-model="current.deferrable"
            type="checkbox"
            :disabled="!editable"
          >
          <label :for="`${uid}-defer`">DEFERRABLE</label>
          <input
            :id="`${uid}-init`"
            v-model="current.initiallyDeferred"
            type="checkbox"
            :disabled="!editable || !current.deferrable"
          >
          <label :for="`${uid}-init`">INITIALLY DEFERRED</label>
        </div>
        <div
          v-if="pg && (current.type === 'check' || current.type === 'foreignKey')"
          class="row checks"
        >
          <input
            :id="`${uid}-nv`"
            v-model="current.notValid"
            type="checkbox"
            :disabled="!editable"
          >
          <label :for="`${uid}-nv`">NOT VALID <span class="subtle">Skips checking existing rows now; validate later with a short lock.</span></label>
        </div>
        <div
          v-if="!pg && current.type === 'check'"
          class="row checks"
        >
          <input
            :id="`${uid}-enf`"
            v-model="current.enforced"
            type="checkbox"
            :disabled="ctx.readOnly || current.dropped"
          >
          <label :for="`${uid}-enf`">Enforced</label>
        </div>
        <div
          v-if="pg && current.type !== 'notNull'"
          class="field"
        >
          <label
            class="label"
            :for="`${uid}-comment`"
          >Comment</label>
          <input
            :id="`${uid}-comment`"
            v-model="current.comment"
            class="input"
            :disabled="ctx.readOnly || current.dropped"
            :placeholder="current.name.trim() ? 'none' : 'name the constraint to give it a comment'"
          >
        </div>
      </section>
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

.toolbar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 44px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border-inner);
}

.note {
  font-size: 12px;
}

.type {
  width: auto;
}

.body {
  flex: 1;
  min-height: 0;
  overflow: auto;
}

.list,
.mapping {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.list th,
.mapping th {
  height: 28px;
  padding: 0 8px;
  border-bottom: 1px solid var(--border);
  background: var(--surface-raised);
  color: var(--text-muted);
  font-weight: 500;
  text-align: left;
}

.list td {
  height: 28px;
  padding: 0 8px;
  border-bottom: 1px solid var(--border-inner);
  cursor: pointer;
}

.mapping td {
  padding: 3px 8px;
}

.def {
  max-width: 520px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.list tr.selected td {
  background: var(--accent-tint);
  font-weight: 500;
}

.list tr.selected td:first-child {
  box-shadow: inset 2px 0 0 var(--accent);
}

.list tr.new td {
  background: var(--success-tint);
}

.list tr.modified td {
  background: var(--warning-tint);
}

.list tr.dropped td {
  background: var(--danger-tint);
  color: var(--danger-text);
  text-decoration: line-through;
}

.type.t-primaryKey {
  color: var(--warning-text);
}

.type.t-foreignKey {
  color: var(--info-text);
}

.type {
  font-size: 11px;
  font-weight: 600;
}

.tag {
  margin-left: 6px;
  padding: 0 5px;
  border-radius: 3px;
  font-family: var(--font-sans);
  font-size: 10px;
  font-weight: 600;
}

.editor {
  display: flex;
  flex-direction: column;
  gap: 14px;
  margin: 12px;
  padding: 16px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
}

.editor-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.strong {
  font-weight: 600;
}

.spacer {
  flex: 1;
}

.grid2 {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 12px;
}

.grid3 {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 12px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.row {
  display: flex;
  align-items: center;
  gap: 6px;
}

.checks {
  font-size: 13px;
}

.compact {
  height: 26px;
  font-size: 12px;
}

.op {
  width: 80px;
}

.inline {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 12px;
  white-space: nowrap;
}

.chips {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.chip {
  height: 24px;
  padding: 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
  color: var(--text);
  font-size: 12px;
  cursor: pointer;
}

.chip[aria-pressed='true'] {
  border-color: var(--accent);
  background: var(--accent-tint);
  color: var(--accent-hover);
}

.notice {
  margin: 0;
}
</style>
