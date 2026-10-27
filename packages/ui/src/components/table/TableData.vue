<script setup lang="ts">
import { AllCommunityModule, ModuleRegistry, themeQuartz } from 'ag-grid-community';
import type {
  CellDoubleClickedEvent,
  CellFocusedEvent,
  ColDef,
  GridApi,
  GridOptions,
  GridReadyEvent,
  SortChangedEvent,
  ValueSetterParams,
} from 'ag-grid-community';
import { AgGridVue } from 'ag-grid-vue3';
import { computed, markRaw, onMounted, reactive, ref, shallowRef, useId, watch } from 'vue';
import { describeTable, editableKey, rowEditApply, rowEditPlan, runGridQuery, tableRef } from '../../api/grid';
import type { CellValue, ColumnInfo, KeyInfo, RowEditPlan, TableTarget } from '../../api/grid';
import type { ObjectRef } from '../../api/catalog';
import { openTransactionOf, presentError, toCmdError } from '../../api/errors';
import type { SessionInfo, Value } from '../../api/sessions';
import { useDialogs } from '../../composables/useDialogs';
import { useToasts } from '../../composables/useToasts';
import { runWithConfirmation } from '../../lib/destructive';
import {
  addInsert,
  applyFailureAction,
  buildRowChanges,
  changeCounts,
  changeLabels,
  describeProblem,
  discardAll,
  discardRef,
  emptyChanges,
  keyLabel,
  mergeApplied,
  missingRequired,
  refreshSnapshots,
  removeInsert,
  revertCell,
  setCell,
  setInsertCell,
  toGridRows,
  toggleDelete,
} from '../../lib/gridChanges';
import type { ApplyProblem, ChangeRef, ChangeSet, GridRowData } from '../../lib/gridChanges';
import { createRowViewCache, RowVersions } from '../../lib/gridRows';
import type { ViewRow } from '../../lib/gridRows';
import { countSql, dialectOf, FILTER_OPERATORS, operatorNeedsValue, selectPageSql } from '../../lib/gridSql';
import type { FilterCondition, GridFilter, SortKey } from '../../lib/gridSql';
import { cachedEditText, opensInDialog, previewText, tooltipText } from '../../lib/gridPreview';
import { columnAccess, keyRefusal, parseCellText, pendingCellText, tableReadOnlyReason } from '../../lib/gridValues';
import { cellClass, formatCount, formatDuration } from '../../lib/values';
import Icon from '../common/Icon.vue';
import { TypedHeader } from '../query/typedHeader';
import RowEditReview from './RowEditReview.vue';
import ValueEditorDialog from './ValueEditorDialog.vue';

ModuleRegistry.registerModules([AllCommunityModule]);

/**
 * Table data with grid editing (canvas TableData): server-side paging,
 * sorting and filtering, inline and dialog editing, inserts and deletes
 * kept as pending changes, a review of the generated statements, one
 * transaction to apply, and conflict handling when someone else changed a
 * row in the meantime.
 */
const props = defineProps<{
  /** The user session of the tab. */
  session: SessionInfo;
  /** Name of the connection, for the destructive confirmation dialog. */
  connectionName: string;
  /** The table, view or other relation to show. */
  object: ObjectRef;
}>();
const emit = defineEmits<{
  /** The view has pending changes (true) or none (false). */
  dirty: [dirty: boolean];
  /** A backend call failed; the parent reports it (lost sessions, reconnect needed). */
  error: [error: unknown];
  /** Rows were written. */
  changed: [];
}>();

const toasts = useToasts();
const dialogs = useDialogs();
const uid = useId();
const dialect = computed(() => dialectOf(props.session.engine));
const target = computed<TableTarget>(() => ({
  database: props.object.database ?? props.session.database ?? '',
  schema: dialect.value === 'postgres' ? props.object.schema : null,
  table: props.object.name,
}));
const qualified = computed(() => `${target.value.schema ?? target.value.database}.${target.value.table}`);

const TABLE_KINDS = ['table', 'partitionedTable', 'partition', 'foreignTable'];
const isTable = computed(() => TABLE_KINDS.includes(props.object.kind));

// --- state ---

const key = shallowRef<KeyInfo | null>(null);
const keyReason = ref<string | null>(null);
const columns = shallowRef<ColumnInfo[]>([]);
const rows = shallowRef<GridRowData[]>([]);
/**
 * Pending changes. Kept out of Vue's deep reactivity (the snapshots hold
 * whole rows of values); every change goes through `mutate`, which bumps
 * `csVersion` so the computeds that read the change set update.
 */
const cs: ChangeSet = markRaw(emptyChanges());
const csVersion = ref(0);
/** Versions of each row's pending changes, so the grid redraws only rows that changed. */
const rowVersions = new RowVersions();
/**
 * Runs a change to the change set, then bumps the version of the rows it
 * touched (`'all'` for changes to every row) and of the change set.
 */
function mutate<T>(rowIds: readonly string[] | 'all', fn: () => T): T {
  try {
    return fn();
  } finally {
    if (rowIds === 'all') rowVersions.bumpAll();
    else rowVersions.bump(rowIds);
    csVersion.value += 1;
  }
}
const estimate = ref<number | null>(null);
const exactCount = ref<number | null>(null);
const loading = ref(false);
const loadError = ref<string | null>(null);
const elapsedMs = ref<number | null>(null);
const pageSize = ref(200);
const offset = ref(0);
const hasNext = ref(false);
const sort = ref<SortKey[]>([]);
const applied = ref<GridFilter>({ mode: 'builder', conditions: [] });
const hidden = ref<Set<string>>(new Set());
const showColumns = ref(false);
const recordOpen = ref(true);
const focus = ref<{ rowId: string; column: string | null } | null>(null);
const problem = ref<ApplyProblem | null>(null);
const problemRow = ref<string | null>(null);
const written = ref<Set<string>>(new Set());

const readOnlyReason = computed(() => {
  if (!isTable.value) return `This is a ${props.object.kind === 'materializedView' ? 'materialized view' : props.object.kind}; its rows are shown read only.`;
  return tableReadOnlyReason({ readOnlySession: props.session.readOnly, hasKey: key.value !== null, keyRefused: keyReason.value });
});
const editable = computed(() => readOnlyReason.value === null && key.value !== null);
const counts = computed(() => {
  void csVersion.value;
  return changeCounts(cs);
});
watch(() => counts.value.total > 0, (d) => emit('dirty', d));

