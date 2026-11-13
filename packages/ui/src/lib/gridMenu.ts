import type { MenuItem } from './menu';

/** What the menu needs to know about the cell a right click landed on. */
export interface MenuCell {
  /** The row exists (a right click always has one; the toolbar may have none). */
  row: { kind: 'row' | 'insert'; deleted: boolean } | null;
  /** The column of the cell, or null for a click outside the data columns. */
  column: { nullable: boolean } | null;
  /** The cell has an edit that is not saved yet. */
  pending: boolean;
}

/**
 * The entries of a data grid cell's context menu. A table that cannot be
 * edited (no key, read only) only offers copying.
 */
export function cellMenuItems(cell: MenuCell, editable: boolean): MenuItem[] {
  const { row, column } = cell;
  const copyValue: MenuItem = { id: 'copy-value', label: 'Copy value', disabled: !row || !column };
  const copyRow: MenuItem = { id: 'copy-row', label: 'Copy row', disabled: !row };
  if (!editable) return [copyValue, copyRow];
  const gone = !!row?.deleted;
  return [
    { id: 'edit', label: 'Edit value…', disabled: !row || !column || gone },
    { id: 'null', label: 'Set NULL', disabled: !row || !column || !column.nullable || gone },
    { id: 'default', label: 'Set DEFAULT', disabled: !row || !column || gone },
    { id: 'revert', label: 'Revert cell', disabled: !cell.pending },
    { ...copyValue, separatorBefore: true },
    copyRow,
    { id: 'add', label: 'Add row', separatorBefore: true },
    {
      id: 'delete',
      label: gone ? 'Undo delete' : row?.kind === 'insert' ? 'Remove new row' : 'Delete row',
      danger: !gone,
      disabled: !row,
    },
  ];
}
