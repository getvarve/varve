<script setup lang="ts">
import { computed, onMounted, ref, useId, watch } from 'vue';
import { catalogEngines } from '../../api/grid';
import type { MyEngine } from '../../api/grid';
import { presentError, toCmdError } from '../../api/errors';
import { changedProps, collationOf, engineRowLoss, rowFormatFor, rowFormatsOf } from '../../lib/gridDesigner';
import type { TablePropsDraft } from '../../lib/gridDesigner';
import Icon from '../common/Icon.vue';
import { useDesigner } from './designerContext';

/**
 * Designer section: table properties. PostgreSQL: name, schema, owner,
 * comment, UNLOGGED, INHERITS, storage parameters and tablespace. MySQL and
 * MariaDB: name, storage engine (from the server's own list), ROW_FORMAT,
 * AUTO_INCREMENT, character set and collation, comment. Saved with the
 * rest of the draft, through the same plan review and confirmation.
 */
const ctx = useDesigner();
const uid = useId();
const pg = computed(() => ctx.draft.engine === 'postgres');
const p = computed<TablePropsDraft>(() => ctx.draft.props);
const isNew = computed(() => ctx.draft.isNew);
const changed = computed(() => new Set(isNew.value ? [] : changedProps(ctx.original, ctx.draft)));
const disabled = computed(() => ctx.readOnly);

/** PostgreSQL: a partitioned table has no storage of its own (no UNLOGGED, storage parameters or INHERITS). */
const partitioned = computed(() => ctx.draft.kind === 'partitionedTable' || (isNew.value && ctx.draft.partitionBy.enabled));
const partition = computed(() => ctx.draft.kind === 'partition');

/** Storage parameters PostgreSQL knows for tables, offered while typing. */
const PG_PARAMS = [
  'fillfactor',
  'toast_tuple_target',
  'parallel_workers',
  'autovacuum_enabled',
  'autovacuum_vacuum_threshold',
  'autovacuum_vacuum_scale_factor',
  'autovacuum_vacuum_insert_threshold',
  'autovacuum_vacuum_insert_scale_factor',
  'autovacuum_analyze_threshold',
  'autovacuum_analyze_scale_factor',
  'autovacuum_vacuum_cost_delay',
  'autovacuum_vacuum_cost_limit',
  'autovacuum_freeze_min_age',
  'autovacuum_freeze_max_age',
  'autovacuum_freeze_table_age',
  'log_autovacuum_min_duration',
  'user_catalog_table',
  'vacuum_index_cleanup',
  'vacuum_truncate',
  'toast.autovacuum_enabled',
  'toast.vacuum_truncate',
];

function addParam(): void {
  p.value.storageParams.push({ name: '', value: '' });
}

function addParent(): void {
  p.value.inherits.push('');
}

// --- MySQL / MariaDB engines ---

const engines = ref<MyEngine[] | null>(null);
const enginesError = ref<string | null>(null);

onMounted(async () => {
  if (pg.value) return;
  try {
    engines.value = await catalogEngines(ctx.sessionId);
  } catch (error: unknown) {
    enginesError.value = presentError(toCmdError(error)).message;
  }
});

/** The server's engines, plus the table's own when the server no longer lists it as available. */
const engineOptions = computed<MyEngine[]>(() => {
  const list = engines.value ?? [];
  const own = ctx.original.props.engine;
  if (own && !list.some((e) => e.name.toLowerCase() === own.toLowerCase())) {
    return [...list, { name: own, default: false, transactions: null, comment: 'Not listed as available on this server.', warning: null }];
  }
  return list;
});
const selectedEngine = computed(() => engineOptions.value.find((e) => e.name.toLowerCase() === p.value.engine.toLowerCase()) ?? null);
const defaultEngine = computed(() => engines.value?.find((e) => e.default)?.name ?? null);