// --- filter editing ---

const filterMode = ref<'builder' | 'raw'>('builder');
const conditions = ref<FilterCondition[]>([]);
const editing = reactive<FilterCondition>({ column: '', op: '=', value: '' });
const rawFilter = ref('');
const filterDirty = computed(() => {
  const now: GridFilter = filterMode.value === 'raw' ? { mode: 'raw', sql: rawFilter.value } : { mode: 'builder', conditions: conditions.value };
  return JSON.stringify(now) !== JSON.stringify(applied.value);
});

function addCondition(): void {
  if (!editing.column) return;
  conditions.value = [...conditions.value, { ...editing }];
  editing.value = '';
}

function removeCondition(at: number): void {
  conditions.value = conditions.value.filter((_, i) => i !== at);
}

async function applyFilter(): Promise<void> {
  if (filterMode.value === 'builder' && editing.column && (editing.value !== '' || !operatorNeedsValue(editing.op))) addCondition();
  applied.value = filterMode.value === 'raw' ? { mode: 'raw', sql: rawFilter.value } : { mode: 'builder', conditions: [...conditions.value] };
  offset.value = 0;
  exactCount.value = null;
  await loadPage();
}

function conditionText(c: FilterCondition): { op: string; value: string } {
  const op = FILTER_OPERATORS.find((o) => o.op === c.op);
  return { op: op?.label ?? c.op, value: operatorNeedsValue(c.op) ? `'${c.value}'` : '' };
}

// --- loading ---

function synthColumns(names: { name: string; typeName: string }[]): ColumnInfo[] {
  return names.map((c) => ({
    name: c.name,
    dataType: c.typeName,
    typeSchema: null,
    typeName: c.typeName,
    baseType: null,
    array: false,
    arrayOfArrays: false,
    nullable: true,
    hasDefault: false,
    generated: null,
    identity: null,
    autoIncrement: false,
    editable: false,
  }));
}

async function init(): Promise<void> {
  loading.value = true;
  loadError.value = null;
  key.value = null;
  keyReason.value = null;
  if (isTable.value) {
    const [k, d] = await Promise.allSettled([
      editableKey(props.session.id, target.value),
      describeTable(props.session.id, tableRef(target.value, props.object.kind)),
    ]);
    if (k.status === 'fulfilled') {
      key.value = k.value;
      if (k.value) {
        keyReason.value = keyRefusal(k.value);
        columns.value = k.value.tableColumns;
      }
    } else {
      emit('error', k.reason);
      keyReason.value = `Varve could not find the key of this table, so its rows are read only: ${presentError(toCmdError(k.reason)).message}`;
    }
    if (d.status === 'fulfilled') {
      const n = d.value.object.rowEstimate;
      estimate.value = n !== null && n >= 0 ? n : null;
      if (!key.value) {
        const cols = d.value.engine === 'postgres'
          ? [...d.value.object.columns].sort((a, b) => a.position - b.position).map((c) => ({ name: c.name, typeName: c.dataType }))
          : [...d.value.object.columns].sort((a, b) => a.position - b.position).map((c) => ({ name: c.name, typeName: c.columnType }));
        columns.value = synthColumns(cols);
      }
    }
  }
  await loadPage();
}

async function loadPage(): Promise<void> {
  loading.value = true;
  loadError.value = null;
  let sql: string;
  try {
    sql = selectPageSql({
      dialect: dialect.value,
      target: target.value,
      columns: columns.value.map((c) => c.name),
      filter: applied.value,
      sort: sort.value,
      tiebreak: key.value?.columns ?? [],
      limit: pageSize.value + 1,
      offset: offset.value,
    });
  } catch (error: unknown) {
    loadError.value = error instanceof Error ? error.message : String(error);
    loading.value = false;
    return;
  }
  try {
    const out = await runGridQuery(props.session.id, sql, pageSize.value + 1);
    const result = out.results.find((r) => r.kind === 'rows');
    elapsedMs.value = out.elapsedMs;
    if (!result || result.kind !== 'rows') throw new Error('The query returned no rows result.');
    if (columns.value.length === 0) columns.value = synthColumns(result.columns);
    hasNext.value = result.rows.length > pageSize.value;
    const page = result.rows.slice(0, pageSize.value);
    if (key.value && !keyReason.value) {
      rows.value = markRaw(toGridRows(key.value, columns.value, page));
      const fresh = rows.value;
      rowVersions.bump(refreshSnapshots(cs, dialect.value, columns.value, fresh));
      csVersion.value += 1;
    } else {
      rows.value = markRaw(page.map((values, i) => ({ id: `r${offset.value + i}`, values })));
    }
  } catch (error: unknown) {
    emit('error', error);
    loadError.value = presentError(toCmdError(error)).message;
  } finally {
    loading.value = false;
  }
}

onMounted(init);

async function countExactly(): Promise<void> {
  try {
    const out = await runGridQuery(props.session.id, countSql(dialect.value, target.value, applied.value), 1);
    const r = out.results.find((x) => x.kind === 'rows');
    const v = r && r.kind === 'rows' ? r.rows[0]?.[0] : undefined;
    exactCount.value = v && (v.t === 'int' || v.t === 'uInt' || v.t === 'decimal') ? Number(v.v) : null;
  } catch (error: unknown) {
    emit('error', error);
    toasts.pushError(error, 'Could not count the rows');
  }
}

function page(by: 1 | -1): void {
  offset.value = Math.max(0, offset.value + by * pageSize.value);
  void loadPage();
}

watch(pageSize, () => {
  offset.value = 0;
  void loadPage();
});

// --- grid ---

/** Builds the grid rows, reusing the objects of rows that did not change (lib/gridRows.ts). */
const buildRows = createRowViewCache();

const viewRows = computed<ViewRow[]>(() => {
  void csVersion.value;
  return markRaw(
    buildRows({ rows: rows.value, offset: offset.value, changes: cs, versions: rowVersions, columnCount: columns.value.length, wrap: markRaw }),
  );
});

function cellOf(row: ViewRow, i: number): { pending: CellValue | null; value: Value | null } {
  const name = columns.value[i]?.name ?? '';
  return { pending: row.pending[name] ?? null, value: row.values[i] ?? null };
}

