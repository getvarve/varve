import { reactive, readonly } from 'vue';
import { presentError, toCmdError } from '../api/errors';

export type ToastTone = 'success' | 'info' | 'warning' | 'danger';

export interface Toast {
  id: number;
  tone: ToastTone;
  title: string;
  message: string;
}

const state = reactive<{ toasts: Toast[] }>({ toasts: [] });
let nextId = 1;

function dismiss(id: number): void {
  const at = state.toasts.findIndex((t) => t.id === id);
  if (at >= 0) state.toasts.splice(at, 1);
}

function push(tone: ToastTone, title: string, message = '', timeoutMs = 6000): number {
  const id = nextId;
  nextId += 1;
  state.toasts.push({ id, tone, title, message });
  if (state.toasts.length > 4) state.toasts.shift();
  if (timeoutMs > 0) setTimeout(() => dismiss(id), timeoutMs);
  return id;
}

/** Shows any failure from a backend call as an error toast. */
function pushError(error: unknown, title?: string): number {
  const presented = presentError(toCmdError(error));
  const extra = [presented.code, presented.detail].filter(Boolean).join(' · ');
  return push('danger', title ?? presented.title, extra ? `${presented.message} (${extra})` : presented.message, 10_000);
}

/** App-wide toasts, kept in module state so any component can show one. */
export function useToasts() {
  return { toasts: readonly(state).toasts, push, pushError, dismiss };
}
