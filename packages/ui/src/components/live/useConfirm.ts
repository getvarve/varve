import { shallowRef } from 'vue';
import type { Ref } from 'vue';

/** What `LiveConfirm` shows. */
export interface ConfirmRequest {
  title: string;
  message: string;
  confirmLabel: string;
  statement?: string;
  details?: readonly string[];
  typeToConfirm?: string;
  danger?: boolean;
}

/**
 * One confirmation dialog for a panel: `ask` opens it and resolves with the
 * answer; the panel renders `LiveConfirm` while `pending` is set and calls
 * `answer` from its `done` event. Asking again answers an open one with no.
 */
export function useConfirm(): {
  pending: Readonly<Ref<ConfirmRequest | null>>;
  ask: (request: ConfirmRequest) => Promise<boolean>;
  answer: (yes: boolean) => void;
} {
  const pending = shallowRef<ConfirmRequest | null>(null);
  let resolve: ((yes: boolean) => void) | null = null;
  const answer = (yes: boolean): void => {
    const done = resolve;
    resolve = null;
    pending.value = null;
    done?.(yes);
  };
  const ask = (request: ConfirmRequest): Promise<boolean> => {
    answer(false);
    pending.value = request;
    return new Promise((r) => {
      resolve = r;
    });
  };
  return { pending, ask, answer };
}