function inlineEditable(row: ViewRow, i: number): boolean {
  if (!editable.value || row.deleted) return false;
  const column = columns.value[i];
  if (!column) return false;
  const access = columnAccess(column);
  if (row.kind === 'insert' ? !access.insert : !access.update) return false;
  const { pending, value } = cellOf(row, i);
  if (pending && pending.kind !== 'value') return false;
  if (pending?.kind === 'value') return !opensInDialog(pending.value);
  return !(value && opensInDialog(value));
}

function displayText(row: ViewRow, i: number): string {
  const column = columns.value[i];
  const { pending, value } = cellOf(row, i);
  if (pending?.kind === 'value' && pending.value.t !== 'null') return previewText(pending.value);
  if (pending && column) return pendingCellText(pending, column);
  if (row.kind === 'insert') return column?.hasDefault ? 'DEFAULT' : 'NULL';
  return value ? previewText(value) : '';
}

const theme = themeQuartz.withParams({
  backgroundColor: 'var(--surface)',
  foregroundColor: 'var(--text)',
  headerBackgroundColor: 'var(--surface-raised)',
  headerTextColor: 'var(--text-muted)',
  borderColor: 'var(--border-inner)',
  oddRowBackgroundColor: 'var(--zebra)',
  rowHoverColor: 'var(--accent-tint)',
  selectedRowBackgroundColor: 'var(--accent-tint)',
  accentColor: 'var(--accent)',
  fontFamily: 'var(--font-mono)',
  fontSize: 12,
  headerFontFamily: 'var(--font-sans)',
  headerFontWeight: 500,
  rowHeight: 24,
  headerHeight: 40,
  cellHorizontalPadding: 8,
  wrapperBorderRadius: 0,
  borderRadius: 0,
  columnBorder: true,
  spacing: 4,
});

const columnDefs = computed<ColDef<ViewRow>[]>(() => [
  {
    headerName: '',
    colId: '__n',
    valueGetter: (p) => {
      const r = p.data;
      if (!r) return '';
      if (r.kind === 'insert') return '+';
      if (r.deleted) return '−';
      if (Object.keys(r.pending).length > 0) return '~';
      return String(r.n);
    },
    width: 56,
    pinned: 'left',
    sortable: false,
    editable: false,
    resizable: false,
    suppressMovable: true,
    cellClass: 'row-number',
  },
  ...columns.value.map((column, i): ColDef<ViewRow> => {
    const access = columnAccess(column);
    const isKey = key.value?.columns.includes(column.name) ?? false;
    return {
      colId: `c${i}`,
      headerName: column.name,
      hide: hidden.value.has(column.name),
      headerTooltip: [column.name, column.dataType, access.reason].filter(Boolean).join(' · '),
      headerComponentParams: {
        innerHeaderComponent: TypedHeader,
        innerHeaderComponentParams: { typeName: `${isKey ? 'key · ' : ''}${column.dataType}${access.update ? '' : ' · read only'}` },
      },
      sortable: true,
      comparator: () => 0,
      editable: (p) => (p.data ? inlineEditable(p.data, i) : false),
      valueGetter: (p) => {
        if (!p.data) return '';
        const { pending, value } = cellOf(p.data, i);
        if (pending?.kind === 'value') return cachedEditText(dialect.value, pending.value);
        if (pending) return '';
        return value ? cachedEditText(dialect.value, value) : '';
      },
      valueFormatter: (p) => (p.data ? displayText(p.data, i) : ''),
      valueSetter: (p: ValueSetterParams<ViewRow>) => {
        if (p.data) commitText(p.data, i, String(p.newValue ?? ''));
        return false;
      },
      cellClass: (p) => {
        const r = p.data;
        if (!r) return '';
        const { pending, value } = cellOf(r, i);
        const classes: string[] = [];
        if (pending) classes.push('c-changed', pending.kind === 'value' ? cellClass(pending.value) : 'v-special');
        else if (r.kind === 'insert') classes.push('v-special');
        else if (value) classes.push(cellClass(value));
        if (!access.update && r.kind === 'row') classes.push('c-locked');
        return classes.join(' ');
      },
      tooltipValueGetter: (p) => {
        const r = p.data;
        if (!r) return null;
        const { pending, value } = cellOf(r, i);
        if (pending && value) return `Was: ${previewText(value)}`;
        if (value && opensInDialog(value)) return tooltipText(value);
        return null;
      },
      minWidth: 80,
      maxWidth: 560,
    };
  }),
]);

const gridApi = shallowRef<GridApi<ViewRow> | null>(null);

const gridOptions: GridOptions<ViewRow> = {
  getRowId: (p) => p.data.id,
  getRowClass: (p) => {
    const r = p.data;
    if (!r) return '';
    const out: string[] = [];
    if (r.kind === 'insert') out.push('r-insert');
    if (r.deleted) out.push('r-deleted');
    if (problemRow.value === r.id) out.push('r-conflict');
    if (written.value.has(r.id)) out.push('r-written');
    return out.join(' ');
  },
  tooltipShowDelay: 400,
  // Rows and columns outside the viewport are not rendered (row and column virtualisation stay on).
  rowBuffer: 10,
  stopEditingWhenCellsLoseFocus: true,
  suppressDragLeaveHidesColumns: true,
  autoSizeStrategy: { type: 'fitCellContents', skipHeader: false },
  onGridReady: (e: GridReadyEvent<ViewRow>) => {
    gridApi.value = e.api;
  },
  onSortChanged: (e: SortChangedEvent<ViewRow>) => onSortChanged(e),
  onCellFocused: (e: CellFocusedEvent<ViewRow>) => onCellFocused(e),
  onCellDoubleClicked: (e: CellDoubleClickedEvent<ViewRow>) => onCellDoubleClicked(e),
};

watch([problemRow, written], () => gridApi.value?.redrawRows());

function columnIndexOf(colId: string | null | undefined): number {
  return colId && colId.startsWith('c') ? Number(colId.slice(1)) : -1;
}

function onSortChanged(e: SortChangedEvent<ViewRow>): void {
  const state = e.api
    .getColumnState()
    .filter((s) => s.sort && s.colId.startsWith('c'))
    .sort((a, b) => (a.sortIndex ?? 0) - (b.sortIndex ?? 0));
  const next = state
    .map((s) => ({ column: columns.value[columnIndexOf(s.colId)]?.name ?? '', descending: s.sort === 'desc' }))
    .filter((s) => s.column !== '');
  if (JSON.stringify(next) === JSON.stringify(sort.value)) return;
  sort.value = next;
  offset.value = 0;
  void loadPage();
}

