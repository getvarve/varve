<script setup lang="ts">
import { computed, ref, useId } from 'vue';
import { PG_CORE_INDEX_METHODS, isIndexModified, newIndex, pgIdentText, pgNameText, pgReindex } from '../../lib/gridDesigner';
import type { IndexDraft } from '../../lib/gridDesigner';
import { formatBytes } from '../../lib/values';
import Icon from '../common/Icon.vue';
import { useDesigner } from './designerContext';

/** Designer section: indexes, with an editor for a new or redefined index (canvas TableDesigner-Indexes). */
const ctx = useDesigner();
const pg = computed(() => ctx.draft.engine === 'postgres');
/** The index a tree node asked for, selected when the section opens. */
const selected = ref<string | null>(
  ctx.focus?.section === 'indexes' && ctx.focus.name !== null ? (ctx.draft.indexes.find((i) => i.origName === ctx.focus?.name)?.id ?? null) : null,
);
const uid = useId();
const includeInput = ref('');

const MY_KINDS = ['index', 'unique', 'fulltext', 'spatial'];
/** The core methods, plus the index's own method when it comes from an extension (hnsw, ivfflat, bloom, ...). */
const methodOptions = computed(() => {
  if (!pg.value) return MY_KINDS;
  const own = current.value?.method;
  const core: readonly string[] = PG_CORE_INDEX_METHODS;
  return own && !core.includes(own) ? [...core, own] : [...core];
});
const current = computed(() => ctx.draft.indexes.find((i) => i.id === selected.value) ?? null);
const editable = computed(() => !!current.value && !ctx.readOnly && !current.value.dropped && (current.value.origName === null || current.value.changed));
const columnNames = computed(() => ctx.draft.columns.filter((c) => !c.dropped).map((c) => c.name));

function state(i: IndexDraft): 'new' | 'dropped' | 'modified' | '' {
  if (i.origName === null) return 'new';
  if (i.dropped) return 'dropped';
  return isIndexModified(ctx.original, i) ? 'modified' : '';
}

function describeParts(i: IndexDraft): string {
  return `(${i.parts.map((p) => `${p.expression ? `(${p.text})` : p.text}${p.extra && pg.value ? ` ${p.extra}` : ''}${p.extra && !pg.value ? `(${p.extra})` : ''}${p.descending ? ' DESC' : ''}`).join(', ')})`;
}

function add(): void {
  let n = 1;
  while (ctx.draft.indexes.some((i) => i.name === `${ctx.draft.table}_idx${n}`)) n += 1;
  const i = newIndex(ctx.draft.engine, `${ctx.draft.table}_idx${n}`);
  // PostgreSQL builds an index on a partitioned table on every partition; never CONCURRENTLY.
  if (ctx.draft.partitioned) i.concurrently = false;
  ctx.draft.indexes.push(i);
  selected.value = i.id;
}

function toggleDrop(): void {
  const i = current.value;
  if (!i) return;
  if (i.origName === null) {
    ctx.draft.indexes.splice(ctx.draft.indexes.indexOf(i), 1);
    selected.value = null;
  } else {
    i.dropped = !i.dropped;
  }
}

function addPart(expression: boolean): void {
  current.value?.parts.push({ text: expression ? '' : (columnNames.value[0] ?? ''), expression, descending: false, nulls: '', extra: '' });
}

function addInclude(): void {
  const name = includeInput.value.trim();
  if (current.value && name && !current.value.include.includes(name)) current.value.include.push(name);
  includeInput.value = '';
}

function reindex(): void {
  const i = current.value;
  if (i?.origName) ctx.runNow(`Reindex ${i.origName}`, [pgReindex(ctx.draft.schema, i.origName, ctx.serverMajor >= 12)]);
}

function fixedReason(i: IndexDraft): string | null {
  if (i.locked) return i.locked;
  if (i.primary) return 'The primary key index changes with the primary key under Constraints.';
  if (i.constraint && pg.value) return `This index enforces the constraint ${i.constraint}; change it under Constraints.`;
  return null;
}

