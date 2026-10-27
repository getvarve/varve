import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { createQuitFlow } from '../lib/quitFlow';
import { useQuerySave } from './useQuerySave';
import { useTabs } from './useTabs';
import { useToasts } from './useToasts';

/** Event the backend sends for every exit request (app/src-tauri/src/quit.rs). */
export const QUIT_REQUESTED = 'varve://quit-requested';

/**
 * Quits after the unsaved check: `confirm_quit` (app/src-tauri/src/quit.rs)
 * lets the backend's held exit through. If it fails, the app stays open and
 * the error is shown; the window is never closed behind the backend's back.
 */
async function quit(): Promise<void> {
  await invoke('confirm_quit');
}

/** Open forms with unsaved edits (the connection editor, Settings) register a check here. */
const formChecks = new Set<() => Promise<boolean>>();

/**
 * Registers a check for unsaved form edits; it resolves `true` when the form
 * may be thrown away (nothing changed, saved, or discarded). Returns the
 * function that removes it again.
 */
export function registerUnsavedCheck(check: () => Promise<boolean>): () => void {
  formChecks.add(check);
  return () => formChecks.delete(check);
}

/** Asks about dirty forms first, then about unsaved editor tabs. */
async function confirmAllUnsaved(): Promise<boolean> {
  for (const check of [...formChecks]) if (!(await check())) return false;
  return useQuerySave().confirmUnsavedTabs(useTabs().tabs.map((t) => t.id));
}

/** Every quit path (Cmd+Q in the macOS menu, Ctrl+Q elsewhere, window close) runs this one flow. */
export const requestQuit = createQuitFlow({
  // Before any dialog: the backend then waits for the user as long as needed.
  acknowledge: async () => {
    await invoke('quit_acknowledged');
  },
  confirmUnsaved: confirmAllUnsaved,
  quit,
  stay: async () => {
    await invoke('cancel_quit');
  },
  failed: (error) => useToasts().pushError(error, 'Could not quit'),
});

/** True on macOS, where Cmd+Q is the app menu's Quit item (handled by the backend). */
function isMac(): boolean {
  return /Mac/i.test(globalThis.navigator?.platform ?? '') || /Mac OS X/.test(globalThis.navigator?.userAgent ?? '');
}

/**
 * Windows and Linux have no app menu, so Ctrl+Q is handled here and runs
 * the same flow. On macOS, Cmd+Q comes from the menu as a quit request.
 */
function onQuitShortcut(event: KeyboardEvent): void {
  // Exactly Ctrl+Q: Ctrl+Shift+Q or Ctrl+Alt+Q must not quit.
  // By physical key (`code`), so it works on every keyboard layout.
  if (isMac() || !event.ctrlKey || event.shiftKey || event.altKey || event.metaKey || event.code !== 'KeyQ') return;
  event.preventDefault();
  void requestQuit();
}

/**
 * Listens for exit requests and runs `requestQuit` (which asks only once):
 *
 * - the backend's `varve://quit-requested` event: Cmd+Q and Quit Varve in
 *   the macOS menu, closing the main window;
 * - the window's own close request, as a second path for closing;
 * - Ctrl+Q on Windows and Linux, which have no app menu.
 *
 * Then it calls `quit_ready`, on every mount: a page load resets the
 * backend's ready flag. Outside Tauri it does nothing. Resolves to a cleanup
 * function.
 */
export async function installQuitGuard(): Promise<() => void> {
  if (!isTauri()) return () => undefined;
  const cleanups: (() => void)[] = [];
  const removeAll = (): void => {
    for (const cleanup of cleanups.splice(0)) cleanup();
  };
  try {
    globalThis.addEventListener('keydown', onQuitShortcut);
    cleanups.push(() => globalThis.removeEventListener('keydown', onQuitShortcut));
    cleanups.push(
      await listen(QUIT_REQUESTED, () => {
        void requestQuit();
      }),
    );
    cleanups.push(
      await getCurrentWindow().onCloseRequested((event) => {
        // The flow decides; the window never closes behind the user's back.
        event.preventDefault();
        void requestQuit();
      }),
    );
    // Tell the backend a handler listens; until now it quits without asking,
    // so the app can always be quit.
    await invoke('quit_ready');
  } catch (error: unknown) {
    // Never leave half a guard behind.
    removeAll();
    throw error;
  }
  return removeAll;
}
