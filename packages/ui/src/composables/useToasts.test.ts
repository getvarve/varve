import { useToasts } from './useToasts';

describe('toast actions', () => {
  it('runs an action once and closes its toast', () => {
    const toasts = useToasts();
    const run = vi.fn();
    const id = toasts.push('success', 'Folder Scratch deleted', '', 6000, { label: 'Undo', run });
    expect(toasts.toasts.find((t) => t.id === id)?.action?.label).toBe('Undo');
    toasts.act(id);
    toasts.act(id);
    expect(run).toHaveBeenCalledTimes(1);
    expect(toasts.toasts.some((t) => t.id === id)).toBe(false);
  });

  it('cannot act once the toast has gone', () => {
    vi.useFakeTimers();
    try {
      const toasts = useToasts();
      const run = vi.fn();
      const id = toasts.push('success', 'Folder Scratch deleted', '', 6000, { label: 'Undo', run });
      vi.advanceTimersByTime(6000);
      toasts.act(id);
      expect(run).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it('shows toasts without an action as before', () => {
    const toasts = useToasts();
    const id = toasts.push('info', 'Plain');
    expect(toasts.toasts.find((t) => t.id === id)?.action).toBeNull();
    toasts.dismiss(id);
  });

  it('keeps a toast with an action when newer toasts need its room', () => {
    const toasts = useToasts();
    for (const t of [...toasts.toasts]) toasts.dismiss(t.id);
    const undo = toasts.push('success', 'Folder Scratch deleted', '', 0, { label: 'Undo', run: () => undefined });
    const plain = [1, 2, 3, 4].map((n) => toasts.push('info', `Plain ${n}`, '', 0));
    expect(toasts.toasts).toHaveLength(4);
    expect(toasts.toasts.some((t) => t.id === undo)).toBe(true);
    // The oldest plain toast went instead.
    expect(toasts.toasts.some((t) => t.id === plain[0])).toBe(false);
    for (const t of [...toasts.toasts]) toasts.dismiss(t.id);
  });

  it('still shows at most four when every toast has an action', () => {
    const toasts = useToasts();
    for (const t of [...toasts.toasts]) toasts.dismiss(t.id);
    const ids = [1, 2, 3, 4, 5].map((n) => toasts.push('success', `Deleted ${n}`, '', 0, { label: 'Undo', run: () => undefined }));
    expect(toasts.toasts.map((t) => t.id)).toEqual(ids.slice(1));
    for (const t of [...toasts.toasts]) toasts.dismiss(t.id);
  });
});
