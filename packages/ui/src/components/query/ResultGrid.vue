<script setup lang="ts">
import { AllCommunityModule, ModuleRegistry, themeQuartz } from 'ag-grid-community';
import type { CellClickedEvent, CellKeyDownEvent, ColDef, FullWidthCellKeyDownEvent, GridOptions } from 'ag-grid-community';
import { AgGridVue } from 'ag-grid-vue3';
import { computed, markRaw, toRaw } from 'vue';
import type { Column, Value } from '../../api/sessions';
import { exportText, toTsv } from '../../lib/exportResult';
import { opensInDialog, previewText, tooltipText } from '../../lib/gridPreview';
import { cellClass, compareValues, isPlaceholder, placeholderHelp } from '../../lib/values';
import { TypedHeader } from './typedHeader';

ModuleRegistry.registerModules([AllCommunityModule]);

interface GridRow {
  n: number;
  cells: Value[];
}

const props = defineProps<{ columns: Column[]; rows: Value[][]; label: string }>();
const emit = defineEmits<{ select: [cell: { column: Column; value: Value } | null]; copied: [what: string] }>();

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
  rangeSelectionBorderColor: 'var(--accent)',
  fontFamily: 'var(--font-mono)',
  fontSize: 12,
  headerFontFamily: 'var(--font-sans)',
  headerFontWeight: 500,
  rowHeight: 24,
  headerHeight: 40,
  cellHorizontalPadding: 10,
  wrapperBorderRadius: 0,
  borderRadius: 0,
  columnBorder: true,
  spacing: 4,
});

// Rows go to AG Grid as plain, non-reactive objects: the grid reads every
// visible cell on every scroll frame, and reactive proxies would add a trap
// per read (and let deep watchers walk every value).
const rowData = computed<GridRow[]>(() => markRaw(toRaw(props.rows).map((cells, i) => markRaw({ n: i + 1, cells: toRaw(cells) }))));

const columnDefs = computed<ColDef<GridRow>[]>(() => [
  {
    headerName: '#',
    colId: '__n',
    valueGetter: (p) => p.data?.n ?? null,
    width: 64,
    pinned: 'left',
    sortable: false,
    resizable: false,
    suppressMovable: true,
    cellClass: 'row-number',
  },
  ...props.columns.map(
    (column, i): ColDef<GridRow> => ({
      colId: `c${i}`,
      headerName: column.name,
      headerTooltip: `${column.name} (${column.typeName})`,
      headerComponentParams: { innerHeaderComponent: TypedHeader, innerHeaderComponentParams: { typeName: column.typeName } },
      valueGetter: (p) => p.data?.cells[i] ?? null,
      valueFormatter: (p) => (p.value ? previewText(p.value as Value) : ''),
      comparator: (a: Value | null, b: Value | null) => (a && b ? compareValues(a, b) : 0),
      cellClass: (p) => (p.value ? cellClass(p.value as Value) : ''),
      tooltipValueGetter: (p) => {
        const value = p.value as Value | null;
        if (!value) return null;
        if (isPlaceholder(value)) return placeholderHelp(value);
        if (opensInDialog(value)) return tooltipText(value);
        const preview = previewText(value);
        return preview.length > 60 ? preview : null;
      },
      minWidth: 80,
      maxWidth: 640,
    }),
  ),
]);

const gridOptions: GridOptions<GridRow> = {
  enableCellTextSelection: true,
  ensureDomOrder: true,
  tooltipShowDelay: 400,
  suppressDragLeaveHidesColumns: true,
  autoSizeStrategy: { type: 'fitCellContents', skipHeader: false },
  // Rows and columns outside the viewport are not rendered (virtualisation stays on).
  rowBuffer: 10,
  onCellClicked: (event) => onCellClicked(event),
  onCellKeyDown: (event) => onCellKeyDown(event),
};

function cellAt(rowIndex: number, colId: string | undefined): { column: Column; value: Value } | null {
  if (!colId || !colId.startsWith('c')) return null;
  const col = Number(colId.slice(1));
  const column = props.columns[col];
  const value = props.rows[rowIndex]?.[col];
  return column && value ? { column, value } : null;
}

function onCellClicked(event: CellClickedEvent<GridRow>): void {
  const rowIndex = (event.data?.n ?? 0) - 1;
  emit('select', cellAt(rowIndex, event.column.getColId()));
}

async function copy(text: string, what: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    emit('copied', what);
  } catch {
    emit('copied', '');
  }
}

/** Cmd/Ctrl+C copies the focused cell; with Shift it copies the whole row as tab separated text. */
function onCellKeyDown(event: CellKeyDownEvent<GridRow> | FullWidthCellKeyDownEvent<GridRow>): void {
  const key = event.event;
  if (!(key instanceof KeyboardEvent) || key.key.toLowerCase() !== 'c' || !(key.metaKey || key.ctrlKey)) return;
  if (window.getSelection()?.toString()) return;
  const row = event.data;
  if (!row) return;
  if (key.shiftKey) {
    void copy(toTsv([row.cells]), 'row');
    return;
  }
  if (!('column' in event)) return;
  const cell = cellAt(row.n - 1, event.column.getColId());
  if (!cell) return;
  // A placeholder for a value too large to show is not data; it is never copied.
  if (isPlaceholder(cell.value)) emit('copied', 'oversized');
  else void copy(exportText(cell.value) ?? '', 'cell');
}
</script>

<template>
  <div
    class="grid"
    role="region"
    :aria-label="label"
  >
    <AgGridVue
      class="grid-inner"
      :theme="theme"
      :row-data="rowData"
      :column-defs="columnDefs"
      :grid-options="gridOptions"
    />
  </div>
</template>

<style scoped>
.grid {
  height: 100%;
  min-height: 0;
}

.grid-inner {
  height: 100%;
}

.grid :deep(.v-placeholder) {
  color: var(--text-subtle);
  font-family: var(--font-sans);
  font-style: italic;
}

.grid :deep(.v-null) {
  color: var(--text-subtle);
  font-style: italic;
}

.grid :deep(.v-num) {
  text-align: right;
  font-variant-numeric: tabular-nums;
}

.grid :deep(.v-bytes),
.grid :deep(.v-json) {
  color: var(--text-muted);
}

.grid :deep(.row-number) {
  color: var(--text-subtle);
  text-align: right;
  font-variant-numeric: tabular-nums;
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
</style>