function onCellFocused(e: CellFocusedEvent<ViewRow>): void {
  if (e.rowIndex === null || e.rowIndex === undefined) return;
  const node = e.api.getDisplayedRowAtIndex(e.rowIndex);
  const colId = typeof e.column === 'string' ? e.column : e.column?.getColId();
  const i = columnIndexOf(colId);
  if (node?.data) focus.value = { rowId: node.data.id, column: columns.value[i]?.name ?? null };
}

function onCellDoubleClicked(e: CellDoubleClickedEvent<ViewRow>): void {
  const i = columnIndexOf(e.column.getColId());
  if (!e.data || i < 0 || inlineEditable(e.data, i)) return;
  openEditor(e.data.id, columns.value[i]?.name ?? '');
}

// --- edits ---

function findRow(id: string): GridRowData | undefined {
  return rows.value.find((r) => r.id === id);
}

function setFor(rowId: string, column: string, cell: CellValue | null): void {
  if (!key.value) return;
  try {
    if (rowId.startsWith('new:')) mutate([rowId], () => setInsertCell(cs, columns.value, rowId, column, cell));
    else {
      const row = findRow(rowId);
      if (!row) throw new Error('That row is no longer on this page.');
      const k = key.value;
      if (cell === null) mutate([rowId], () => revertCell(cs, rowId, column));
      else mutate([rowId], () => setCell(cs, dialect.value, k, columns.value, row, column, cell));
    }
    if (problem.value && problemRow.value === rowId) problemRow.value = null;
  } catch (error: unknown) {
    toasts.push('warning', 'Not changed', error instanceof Error ? error.message : String(error));
  }
}

function commitText(row: ViewRow, i: number, text: string): void {
  const column = columns.value[i];
  if (!column) return;
  const parsed = parseCellText(dialect.value, column, text);
  if (!parsed.ok) {
    toasts.push('warning', 'Not changed', parsed.message);
    return;
  }
  setFor(row.id, column.name, parsed.cell);
}

const focusedRow = computed(() => viewRows.value.find((r) => r.id === focus.value?.rowId) ?? null);
const focusedColumn = computed(() => columns.value.find((c) => c.name === focus.value?.column) ?? null);

function addRow(): void {
  const id = mutate([], () => addInsert(cs));
  focus.value = { rowId: id, column: null };
}

function toggleDeleteFocused(): void {
  const r = focusedRow.value;
  if (!r || !key.value) return;
  if (r.kind === 'insert') {
    mutate([r.id], () => removeInsert(cs, r.id));
    focus.value = null;
    return;
  }
  const row = findRow(r.id);
  const k = key.value;
  if (row) mutate([row.id], () => toggleDelete(cs, k, columns.value, row));
}

function setSpecial(kind: 'null' | 'default'): void {
  const r = focusedRow.value;
  const c = focusedColumn.value;
  if (!r || !c) return;
  setFor(r.id, c.name, kind === 'null' ? { kind: 'value', value: { t: 'null' } } : r.kind === 'insert' ? null : { kind: 'default' });
}

function revertFocused(): void {
  const r = focusedRow.value;
  const c = focusedColumn.value;
  if (r && c && r.kind === 'row') mutate([r.id], () => revertCell(cs, r.id, c.name));
}

// --- value editor dialog ---

const editor = ref<{ rowId: string; column: ColumnInfo; current: CellValue; insert: boolean; readOnly: boolean } | null>(null);

function openEditor(rowId: string, columnName: string): void {
  const column = columns.value.find((c) => c.name === columnName);
  const row = viewRows.value.find((r) => r.id === rowId);
  if (!column || !row) return;
  const i = columns.value.indexOf(column);
  const { pending, value } = cellOf(row, i);
  const current: CellValue = pending ?? (row.kind === 'insert' ? { kind: 'default' } : { kind: 'value', value: value ?? { t: 'null' } });
  editor.value = { rowId, column, current, insert: row.kind === 'insert', readOnly: !editable.value || row.deleted };
}

function editorDone(answer: CellValue | null): void {
  const e = editor.value;
  editor.value = null;
  if (!e || !answer || e.readOnly) return;
  if (e.insert && answer.kind === 'default') setFor(e.rowId, e.column.name, null);
  else setFor(e.rowId, e.column.name, answer);
}

// --- record view ---

function recordValue(i: number): { text: string; isNull: boolean; special: string | null; large: boolean } {
  const r = focusedRow.value;
  if (!r) return { text: '', isNull: false, special: null, large: false };
  const column = columns.value[i];
  const { pending, value } = cellOf(r, i);
  if (pending && pending.kind !== 'value' && column) return { text: '', isNull: false, special: pendingCellText(pending, column), large: false };
  if (!pending && r.kind === 'insert') return { text: '', isNull: false, special: column?.hasDefault ? 'DEFAULT' : 'DEFAULT (NULL)', large: false };
  const v = pending?.kind === 'value' ? pending.value : value;
  if (!v || v.t === 'null') return { text: '', isNull: true, special: null, large: false };
  return { text: cachedEditText(dialect.value, v), isNull: false, special: null, large: opensInDialog(v) };
}

function recordWritable(i: number): boolean {
  const r = focusedRow.value;
  const c = columns.value[i];
  if (!r || !c || !editable.value || r.deleted) return false;
  const access = columnAccess(c);
  return r.kind === 'insert' ? access.insert : access.update;
}

function recordInput(i: number, event: Event): void {
  const r = focusedRow.value;
  if (!r) return;
  commitText(r, i, (event.target as HTMLInputElement | HTMLTextAreaElement).value);
}

function recordNull(i: number, event: Event): void {
  const r = focusedRow.value;
  const c = columns.value[i];
  if (!r || !c) return;
  if ((event.target as HTMLInputElement).checked) setFor(r.id, c.name, { kind: 'value', value: { t: 'null' } });
  else setFor(r.id, c.name, { kind: 'value', value: { t: 'text', v: '' } });
}

function moveRecord(by: 1 | -1): void {
  const list = viewRows.value;
  const at = list.findIndex((r) => r.id === focus.value?.rowId);
  const next = list[at + by];
  if (next) focus.value = { rowId: next.id, column: focus.value?.column ?? null };
}

