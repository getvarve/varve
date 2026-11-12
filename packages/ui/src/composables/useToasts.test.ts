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
});
