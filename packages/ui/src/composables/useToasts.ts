import { reactive, readonly } from 'vue';
import { presentError, toCmdError } from '../api/errors';

export type ToastTone = 'success' | 'info' | 'warning' | 'danger';

/** A button on a toast (for example Undo): it runs `run` and closes the toast. */
export interface ToastAction {
  label: string;
  run: () => void;
}

export interface Toast {
  id: number;
  tone: ToastTone;
  title: string;
  message: string;
  action: ToastAction | null;
}

const state = reactive<{ toasts: Toast[] }>({ toasts: [] });
let nextId = 1;

function dismiss(id: number): void {
  const at = state.toasts.findIndex((t) => t.id === id);
  if (at >= 0) state.toasts.splice(at, 1);
}

/** Shows a toast for `timeoutMs` (0: until dismissed), with an optional action button. */
function push(tone: ToastTone, title: string, message = '', timeoutMs = 6000, action: ToastAction | null = null): number {
  const id = nextId;
  nextId += 1;
  state.toasts.push({ id, tone, title, message, action });
  if (state.toasts.length > 4) state.toasts.shift();
  if (timeoutMs > 0) setTimeout(() => dismiss(id), timeoutMs);
  return id;
}

/** Runs a toast's action once and closes the toast; nothing happens when it has gone already. */
function act(id: number): void {
  const toast = state.toasts.find((t) => t.id === id);
  if (!toast?.action) return;
  const { run } = toast.action;
  dismiss(id);
  run();
}

/** Shows any failure from a backend call as an error toast. */
function pushError(error: unknown, title?: string): number {
  const presented = presentError(toCmdError(error));
  const extra = [presented.code, presented.detail].filter(Boolean).join(' · ');
  return push('danger', title ?? presented.title, extra ? `${presented.message} (${extra})` : presented.message, 10_000);
}

/** App-wide toasts, kept in module state so any component can show one. */
export function useToasts() {
  return { toasts: readonly(state).toasts, push, pushError, dismiss, act };
}