const focusedIndex = computed(() => viewRows.value.findIndex((r) => r.id === focus.value?.rowId));

// --- review and apply ---

const review = reactive<{
  open: boolean;
  plan: RowEditPlan | null;
  planId: string | null;
  refs: ChangeRef[];
  labels: string[];
  checkOriginal: boolean;
  busy: boolean;
  message: { tone: 'danger' | 'warning'; text: string } | null;
}>({ open: false, plan: null, planId: null, refs: [], labels: [], checkOriginal: true, busy: false, message: null });

async function plan(): Promise<void> {
  if (!key.value) return;
  const { changes, refs } = buildRowChanges(cs, columns.value);
  review.refs = refs;
  review.labels = changeLabels(refs);
  review.plan = null;
  review.planId = null;
  review.message = null;
  try {
    const planned = await rowEditPlan(props.session.id, target.value, changes, { checkOriginal: review.checkOriginal, key: key.value });
    review.plan = planned.plan;
    review.planId = planned.planId;
  } catch (error: unknown) {
    emit('error', error);
    const p = presentError(toCmdError(error));
    review.message = { tone: 'danger', text: `${p.message}${p.detail ? ` ${p.detail}` : ''}` };
  }
}

async function openReview(): Promise<void> {
  const missing = cs.inserts.map((r) => missingRequired(columns.value, r)).find((m) => m !== null);
  if (missing) {
    toasts.push('warning', 'A new row is incomplete', `${missing} is NOT NULL and has no default: fill it in, or remove the row.`);
    return;
  }
  problem.value = null;
  problemRow.value = null;
  review.open = true;
  await plan();
}

watch(
  () => review.checkOriginal,
  () => {
    if (review.open) void plan();
  },
);

async function apply(): Promise<void> {
  if (!review.planId || !key.value) return;
  const planId = review.planId;
  review.busy = true;
  review.message = null;
  try {
    const outcome = await runWithConfirmation(
      (confirmed) => rowEditApply(props.session.id, planId, confirmed),
      (request) => dialogs.askDestructive({ connectionName: props.connectionName, environment: request.environment, statements: request.statements }),
    );
    if (outcome === null) {
      review.message = { tone: 'warning', text: 'Not confirmed: nothing was applied. Apply again to confirm.' };
      return;
    }
    review.open = false;
    review.planId = null;
    if (outcome.status === 'applied') {
      const k = key.value;
      const merged = mutate('all', () => mergeApplied(cs, k, columns.value, rows.value, review.refs, outcome.rows));
      rows.value = markRaw(merged.rows);
      written.value = new Set(merged.written);
      setTimeout(() => {
        written.value = new Set();
      }, 2500);
      toasts.push('success', 'Changes applied', `${outcome.rows.length} ${outcome.rows.length === 1 ? 'change' : 'changes'} written to ${qualified.value}.`);
      emit('changed');
      if (merged.needsReload) await loadPage();
      return;
    }
    problem.value = describeProblem(outcome, review.refs);
    problemRow.value = problem.value.ref?.id ?? null;
  } catch (error: unknown) {
    const cmd = toCmdError(error);
    const action = applyFailureAction(cmd);
    if (action === 'keep') {
      review.message = { tone: 'warning', text: `${openTransactionOf(cmd) ?? ''} Commit or roll back in the console first, then apply again; the plan is kept.` };
      return;
    }
    if (action === 'replan') {
      // Nothing ran: plan the pending changes again for a fresh review.
      await plan();
      if (review.planId) {
        review.message = { tone: 'warning', text: 'The plan expired before it ran, so nothing was applied. It was planned again from your pending changes: review it and apply again.' };
      }
      return;
    }
    emit('error', error);
    const p = presentError(cmd);
    review.open = false;
    problem.value = { ref: null, conflict: false, title: p.title, message: `${p.message}${p.detail ? ` ${p.detail}` : ''}` };
  } finally {
    review.busy = false;
  }
}

async function reloadAfterProblem(): Promise<void> {
  await loadPage();
  const id = problemRow.value;
  if (id && !id.startsWith('new:') && !rows.value.some((r) => r.id === id) && problem.value) {
    problem.value = {
      ...problem.value,
      message: `The row ${keyLabel(id)} is not on this page any more: someone may have deleted it or changed its key. Discard the change to it, or find it with a filter.`,
    };
  } else if (problem.value) {
    problem.value = { ...problem.value, title: 'Rows read again', message: 'The rows show what the table holds now; your changes stay pending. Review and apply again.' };
  }
}

function discardProblemChange(): void {
  const ref = problem.value?.ref;
  if (ref) mutate([ref.id], () => discardRef(cs, ref));
  problem.value = null;
  problemRow.value = null;
}

async function discard(): Promise<void> {
  const ok = await dialogs.askConfirm({
    title: 'Discard pending changes?',
    message: `${counts.value.total} pending ${counts.value.total === 1 ? 'change' : 'changes'} to ${qualified.value} are dropped.`,
    confirmLabel: 'Discard',
    danger: true,
  });
  if (!ok) return;
  mutate('all', () => discardAll(cs));
  problem.value = null;
  problemRow.value = null;
}

async function refresh(): Promise<void> {
  exactCount.value = null;
  await loadPage();
}

function toggleColumn(name: string): void {
  const next = new Set(hidden.value);
  if (next.has(name)) next.delete(name);
  else next.add(name);
  hidden.value = next;
}

const rangeText = computed(() => {
  const shown = rows.value.length;
  if (shown === 0) return offset.value === 0 ? 'No rows' : `Nothing after row ${formatCount(offset.value)}`;
  return `${formatCount(offset.value + 1)} to ${formatCount(offset.value + shown)}`;
});

const sortText = computed(() => (sort.value.length === 0 ? (key.value ? `${key.value.columns.join(', ')} (key)` : 'none') : sort.value.map((s) => `${s.column} ${s.descending ? 'DESC' : 'ASC'}`).join(', ')));
</script>