const ROW_FORMATS: { value: string; label: string }[] = [
  { value: 'default', label: 'DEFAULT' },
  { value: 'dynamic', label: 'DYNAMIC' },
  { value: 'compact', label: 'COMPACT' },
  { value: 'redundant', label: 'REDUNDANT' },
  { value: 'compressed', label: 'COMPRESSED' },
  { value: 'fixed', label: 'FIXED' },
  { value: 'page', label: 'PAGE (Aria)' },
];
/** The row formats the chosen engine accepts (all of them for an engine the designer does not know), plus the one set now. */
const rowFormats = computed(() => {
  const valid = rowFormatsOf(p.value.engine || defaultEngine.value || '');
  return ROW_FORMATS.filter((f) => !valid || valid.includes(f.value) || f.value === p.value.rowFormat);
});
/** The engine changes and the table's row format does not work with it: the save sets ROW_FORMAT=DEFAULT with it. */
const rowFormatReset = computed(
  () => !isNew.value && changed.value.has('engine') && !changed.value.has('rowFormat') && rowFormatFor(p.value.engine, p.value.rowFormat) !== p.value.rowFormat,
);
/** How the new engine loses the table's rows, if it does. */
const rowLoss = computed(() => (changed.value.has('engine') ? engineRowLoss(p.value.engine) : null));

// A collation read with the table belongs to its old character set: a new character set starts on its own default.
watch(
  () => p.value.charset,
  (charset) => {
    const was = ctx.original.props;
    if (pg.value || charset.trim().toLowerCase() === was.charset.trim().toLowerCase()) return;
    if (p.value.collation.trim() === was.collation.trim() && !collationOf(charset, p.value.collation)) p.value.collation = '';
  },
);
const collationProblem = computed(() => {
  const cs = (p.value.charset || ctx.original.props.charset).trim();
  const coll = p.value.collation.trim();
  return cs && coll && !collationOf(cs, coll) ? `${coll} is not a collation of ${cs}.` : null;
});
/** The typed AUTO_INCREMENT is below the next value the server reported: it keeps the larger one. */
const autoIncrementLow = computed(() => {
  const typed = p.value.autoIncrement.trim();
  const read = ctx.original.props.autoIncrement.trim();
  return !isNew.value && changed.value.has('autoIncrement') && /^\d+$/.test(typed) && /^\d+$/.test(read) && BigInt(typed) < BigInt(read);
});

const charsetChanged = computed(() => changed.value.has('charset') || changed.value.has('collation'));

function mark(key: keyof TablePropsDraft): string {
  return changed.value.has(key) ? 'changed' : '';
}
</script>

