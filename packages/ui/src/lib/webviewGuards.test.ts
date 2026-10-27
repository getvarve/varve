import { describe, expect, it, vi } from 'vitest';
import { installWebviewGuards, isReloadKey, nativeMenuAllowed, reloadKeyAction } from './webviewGuards';

function key(init: KeyboardEventInit): KeyboardEvent {
  return new KeyboardEvent('keydown', { cancelable: true, ...init });
}

describe('webview guards', () => {
  it('keeps the native menu only in text fields and editors', () => {
    document.body.innerHTML = `
      <input id="text"><input id="box" type="checkbox"><textarea id="area"></textarea>
      <div class="cm-editor"><span id="code">x</span></div><button id="btn">b</button><div id="plain">p</div>`;
    const el = (id: string) => document.getElementById(id);
    expect(['text', 'area', 'code'].map((id) => nativeMenuAllowed(el(id)))).toEqual([true, true, true]);
    expect(['box', 'btn', 'plain'].map((id) => nativeMenuAllowed(el(id)))).toEqual([false, false, false]);
  });

  it('knows the reload keys and what they do', () => {
    expect(isReloadKey({ key: 'r', ctrlKey: true, metaKey: false, altKey: false })).toBe(true);
    expect(isReloadKey({ key: 'R', ctrlKey: false, metaKey: true, altKey: false })).toBe(true);
    expect(isReloadKey({ key: 'F5', ctrlKey: false, metaKey: false, altKey: false })).toBe(true);
    expect(isReloadKey({ key: 'r', ctrlKey: false, metaKey: false, altKey: false })).toBe(false);
    expect(reloadKeyAction(false, 0)).toBe('block');
    expect(reloadKeyAction(true, 0)).toBe('allow');
    expect(reloadKeyAction(true, 2)).toBe('confirm');
  });

  it('suppresses the context menu and the reload keys in release builds', () => {
    const reload = vi.fn();
    const remove = installWebviewGuards({ debugBuild: () => false, openTransactions: async () => [], confirm: async () => true, reload });
    document.body.innerHTML = '<div id="plain">p</div><input id="text">';
    const menu = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    document.getElementById('plain')?.dispatchEvent(menu);
    expect(menu.defaultPrevented).toBe(true);
    const inInput = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    document.getElementById('text')?.dispatchEvent(inInput);
    expect(inInput.defaultPrevented).toBe(false);
    const k = key({ key: 'r', metaKey: true });
    window.dispatchEvent(k);
    expect(k.defaultPrevented).toBe(true);
    expect(reload).not.toHaveBeenCalled();
    remove();
  });

  it('asks before a reload in debug builds when a transaction is open', async () => {
    const reload = vi.fn();
    const confirm = vi.fn(async () => false);
    const remove = installWebviewGuards({ debugBuild: () => true, openTransactions: async () => ['c1'], confirm, reload });
    window.dispatchEvent(key({ key: 'r', ctrlKey: true }));
    await vi.waitFor(() => expect(confirm).toHaveBeenCalledWith(['c1']));
    expect(reload).not.toHaveBeenCalled();
    remove();
  });
});