<template>
  <div class="data">
    <!-- Filter bar -->
    <div class="filter">
      <span class="label where"><Icon name="search" />Where</span>
      <template v-if="filterMode === 'builder'">
        <span
          v-for="(c, n) in conditions"
          :key="n"
          class="chip mono"
        >
          {{ c.column }} <span class="op">{{ conditionText(c).op }}</span> <span class="val">{{ conditionText(c).value }}</span>
          <button
            type="button"
            class="icon-btn small"
            :aria-label="`Remove condition ${c.column} ${conditionText(c).op} ${c.value}`"
            @click="removeCondition(n)"
          >
            <Icon
              name="close"
              :size="12"
            />
          </button>
        </span>
        <span
          v-if="conditions.length > 0"
          class="and"
        >AND</span>
        <div class="builder mono">
          <label
            class="visually-hidden"
            :for="`${uid}-fcol`"
          >Column</label>
          <select
            :id="`${uid}-fcol`"
            v-model="editing.column"
          >
            <option value="">
              column
            </option>
            <option
              v-for="c in columns"
              :key="c.name"
              :value="c.name"
            >
              {{ c.name }}
            </option>
          </select>
          <label
            class="visually-hidden"
            :for="`${uid}-fop`"
          >Operator</label>
          <select
            :id="`${uid}-fop`"
            v-model="editing.op"
            class="op"
          >
            <option
              v-for="o in FILTER_OPERATORS"
              :key="o.op"
              :value="o.op"
            >
              {{ o.label }}
            </option>
          </select>
          <label
            class="visually-hidden"
            :for="`${uid}-fval`"
          >Value</label>
          <input
            v-if="operatorNeedsValue(editing.op)"
            :id="`${uid}-fval`"
            v-model="editing.value"
            class="val"
            spellcheck="false"
            @keydown.enter.prevent="applyFilter"
          >
        </div>
        <button
          type="button"
          class="btn compact ghost"
          :disabled="!editing.column"
          @click="addCondition"
        >
          <Icon name="plus" />Condition
        </button>
      </template>
      <template v-else>
        <label
          class="visually-hidden"
          :for="`${uid}-raw`"
        >WHERE clause</label>
        <input
          :id="`${uid}-raw`"
          v-model="rawFilter"
          class="input compact mono raw"
          placeholder="status = 'paid' AND total > 100"
          spellcheck="false"
          @keydown.enter.prevent="applyFilter"
        >
      </template>
      <button
        type="button"
        class="btn compact"
        @click="applyFilter"
      >
        Apply filter
      </button>
      <span
        v-if="filterDirty"
        class="subtle small"
      >Not applied yet</span>
      <span class="spacer" />
      <div
        role="group"
        aria-label="Filter mode"
        class="segmented small-seg"
      >
        <button
          type="button"
          :aria-pressed="filterMode === 'builder'"
          @click="filterMode = 'builder'"
        >
          Builder
        </button>
        <button
          type="button"
          :aria-pressed="filterMode === 'raw'"
          @click="filterMode = 'raw'"
        >
          Raw SQL
        </button>
      </div>
      <span class="sep" />
      <span
        class="small muted"
        title="Click a column header to sort; Shift+click adds a column"
      >Sort: <span class="mono">{{ sortText }}</span></span>
      <div class="cols-menu">
        <button
          type="button"
          class="btn compact"
          :aria-expanded="showColumns"
          @click="showColumns = !showColumns"
        >
          Columns {{ columns.length - hidden.size }} of {{ columns.length }}
        </button>
        <div
          v-if="showColumns"
          class="menu"
          role="group"
          aria-label="Visible columns"
        >
          <label
            v-for="c in columns"
            :key="c.name"
            class="menu-item mono"
          ><input
            type="checkbox"
            :checked="!hidden.has(c.name)"
            @change="toggleColumn(c.name)"
          >{{ c.name }}</label>
        </div>
      </div>
      <button
        type="button"
        class="icon-btn bordered"
        aria-label="Refresh data"
        title="Read the rows again"
        @click="refresh"
      >
        <Icon name="refresh" />
      </button>
    </div>

    <!-- Edit toolbar and notices -->
    <div class="edit-bar">
      <template v-if="editable">
        <button
          type="button"
          class="btn compact"
          @click="addRow"
        >
          <Icon name="plus" />Add row
        </button>
        <button
          type="button"
          class="btn compact"
          :disabled="!focusedRow"
          @click="toggleDeleteFocused"
        >
          {{ focusedRow?.deleted ? 'Undo delete' : focusedRow?.kind === 'insert' ? 'Remove new row' : 'Delete row' }}
        </button>
        <span class="sep" />
        <button
          type="button"
          class="btn compact ghost"
          :disabled="!focusedColumn || !focusedRow || !focusedColumn.nullable"
          @click="setSpecial('null')"
        >
          Set NULL
        </button>
        <button
          type="button"
          class="btn compact ghost"
          :disabled="!focusedColumn || !focusedRow"
          @click="setSpecial('default')"
        >
          Set DEFAULT
        </button>
        <button
          type="button"
          class="btn compact ghost"
          :disabled="!focusedColumn || !focusedRow"
          @click="focusedRow && focusedColumn && openEditor(focusedRow.id, focusedColumn.name)"
        >
          Edit value…
        </button>
        <button
          type="button"
          class="btn compact ghost"
          :disabled="!focusedRow || !focusedColumn || !(focusedColumn.name in (focusedRow?.pending ?? {}))"
          @click="revertFocused"
        >
          Revert cell
        </button>
      </template>
      <span
        v-else-if="readOnlyReason"
        class="read-only small"
      ><Icon name="lock" />{{ readOnlyReason }}</span>
      <span class="spacer" />
      <button
        type="button"
        class="btn compact ghost"
        :aria-pressed="recordOpen"
        @click="recordOpen = !recordOpen"
      >
        Record view
      </button>
    </div>

    <div
      v-if="problem"
      class="notice problem"
      :class="problem.conflict ? 'tone-warning' : 'tone-danger'"
      role="alert"
    >
      <div class="problem-text">
        <strong>{{ problem.title }}</strong>
        <span>{{ problem.message }}</span>
      </div>
      <button
        type="button"
        class="btn compact"
        @click="reloadAfterProblem"
      >
        Reload rows
      </button>
      <button
        v-if="problem.ref"
        type="button"
        class="btn compact"
        @click="discardProblemChange"
      >
        Discard this change
      </button>
      <button
        type="button"
        class="icon-btn"
        aria-label="Dismiss"
        @click="problem = null; problemRow = null"
      >
        <Icon name="close" />
      </button>
    </div>
    <p
      v-if="loadError"
      class="notice tone-danger problem"
      role="alert"
    >
      {{ loadError }}
    </p>

    <div class="main">
      <div class="grid-wrap">
        <div
          class="grid"
          role="region"
          :aria-label="`${qualified} rows`"
          :aria-busy="loading"
        >
          <AgGridVue
            class="grid-inner"
            :theme="theme"
            :row-data="viewRows"
            :column-defs="columnDefs"
            :grid-options="gridOptions"
          />
        </div>
        <div class="pager">
          <button
            type="button"
            class="icon-btn bordered"
            aria-label="Previous page"
            :disabled="offset === 0 || loading"
            @click="page(-1)"
          >
            <Icon
              name="chevronRight"
              class="flip"
            />
          </button>
          <span class="mono strong">{{ rangeText }}</span>
          <button
            type="button"
            class="icon-btn bordered"
            aria-label="Next page"
            :disabled="!hasNext || loading"
            @click="page(1)"
          >
            <Icon name="chevronRight" />
          </button>
          <span v-if="exactCount !== null">of <span class="mono strong">{{ formatCount(exactCount) }}</span> rows</span>
          <span v-else-if="estimate !== null && applied.mode === 'builder' && applied.conditions.length === 0">of <span class="mono strong">{{ formatCount(estimate) }}</span> rows (estimate)</span>
          <button
            v-if="exactCount === null"
            type="button"
            class="btn compact ghost link"
            @click="countExactly"
          >
            Count exactly
          </button>
          <span class="spacer" />
          <span
            v-if="elapsedMs !== null"
            class="subtle"
          >{{ formatDuration(elapsedMs) }}</span>
          <label :for="`${uid}-page`">Page size</label>
          <select
            :id="`${uid}-page`"
            v-model.number="pageSize"
            class="page-size"
          >
            <option :value="100">
              100
            </option>
            <option :value="200">
              200
            </option>
            <option :value="500">
              500
            </option>
            <option :value="1000">
              1000
            </option>
          </select>
          <span class="mono subtle">{{ key ? `Ordered by ${key.columns.join(', ')}` : 'No key: order is not stable' }}</span>
        </div>
      </div>

      <aside
        v-if="recordOpen"
        class="record"
        aria-label="Record view"
      >
        <div class="record-head">
          <span class="strong">Record</span>
          <span
            v-if="focusedRow"
            class="small muted"
          >{{ focusedRow.kind === 'insert' ? `new row ${focusedRow.n}` : `row ${focusedRow.n}` }}</span>
          <span class="spacer" />
          <button
            type="button"
            class="icon-btn"
            aria-label="Previous record"
            :disabled="focusedIndex <= 0"
            @click="moveRecord(-1)"
          >
            <Icon
              name="chevronDown"
              class="flip"
            />
          </button>
          <button
            type="button"
            class="icon-btn"
            aria-label="Next record"
            :disabled="focusedIndex < 0 || focusedIndex >= viewRows.length - 1"
            @click="moveRecord(1)"
          >
            <Icon name="chevronDown" />
          </button>
          <button
            type="button"
            class="icon-btn"
            aria-label="Close record view"
            @click="recordOpen = false"
          >
            <Icon name="close" />
          </button>
        </div>
        <div
          v-if="!focusedRow"
          class="record-empty subtle small"
        >
          Select a cell to see its row here.
        </div>
        <div
          v-else
          class="record-body"
        >
          <p
            v-if="focusedRow.deleted"
            class="notice tone-danger"
          >
            Marked for deletion.
          </p>
          <div
            v-for="(c, i) in columns"
            :key="c.name"
            class="rfield"
          >
            <div class="rlabel">
              <label
                :for="`${uid}-r${i}`"
                class="mono"
              >{{ c.name }}</label>
              <span class="subtle small">{{ c.dataType }}{{ c.identity ? ` · identity ${c.identity === 'always' ? 'always' : 'by default'}` : '' }}{{ c.generated ? ' · generated' : '' }}{{ c.nullable ? ' · nullable' : '' }}</span>
            </div>
            <template v-if="recordValue(i).special">
              <div class="special">
                <span
                  :id="`${uid}-r${i}`"
                  class="mono subtle"
                >{{ recordValue(i).special }}</span>
                <button
                  v-if="recordWritable(i)"
                  type="button"
                  class="btn compact ghost"
                  @click="openEditor(focusedRow.id, c.name)"
                >
                  Edit
                </button>
              </div>
            </template>
            <template v-else-if="recordValue(i).large">
              <textarea
                :id="`${uid}-r${i}`"
                class="textarea"
                rows="4"
                readonly
                :value="recordValue(i).text"
              />
              <button
                type="button"
                class="btn compact ghost"
                @click="openEditor(focusedRow.id, c.name)"
              >
                {{ recordWritable(i) ? 'Edit in full' : 'Show in full' }}
              </button>
            </template>
            <template v-else>
              <input
                :id="`${uid}-r${i}`"
                class="input compact mono"
                :value="recordValue(i).text"
                :placeholder="recordValue(i).isNull ? 'NULL' : ''"
                :readonly="!recordWritable(i)"
                :disabled="recordValue(i).isNull && recordWritable(i)"
                @change="recordInput(i, $event)"
              >
            </template>
            <label
              v-if="c.nullable && recordWritable(i) && !recordValue(i).special"
              class="inline small"
            ><input
              type="checkbox"
              :checked="recordValue(i).isNull"
              @change="recordNull(i, $event)"
            >NULL</label>
            <span
              v-if="!recordWritable(i) && columnAccess(c).reason"
              class="subtle small"
            >{{ columnAccess(c).reason }}</span>
          </div>
          <p class="subtle small">
            Edits here join the pending changes below. Nothing is written until you apply.
          </p>
        </div>
      </aside>
    </div>

    <!-- Pending changes -->
    <div
      v-if="counts.total > 0"
      class="pending"
    >
      <span class="pill tone-warning"><span class="dot" />{{ counts.total }} {{ counts.total === 1 ? 'change' : 'changes' }}</span>
      <span class="parts">
        <span
          v-if="counts.updated"
          class="pill tone-warning"
        >{{ counts.updated }} updated</span>
        <span
          v-if="counts.inserted"
          class="pill tone-success"
        >{{ counts.inserted }} inserted</span>
        <span
          v-if="counts.deleted"
          class="pill tone-danger"
        >{{ counts.deleted }} deleted</span>
      </span>
      <span class="muted small">Applied in one transaction on {{ connectionName }} ({{ session.environment }})</span>
      <span class="spacer" />
      <button
        type="button"
        class="btn compact"
        @click="discard"
      >
        Discard
      </button>
      <button
        type="button"
        class="btn compact"
        @click="openReview"
      >
        Preview SQL
      </button>
      <button
        type="button"
        class="btn compact primary"
        @click="openReview"
      >
        Apply {{ counts.total }} {{ counts.total === 1 ? 'change' : 'changes' }}
      </button>
    </div>

    <RowEditReview
      v-if="review.open"
      v-model:check-original="review.checkOriginal"
      :plan="review.plan"
      :labels="review.labels"
      :busy="review.busy"
      :message="review.message"
      :connection-name="connectionName"
      :environment="session.environment"
      @apply="apply"
      @close="review.open = false"
    />
    <ValueEditorDialog
      v-if="editor"
      :column="editor.column"
      :dialect="dialect"
      :current="editor.current"
      :insert="editor.insert"
      :read-only="editor.readOnly"
      @done="editorDone"
    />
  </div>