<template>
  <div class="section">
    <div class="body">
      <form
        class="form"
        :aria-label="`Properties of ${ctx.draft.schema}.${ctx.draft.table}`"
        @submit.prevent
      >
        <p
          v-if="!isNew && (changed.has('name') || changed.has('schema'))"
          class="notice tone-info"
        >
          {{ pg ? 'Renaming and moving to another schema run' : 'Renaming runs' }} last, after every other change of this save.
        </p>

        <!-- PostgreSQL -->
        <template v-if="pg">
          <div class="grid2">
            <div
              v-if="!isNew"
              class="field"
              :class="mark('name')"
            >
              <label
                class="label"
                :for="`${uid}-name`"
              >Name</label>
              <input
                :id="`${uid}-name`"
                v-model="p.name"
                class="input mono"
                :disabled="disabled"
                spellcheck="false"
              >
            </div>
            <div
              class="field"
              :class="mark('schema')"
            >
              <label
                class="label"
                :for="`${uid}-schema`"
              >Schema</label>
              <input
                v-if="isNew"
                :id="`${uid}-schema`"
                v-model="ctx.draft.schema"
                class="input mono"
                :disabled="disabled"
                spellcheck="false"
              >
              <input
                v-else
                :id="`${uid}-schema`"
                v-model="p.schema"
                class="input mono"
                :disabled="disabled"
                spellcheck="false"
              >
            </div>
            <div
              class="field"
              :class="mark('owner')"
            >
              <label
                class="label"
                :for="`${uid}-owner`"
              >Owner</label>
              <input
                :id="`${uid}-owner`"
                v-model="p.owner"
                class="input mono"
                :placeholder="isNew ? 'the connected role' : undefined"
                :disabled="disabled"
                spellcheck="false"
              >
            </div>
            <div
              class="field"
              :class="mark('tablespace')"
            >
              <label
                class="label"
                :for="`${uid}-ts`"
              >Tablespace</label>
              <input
                :id="`${uid}-ts`"
                v-model="p.tablespace"
                class="input mono"
                placeholder="database default"
                :disabled="disabled"
                spellcheck="false"
              >
              <span
                v-if="changed.has('tablespace')"
                class="hint warning"
              >
                {{ partitioned ? 'Sets where new partitions are created; existing partitions stay.' : 'SET TABLESPACE copies the whole table and its TOAST data under an ACCESS EXCLUSIVE lock.' }}
                {{ p.tablespace.trim() === '' ? `Empty moves it back to the database default (${p.databaseTablespace || 'pg_default'}).` : '' }}
              </span>
            </div>
          </div>

          <div
            class="field"
            :class="mark('unlogged')"
          >
            <label class="inline check">
              <input
                v-model="p.unlogged"
                type="checkbox"
                :disabled="disabled || partitioned"
              >
              UNLOGGED
            </label>
            <span class="hint">
              {{
                partitioned
                  ? 'A partitioned table holds no rows of its own and cannot be unlogged; set it on its partitions.'
                  : 'An unlogged table skips the write-ahead log: faster writes, but it is emptied after a crash and not replicated.'
              }}
            </span>
            <p
              v-if="changed.has('unlogged')"
              class="notice tone-warning"
              role="note"
            >
              {{ p.unlogged ? 'SET UNLOGGED' : 'SET LOGGED' }} rewrites the whole table under an ACCESS EXCLUSIVE lock{{ p.unlogged ? '' : ' and writes all of it to the write-ahead log' }}.
            </p>
          </div>

          <fieldset
            v-if="!partition && !partitioned"
            class="field group"
            :class="mark('inherits')"
          >
            <legend class="label">
              Inherits from
            </legend>
            <div
              v-for="(_, n) in p.inherits"
              :key="n"
              class="row"
            >
              <input
                v-model="p.inherits[n]"
                class="input mono"
                :aria-label="`Parent table ${n + 1}`"
                placeholder="schema.table"
                :disabled="disabled"
                spellcheck="false"
              >
              <button
                type="button"
                class="icon-btn"
                :aria-label="`Remove parent ${p.inherits[n] || n + 1}`"
                :disabled="disabled"
                @click="p.inherits.splice(n, 1)"
              >
                <Icon name="close" />
              </button>
            </div>
            <div class="row">
              <button
                type="button"
                class="btn compact"
                :disabled="disabled"
                @click="addParent"
              >
                Add parent
              </button>
              <span class="hint">{{ isNew ? 'The table also gets every column of its parents.' : 'INHERIT needs every column of the parent in this table, with the same types.' }}</span>
            </div>
          </fieldset>

          <fieldset
            class="field group"
            :class="mark('storageParams')"
          >
            <legend class="label">
              Storage parameters
            </legend>
            <p
              v-if="partitioned"
              class="hint"
            >
              A partitioned table takes no storage parameters; set them on its partitions.
            </p>
            <template v-else>
              <datalist :id="`${uid}-params`">
                <option
                  v-for="name in PG_PARAMS"
                  :key="name"
                  :value="name"
                />
              </datalist>
              <div
                v-for="(param, n) in p.storageParams"
                :key="n"
                class="row"
              >
                <input
                  v-model="param.name"
                  class="input mono"
                  :list="`${uid}-params`"
                  :aria-label="`Parameter ${n + 1} name`"
                  placeholder="fillfactor"
                  :disabled="disabled"
                  spellcheck="false"
                >
                <span class="muted">=</span>
                <input
                  v-model="param.value"
                  class="input mono value"
                  :aria-label="`Parameter ${param.name || n + 1} value`"
                  placeholder="90"
                  :disabled="disabled"
                  spellcheck="false"
                >
                <button
                  type="button"
                  class="icon-btn"
                  :aria-label="`Remove parameter ${param.name || n + 1}`"
                  :disabled="disabled"
                  @click="p.storageParams.splice(n, 1)"
                >
                  <Icon name="close" />
                </button>
              </div>
              <div class="row">
                <button
                  type="button"
                  class="btn compact"
                  :disabled="disabled"
                  @click="addParam"
                >
                  Add parameter
                </button>
                <span class="hint">Removed parameters are RESET to the server default. Changing one takes a SHARE UPDATE EXCLUSIVE lock; fillfactor applies to new pages only.</span>
              </div>
            </template>
          </fieldset>
        </template>

        <!-- MySQL / MariaDB -->
        <template v-else>
          <div class="grid2">
            <div
              v-if="!isNew"
              class="field"
              :class="mark('name')"
            >
              <label
                class="label"
                :for="`${uid}-name`"
              >Name</label>
              <input
                :id="`${uid}-name`"
                v-model="p.name"
                class="input mono"
                :disabled="disabled"
                spellcheck="false"
              >
            </div>
            <div
              class="field"
              :class="mark('engine')"
            >
              <label
                class="label"
                :for="`${uid}-engine`"
              >Storage engine</label>
              <select
                v-if="engines !== null"
                :id="`${uid}-engine`"
                v-model="p.engine"
                class="select"
                :disabled="disabled"
              >
                <option
                  v-if="isNew"
                  value=""
                >
                  Server default{{ defaultEngine ? ` (${defaultEngine})` : '' }}
                </option>
                <option
                  v-for="e in engineOptions"
                  :key="e.name"
                  :value="e.name"
                >
                  {{ e.name }}{{ e.default ? ' (default)' : '' }}
                </option>
              </select>
              <input
                v-else
                :id="`${uid}-engine`"
                v-model="p.engine"
                class="input mono"
                :placeholder="enginesError ? 'InnoDB' : 'Reading the engines…'"
                :disabled="disabled"
                spellcheck="false"
              >
              <span
                v-if="enginesError"
                class="hint warning"
                role="status"
              >The server's engines could not be read: {{ enginesError }} Type the engine name.</span>
              <span
                v-else-if="selectedEngine?.comment"
                class="hint"
              >{{ selectedEngine.comment }}{{ selectedEngine.transactions === false ? ' No transactions.' : '' }}</span>
              <p
                v-if="changed.has('engine') && rowLoss"
                class="notice tone-danger"
                role="alert"
              >
                {{
                  rowLoss === 'discarded'
                    ? `${p.engine.toUpperCase()} keeps no rows: every row of this table is lost when it changes. The review shows how many.`
                    : 'MEMORY keeps rows only in memory: every row is lost at the next server restart, and BLOB and TEXT columns make the change fail. The review shows how many rows.'
                }}
              </p>
              <p
                v-else-if="changed.has('engine')"
                class="notice tone-warning"
                role="note"
              >
                {{ selectedEngine?.warning ? `${selectedEngine.warning}.` : 'Changing the engine copies the whole table; features the new engine lacks (foreign keys, transactions, full-text or spatial indexes) are lost or make the change fail.' }}
              </p>
              <p
                v-if="rowFormatReset"
                class="notice tone-info"
                role="note"
              >
                ROW_FORMAT {{ p.rowFormat.toUpperCase() }} does not work with {{ p.engine }}: the change also sets ROW_FORMAT=DEFAULT.
              </p>
            </div>
            <div
              class="field"
              :class="mark('rowFormat')"
            >
              <label
                class="label"
                :for="`${uid}-rf`"
              >Row format</label>
              <select
                :id="`${uid}-rf`"
                v-model="p.rowFormat"
                class="select"
                :disabled="disabled"
              >
                <option
                  v-if="isNew || ctx.original.props.rowFormat === ''"
                  value=""
                >
                  Engine default
                </option>
                <option
                  v-for="f in rowFormats"
                  :key="f.value"
                  :value="f.value"
                >
                  {{ f.label }}
                </option>
              </select>
              <span
                v-if="changed.has('rowFormat')"
                class="hint warning"
              >A new row format rebuilds the table.</span>
            </div>
            <div
              class="field"
              :class="mark('autoIncrement')"
            >
              <label
                class="label"
                :for="`${uid}-ai`"
              >Next AUTO_INCREMENT</label>
              <input
                :id="`${uid}-ai`"
                v-model="p.autoIncrement"
                class="input mono"
                inputmode="numeric"
                :placeholder="isNew ? '1' : undefined"
                :disabled="disabled"
                spellcheck="false"
              >
              <span class="hint">{{ isNew ? 'The first value an AUTO_INCREMENT column gets.' : 'As the server last reported it (MySQL caches it for information_schema_stats_expiry). InnoDB never goes below the largest value in the column plus one.' }}</span>
              <span
                v-if="autoIncrementLow"
                class="hint warning"
                role="note"
              >{{ p.autoIncrement.trim() }} is below the next value the server reported ({{ ctx.original.props.autoIncrement }}): the server keeps a value above the largest one in the column. The review checks it.</span>
            </div>
            <div
              class="field"
              :class="mark('charset')"
            >
              <label
                class="label"
                :for="`${uid}-cs`"
              >Character set</label>
              <input
                :id="`${uid}-cs`"
                v-model="p.charset"
                class="input mono"
                :placeholder="isNew ? 'database default' : undefined"
                :disabled="disabled"
                spellcheck="false"
              >
            </div>
            <div
              class="field"
              :class="mark('collation')"
            >
              <label
                class="label"
                :for="`${uid}-coll`"
              >Collation</label>
              <input
                :id="`${uid}-coll`"
                v-model="p.collation"
                class="input mono"
                placeholder="character set default"
                :disabled="disabled"
                spellcheck="false"
              >
              <span
                v-if="collationProblem"
                class="hint warning"
                role="alert"
              >{{ collationProblem }} Leave it empty for the character set's default.</span>
            </div>
          </div>
          <div
            v-if="!isNew && charsetChanged"
            class="field"
          >
            <label class="inline check">
              <input
                v-model="p.convertCharset"
                type="checkbox"
                :disabled="disabled"
              >
              Convert the existing columns (CONVERT TO CHARACTER SET)
            </label>
            <p
              class="notice"
              :class="p.convertCharset ? 'tone-warning' : 'tone-info'"
              role="note"
            >
              {{
                p.convertCharset
                  ? 'CONVERT TO rewrites every text column into the new character set; characters it lacks are lost. Needs confirmation.'
                  : 'Only the default for columns added later changes; existing columns keep their character set.'
              }}
            </p>
          </div>
        </template>

        <div
          class="field"
          :class="mark('comment')"
        >
          <label
            class="label"
            :for="`${uid}-comment`"
          >Comment</label>
          <textarea
            :id="`${uid}-comment`"
            v-model="p.comment"
            class="input comment"
            rows="3"
            :disabled="disabled"
          />
        </div>
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
  max-width: 880px;
  padding: 16px;
}

.grid2 {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 12px 16px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}

.field.changed .label {
  color: var(--warning-text);
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

.row .input {
  flex: 1;
  min-width: 0;
}

.row .input.value {
  flex: 0 1 200px;
}

.inline {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
}

.hint {
  color: var(--text-muted);
  font-size: 12px;
}

.hint.warning {
  color: var(--warning-text);
}

.comment {
  height: auto;
  min-height: 64px;
  padding: 6px 8px;
  resize: vertical;
}

.compact {
  height: 26px;
  font-size: 12px;
}

.notice {
  margin: 0;
}
</style>