const preview = computed(() => {
  const i = current.value;
  if (!i || !pg.value) return '';
  const parts = i.parts.map((p) => {
    const key = p.expression ? `(${p.text})` : pgIdentText(p.text);
    const keepCollation = p.read?.collation && p.read.text === (p.expression ? p.text.trim() : p.text) && p.read.expression === p.expression;
    const collation = keepCollation && p.read?.collation ? ` COLLATE ${pgNameText(p.read.collation)}` : '';
    const options = p.read && p.extra.trim() === p.read.opclassText && p.read.opclassOptions.length ? ` (${p.read.opclassOptions.map((o) => `${o.name} = ${o.value}`).join(', ')})` : '';
    return `${key}${collation}${p.extra ? ` ${p.extra}` : ''}${options}${p.descending ? ' DESC' : ''}${p.nulls ? ` NULLS ${p.nulls.toUpperCase()}` : ''}`;
  });
  const params = [...(i.fillfactor ? [`fillfactor = ${i.fillfactor}`] : []), ...i.storageParams.map((p) => `${p.name} = ${p.value}`)];
  return `CREATE ${i.unique ? 'UNIQUE ' : ''}INDEX ${i.concurrently ? 'CONCURRENTLY ' : ''}${pgIdentText(i.name)} ON ${pgIdentText(ctx.draft.schema)}.${pgIdentText(ctx.draft.table)} USING ${i.method} (${parts.join(', ')})${i.include.length ? ` INCLUDE (${i.include.map(pgIdentText).join(', ')})` : ''}${i.unique && i.nullsNotDistinct ? ' NULLS NOT DISTINCT' : ''}${params.length ? ` WITH (${params.join(', ')})` : ''}${i.tablespace ? ` TABLESPACE ${pgIdentText(i.tablespace)}` : ''}${i.predicate.trim() ? ` WHERE ${i.predicate.trim()}` : ''}`;
});
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
        <Icon name="plus" />Add index
      </button>
      <button
        v-if="pg"
        type="button"
        class="btn compact"
        :disabled="ctx.readOnly || !current || current.origName === null || current.dropped"
        @click="reindex"
      >
        {{ ctx.serverMajor >= 12 ? 'Reindex concurrently' : 'Reindex' }}
      </button>
      <button
        type="button"
        class="btn compact"
        :disabled="ctx.readOnly || !current || fixedReason(current) !== null"
        @click="toggleDrop"
      >
        <Icon name="close" />{{ current?.dropped ? 'Keep' : 'Drop' }}
      </button>
    </div>

    <div class="body">
      <div class="scroll">
        <table
          class="list"
          :aria-label="`Indexes on ${ctx.draft.schema}.${ctx.draft.table}`"
        >
          <thead>
            <tr>
              <th scope="col">
                Name
              </th>
              <th scope="col">
                {{ pg ? 'Method' : 'Kind' }}
              </th>
              <th scope="col">
                Columns / expressions
              </th>
              <th
                v-if="pg"
                scope="col"
              >
                Unique
              </th>
              <th
                v-if="pg"
                scope="col"
              >
                Partial WHERE
              </th>
              <th
                v-if="pg"
                scope="col"
              >
                INCLUDE
              </th>
              <th
                v-else
                scope="col"
              >
                Visible
              </th>
              <th scope="col">
                Size
              </th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="i in ctx.draft.indexes"
              :key="i.id"
              :class="[state(i), { selected: selected === i.id }]"
              :aria-selected="selected === i.id"
              tabindex="0"
              @click="selected = i.id"
              @keydown.enter="selected = i.id"
            >
              <td class="mono">
                {{ i.name }}
                <span
                  v-if="i.primary"
                  class="tag tone-warning"
                >PK</span>
                <span
                  v-else-if="i.constraint"
                  class="tag tone-info"
                >CONSTRAINT</span>
                <span
                  v-if="state(i) === 'new'"
                  class="tag tone-success"
                >NEW</span>
                <span
                  v-if="i.concurrently && state(i) === 'new' && pg"
                  class="tag tone-info"
                >CONCURRENTLY</span>
              </td>
              <td class="mono">
                {{ i.method }}
              </td>
              <td class="mono">
                {{ describeParts(i) }}
              </td>
              <td v-if="pg">
                {{ i.unique ? 'Yes' : 'No' }}
              </td>
              <td
                v-if="pg"
                class="mono"
                :class="{ subtle: !i.predicate }"
              >
                {{ i.predicate || 'none' }}
              </td>
              <td
                v-if="pg"
                class="mono"
                :class="{ subtle: i.include.length === 0 }"
              >
                {{ i.include.length ? `(${i.include.join(', ')})` : 'none' }}
              </td>
              <td v-else>
                {{ i.invisible ? 'No' : 'Yes' }}
              </td>
              <td
                class="mono"
                :class="{ subtle: i.sizeBytes === null }"
              >
                {{ i.sizeBytes === null ? (i.origName === null ? 'not built' : '') : formatBytes(i.sizeBytes) }}
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <section
        v-if="current"
        class="editor"
        :aria-label="`Index ${current.name}`"
      >
        <header class="editor-head">
          <span class="strong">{{ current.origName === null ? 'New index' : 'Index' }}</span>
          <span class="mono muted">{{ current.name }}</span>
          <span class="spacer" />
          <button
            v-if="current.origName !== null && !current.changed && !current.dropped && fixedReason(current) === null"
            type="button"
            class="btn compact"
            :disabled="ctx.readOnly"
            @click="current.changed = true"
          >
            Redefine
          </button>
        </header>
        <p
          v-if="fixedReason(current)"
          class="notice tone-neutral"
        >
          {{ fixedReason(current) }}
        </p>
        <p
          v-if="current.changed"
          class="notice tone-warning"
        >
          The index is dropped and built again with this definition.<template v-if="current.replicaIdentity">
            It is the table's replica identity: the new index is built under a temporary name, becomes the
            replica identity, and then replaces the old one, so the table never lacks one.
          </template><template v-if="current.clustered">
            CLUSTER ON is set again.
          </template>
        </p>
        <div class="grid3">
          <div class="field">
            <label
              class="label"
              :for="`${uid}-name`"
            >Name</label>
            <input
              :id="`${uid}-name`"
              v-model="current.name"
              class="input mono"
              :disabled="ctx.readOnly || current.dropped || fixedReason(current) !== null"
              spellcheck="false"
            >
          </div>
          <div class="field">
            <label
              class="label"
              :for="`${uid}-method`"
            >{{ pg ? 'Method' : 'Kind' }}</label>
            <select
              :id="`${uid}-method`"
              v-model="current.method"
              class="select"
              :disabled="!editable"
            >
              <option
                v-for="m in methodOptions"
                :key="m"
                :value="m"
              >
                {{ m }}
              </option>
            </select>
          </div>
          <div
            v-if="!pg"
            class="field check"
          >
            <input
              :id="`${uid}-invisible`"
              v-model="current.invisible"
              type="checkbox"
              :disabled="ctx.readOnly || current.dropped || current.primary"
            >
            <label :for="`${uid}-invisible`">Invisible to the optimizer</label>
          </div>
        </div>

        <div class="field">
          <span class="label">Key columns and expressions</span>
          <table class="parts">
            <thead>
              <tr>
                <th scope="col">
                  #
                </th>
                <th scope="col">
                  Column or expression
                </th>
                <th scope="col">
                  Order
                </th>
                <th
                  v-if="pg"
                  scope="col"
                >
                  Nulls
                </th>
                <th scope="col">
                  {{ pg ? 'Opclass' : 'Prefix length' }}
                </th>
                <th scope="col">
                  <span class="visually-hidden">Remove</span>
                </th>
              </tr>
            </thead>
            <tbody>
              <tr
                v-for="(p, n) in current.parts"
                :key="n"
              >
                <td class="mono subtle">
                  {{ n + 1 }}
                </td>
                <td>
                  <select
                    v-if="!p.expression"
                    v-model="p.text"
                    class="select compact mono"
                    :aria-label="`Column ${n + 1}`"
                    :disabled="!editable"
                  >
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
                    v-model="p.text"
                    class="input compact mono"
                    :aria-label="`Expression ${n + 1}`"
                    placeholder="lower(email)"
                    :disabled="!editable"
                    spellcheck="false"
                  >
                </td>
                <td>
                  <select
                    v-model="p.descending"
                    class="select compact"
                    :aria-label="`Order of part ${n + 1}`"
                    :disabled="!editable"
                  >
                    <option :value="false">
                      ASC
                    </option>
                    <option :value="true">
                      DESC
                    </option>
                  </select>
                </td>
                <td v-if="pg">
                  <select
                    v-model="p.nulls"
                    class="select compact"
                    :aria-label="`Nulls of part ${n + 1}`"
                    :disabled="!editable"
                  >
                    <option value="">
                      default
                    </option>
                    <option value="first">
                      FIRST
                    </option>
                    <option value="last">
                      LAST
                    </option>
                  </select>
                </td>
                <td>
                  <input
                    v-model="p.extra"
                    class="input compact mono"
                    :aria-label="pg ? `Operator class of part ${n + 1}` : `Prefix length of part ${n + 1}`"
                    placeholder="default"
                    :disabled="!editable"
                    spellcheck="false"
                  >
                </td>
                <td>
                  <button
                    type="button"
                    class="icon-btn"
                    :aria-label="`Remove part ${n + 1}`"
                    :disabled="!editable"
                    @click="current.parts.splice(n, 1)"
                  >
                    <Icon name="close" />
                  </button>
                </td>
              </tr>
            </tbody>
          </table>
          <div class="row">
            <button
              type="button"
              class="btn compact ghost"
              :disabled="!editable"
              @click="addPart(false)"
            >
              + Column
            </button>
            <button
              type="button"
              class="btn compact ghost"
              :disabled="!editable"
              @click="addPart(true)"
            >
              + Expression
            </button>
          </div>
        </div>

        <template v-if="pg">
          <div class="field">
            <span class="label">INCLUDE (covering, not part of the key)</span>
            <div class="chips">
              <span
                v-for="(name, n) in current.include"
                :key="name"
                class="chip mono"
              >{{ name }}<button
                type="button"
                class="icon-btn small"
                :aria-label="`Remove ${name} from INCLUDE`"
                :disabled="!editable"
                @click="current.include.splice(n, 1)"
              ><Icon
                name="close"
                :size="12"
              /></button></span>
              <label
                class="visually-hidden"
                :for="`${uid}-incl`"
              >Add INCLUDE column</label>
              <input
                :id="`${uid}-incl`"
                v-model="includeInput"
                class="input compact mono incl"
                placeholder="Add column"
                :disabled="!editable"
                @keydown.enter.prevent="addInclude"
                @blur="addInclude"
              >
            </div>
          </div>
          <div class="grid3">
            <div class="field check">
              <input
                :id="`${uid}-conc`"
                v-model="current.concurrently"
                type="checkbox"
                :disabled="ctx.readOnly || ctx.draft.partitioned || current.origName !== null && !current.changed && !current.dropped"
              >
              <label :for="`${uid}-conc`">CONCURRENTLY <span class="subtle">{{ ctx.draft.partitioned ? '(not on a partitioned table: PostgreSQL builds it on every partition and locks writes meanwhile)' : '(no write lock; runs outside a transaction)' }}</span></label>
            </div>
            <div class="field check">
              <input
                :id="`${uid}-unique`"
                v-model="current.unique"
                type="checkbox"
                :disabled="!editable"
              >
              <label :for="`${uid}-unique`">Unique</label>
              <input
                :id="`${uid}-nnd`"
                v-model="current.nullsNotDistinct"
                type="checkbox"
                :disabled="!editable || !current.unique || ctx.serverMajor < 15"
              >
              <label :for="`${uid}-nnd`">NULLS NOT DISTINCT</label>
            </div>
          </div>
          <div class="grid3">
            <div class="field wide">
              <label
                class="label"
                :for="`${uid}-where`"
              >Partial index WHERE</label>
              <input
                :id="`${uid}-where`"
                v-model="current.predicate"
                class="input mono"
                :disabled="!editable"
                spellcheck="false"
              >
            </div>
            <div class="field">
              <label
                class="label"
                :for="`${uid}-ff`"
              >Fillfactor</label>
              <input
                :id="`${uid}-ff`"
                v-model="current.fillfactor"
                class="input mono"
                inputmode="numeric"
                placeholder="default"
                :disabled="!editable"
              >
            </div>
          </div>
          <div
            v-if="editable"
            class="field"
          >
            <span class="label">Statement (preview; the server writes the final SQL)</span>
            <pre class="sql mono">{{ preview }};</pre>
          </div>
        </template>
        <div
          v-else
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
            :disabled="!editable"
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

.body {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-height: 0;
  overflow: auto;
}

.scroll {
  flex-shrink: 0;
}

.list,
.parts {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.list th,
.parts th {
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

.parts td {
  padding: 3px 6px;
  border-bottom: 1px solid var(--border-inner);
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
  background: var(--surface);
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

.field.wide {
  grid-column: span 2;
}

.field.check {
  flex-direction: row;
  align-items: center;
  gap: 6px;
  font-size: 13px;
}

.compact {
  height: 26px;
  font-size: 12px;
}

.row {
  display: flex;
  gap: 6px;
}

.chips {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
}

.chip {
  display: inline-flex;
  align-items: center;
  gap: 2px;
  height: 24px;
  padding: 0 2px 0 8px;
  border-radius: var(--radius-control);
  background: var(--surface-raised);
  font-size: 12px;
}

.icon-btn.small {
  width: 20px;
  height: 20px;
}

.incl {
  width: 160px;
}

.sql {
  margin: 0;
  padding: 10px 12px;
  border-radius: var(--radius-control);
  background: var(--ground);
  font-size: 12px;
  white-space: pre-wrap;
}

.notice {
  margin: 0;
}
</style>