</template>

<style scoped>
.data {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--surface);
}

.filter,
.edit-bar {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 44px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border);
}

.edit-bar {
  height: 38px;
  border-bottom-color: var(--border-inner);
}

.where {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 26px;
  padding: 0 4px 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface-raised);
  font-size: 12px;
}

.op {
  color: var(--code-keyword);
}

.val {
  color: var(--code-string);
}

.and {
  font-size: 11px;
  font-weight: 500;
  color: var(--text-muted);
}

.builder {
  display: flex;
  align-items: center;
  height: 28px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
  font-size: 12px;
}

.builder:focus-within {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--focus-ring);
}

.builder select,
.builder input {
  height: 26px;
  padding: 0 6px;
  border: 0;
  border-right: 1px solid var(--border-inner);
  background: transparent;
  color: var(--text);
  font-family: var(--font-mono);
  font-size: 12px;
  outline: none;
}

.builder input {
  width: 140px;
  border-right: 0;
  color: var(--code-string);
}

.builder select.op {
  color: var(--code-keyword);
}

.raw {
  width: 420px;
  height: 28px;
}

.small {
  font-size: 12px;
}

.spacer {
  flex: 1;
}

.sep {
  width: 1px;
  height: 20px;
  background: var(--border-inner);
}

.small-seg button {
  height: 24px;
  padding: 0 10px;
  font-size: 12px;
}

