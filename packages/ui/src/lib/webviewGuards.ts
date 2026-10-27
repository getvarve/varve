/**
 * Keeps a reload of the window from being one click or key away.
 *
 * A reload closes every session of the page (the backend rolls back their
 * open transactions, see app/src-tauri/src/m2/reload.rs). In release builds
 * the webview's own context menu (which offers Reload) is suppressed except
 * in text fields and editors, and the reload keys do nothing. In debug
 * builds both stay, and the reload keys ask first when a transaction is open.
 */

/** Text inputs, text areas, editable content and CodeMirror editors keep the native menu (cut, copy, paste, spelling). */
export function nativeMenuAllowed(target: EventTarget | null): boolean {
  if (typeof Element === 'undefined' || !(target instanceof Element)) return false;
  if (target.closest('textarea, [contenteditable=""], [contenteditable="true"], .cm-editor, [data-native-menu]')) return true;
  const input = target.closest('input');
  if (!input) return false;
  return !['button', 'checkbox', 'color', 'file', 'hidden', 'image', 'radio', 'range', 'reset', 'submit'].includes(input.type);
}

/** F5, Ctrl+R and Cmd+R (also with Shift). */
export function isReloadKey(e: Pick<KeyboardEvent, 'key' | 'ctrlKey' | 'metaKey' | 'altKey'>): boolean {
  if (e.key === 'F5') return true;
  return (e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 'r';
}

/** What a reload key does: nothing in release builds; in debug builds ask when transactions are open. */
export function reloadKeyAction(debugBuild: boolean, openTransactions: number): 'block' | 'confirm' | 'allow' {
  if (!debugBuild) return 'block';
  return openTransactions > 0 ? 'confirm' : 'allow';
}

export interface WebviewGuardDeps {
  /** `null` until the app info is read: treated as a release build. */
  debugBuild: () => boolean | null;
  /** Names of the connections with an open transaction. */
  openTransactions: () => Promise<string[]>;
  /** Asks whether to reload anyway. */
  confirm: (names: string[]) => Promise<boolean>;
  reload: () => void;
}

/** Installs the guards on `target` (the window); returns a function that removes them. */
export function installWebviewGuards(deps: WebviewGuardDeps, target: Pick<Window, 'addEventListener' | 'removeEventListener'> = window): () => void {
  const onContextMenu = (e: Event): void => {
    if (deps.debugBuild() === true || nativeMenuAllowed(e.target)) return;
    e.preventDefault();
  };
  const onKey = (e: Event): void => {
    if (!(e instanceof KeyboardEvent) || !isReloadKey(e)) return;
    e.preventDefault();
    const debug = deps.debugBuild() === true;
    if (!debug) return;
    void deps
      .openTransactions()
      .catch(() => [] as string[])
      .then(async (names) => {
        const action = reloadKeyAction(debug, names.length);
        if (action === 'allow' || (action === 'confirm' && (await deps.confirm(names)))) deps.reload();
      });
  };
  target.addEventListener('contextmenu', onContextMenu);
  target.addEventListener('keydown', onKey, { capture: true });
  return () => {
    target.removeEventListener('contextmenu', onContextMenu);
    target.removeEventListener('keydown', onKey, { capture: true });
  };
}
