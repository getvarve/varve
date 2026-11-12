import { describe, expect, it } from 'vitest';
import { cellMenuItems } from './gridMenu';

const row = { kind: 'row' as const, deleted: false };
const column = { nullable: true };
const by = (items: ReturnType<typeof cellMenuItems>) => Object.fromEntries(items.map((i) => [i.id, i]));

describe('cellMenuItems', () => {
  it('offers only copying on a table that cannot be edited', () => {
    const items = cellMenuItems({ row, column, pending: false }, false);
    expect(items.map((i) => i.id)).toEqual(['copy-value', 'copy-row']);
    expect(items.every((i) => !i.disabled)).toBe(true);
  });

  it('offers every action on an editable cell', () => {
    const items = cellMenuItems({ row, column, pending: false }, true);
    expect(items.map((i) => i.id)).toEqual(['edit', 'null', 'default', 'revert', 'copy-value', 'copy-row', 'add', 'delete']);
    const m = by(items);
    expect(m.edit?.disabled).toBe(false);
    expect(m.null?.disabled).toBe(false);
    expect(m.revert?.disabled).toBe(true);
    expect(m.delete).toMatchObject({ label: 'Delete row', danger: true, disabled: false });
  });

  it('does not offer NULL for a column that cannot hold it, and offers revert for a pending edit', () => {
    const m = by(cellMenuItems({ row, column: { nullable: false }, pending: true }, true));
    expect(m.null?.disabled).toBe(true);
    expect(m.revert?.disabled).toBe(false);
  });

  it('names the delete entry after the row', () => {
    expect(by(cellMenuItems({ row: { kind: 'insert', deleted: false }, column, pending: false }, true)).delete?.label).toBe('Remove new row');
    const gone = by(cellMenuItems({ row: { kind: 'row', deleted: true }, column, pending: false }, true));
    expect(gone.delete).toMatchObject({ label: 'Undo delete', danger: false });
    // A row marked for deletion takes no edits until the mark is undone.
    expect(gone.edit?.disabled).toBe(true);
    expect(gone.null?.disabled).toBe(true);
    expect(gone.default?.disabled).toBe(true);
  });

  it('disables the cell actions without a column', () => {
    const m = by(cellMenuItems({ row, column: null, pending: false }, true));
    expect(m.edit?.disabled).toBe(true);
    expect(m['copy-value']?.disabled).toBe(true);
    expect(m['copy-row']?.disabled).toBe(false);
  });
});