.cols-menu {
  position: relative;
}

.menu {
  position: absolute;
  top: 32px;
  right: 0;
  z-index: 20;
  display: flex;
  flex-direction: column;
  max-height: 320px;
  min-width: 200px;
  padding: 6px;
  overflow: auto;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
  box-shadow: var(--shadow-dialog);
}

.menu-item {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 26px;
  padding: 0 6px;
  font-size: 12px;
}

.icon-btn.bordered {
  border: 1px solid var(--border);
}

.icon-btn.small {
  width: 18px;
  height: 18px;
}

.read-only {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  color: var(--text-muted);
}

.problem {
  flex-shrink: 0;
  align-items: center;
  margin: 8px 12px;
}

.problem-text {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 2px;
}

.main {
  display: flex;
  flex: 1;
  min-height: 0;
}

.grid-wrap {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
}

.grid {
  flex: 1;
  min-height: 0;
}

.grid-inner {
  height: 100%;
}

.grid :deep(.row-number) {
  color: var(--text-subtle);
  text-align: right;
  font-variant-numeric: tabular-nums;
}

.grid :deep(.v-null),
.grid :deep(.v-special) {
  color: var(--text-subtle);
  font-style: italic;
}

.grid :deep(.v-num) {
  text-align: right;
  font-variant-numeric: tabular-nums;
}

.grid :deep(.c-changed) {
  background: var(--warning-tint);
  box-shadow: inset 2px 0 0 var(--warning);
}

.grid :deep(.c-locked) {
  color: var(--text-muted);
}

.grid :deep(.r-insert) {
  background: var(--success-tint);
}

.grid :deep(.r-insert .row-number) {
  color: var(--success-text);
  font-weight: 600;
}

.grid :deep(.r-deleted) {
  background: var(--danger-tint);
  color: var(--danger-text);
  text-decoration: line-through;
}

.grid :deep(.r-deleted .row-number) {
  color: var(--danger-text);
  font-weight: 600;
  text-decoration: none;
}

.grid :deep(.r-conflict) {
  box-shadow: inset 3px 0 0 var(--danger);
}

.grid :deep(.r-written) {
  background: var(--success-tint);
}

.grid :deep(.typed-header) {
  display: flex;
  flex-direction: column;
  min-width: 0;
  line-height: 1.25;
}

.grid :deep(.typed-header-name) {
  overflow: hidden;
  color: var(--text);
  text-overflow: ellipsis;
}

.grid :deep(.typed-header-type) {
  overflow: hidden;
  font-family: var(--font-mono);
  font-size: 11px;
  font-weight: 400;
  color: var(--text-subtle);
  text-overflow: ellipsis;
}

.pager {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 32px;
  padding: 0 12px;
  border-top: 1px solid var(--border);
  color: var(--text-muted);
  font-size: 12px;
}

.flip {
  transform: rotate(180deg);
}

.strong {
  color: var(--text);
  font-weight: 600;
}

.link {
  color: var(--accent);
}

.page-size {
  height: 24px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
  color: var(--text);
  font-size: 12px;
}

.record {
  display: flex;
  flex-direction: column;
  flex-shrink: 0;
  width: 300px;
  border-left: 1px solid var(--border);
}

.record-head {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 8px 0 16px;
  border-bottom: 1px solid var(--border-inner);
}

.record-empty {
  padding: 16px;
}

.record-body {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 12px 16px;
  overflow: auto;
}

.rfield {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.rlabel {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  font-size: 12px;
}

.special {
  display: flex;
  align-items: center;
  justify-content: space-between;
  height: 28px;
}

.inline {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.input.compact {
  height: 30px;
  font-size: 12px;
}

.pending {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 10px;
  height: 48px;
  padding: 0 12px;
  border-top: 1px solid var(--border);
  background: var(--surface);
}

.parts {
  display: inline-flex;
  gap: 6px;
}

.notice {
  margin: 0;
}
</style>
