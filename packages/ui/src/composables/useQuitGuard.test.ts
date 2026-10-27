import { installQuitGuard, QUIT_REQUESTED, registerUnsavedCheck, requestQuit } from './useQuitGuard';
import { useToasts } from './useToasts';

const order: string[] = [];
const invoke = vi.fn(async (command: string) => {
  order.push(command);
});
let quitHandler: (() => void) | null = null;
const listen = vi.fn(async (event: string, handler: () => void) => {
  order.push(`listen:${event}`);
  quitHandler = handler;
  return () => order.push('unlisten');
});
const onCloseRequested = vi.fn(async () => {
  order.push('onCloseRequested');
  return () => order.push('offClose');
});

vi.mock('@tauri-apps/api/core', async (original) => ({
  ...(await original<object>()),
  invoke: (command: string) => invoke(command),
  isTauri: () => true,
}));
vi.mock('@tauri-apps/api/event', () => ({ listen: (event: string, handler: () => void) => listen(event, handler) }));
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: () => ({ onCloseRequested }) }));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

describe('installQuitGuard', () => {
  beforeEach(() => {
    order.length = 0;
    invoke.mockClear();
  });

  it('registers every listener before quit_ready, and calls quit_ready again on each mount', async () => {
    const remove = await installQuitGuard();
    expect(order).toEqual([`listen:${QUIT_REQUESTED}`, 'onCloseRequested', 'quit_ready']);
    remove();
    // A reload remounts the shell: the backend's ready flag was reset, so it is set again.
    const again = await installQuitGuard();
    expect(order.filter((c) => c === 'quit_ready')).toHaveLength(2);
    again();
  });

  it('acknowledges a repeated request that joins the open question', async () => {
    const remove = await installQuitGuard();
    order.length = 0;
    let answer: (ok: boolean) => void = () => undefined;
    const unregister = registerUnsavedCheck(
      () =>
        new Promise<boolean>((resolve) => {
          order.push('dialog');
          answer = resolve;
        }),
    );
    quitHandler?.();
    for (let i = 0; i < 3; i += 1) await tick();
    quitHandler?.();
    for (let i = 0; i < 3; i += 1) await tick();
    expect(order).toEqual(['quit_acknowledged', 'dialog', 'quit_acknowledged']);
    answer(false);
    for (let i = 0; i < 3; i += 1) await tick();
    expect(order.at(-1)).toBe('cancel_quit');
    unregister();
    remove();
  });

  it('acknowledges the quit request before any dialog opens', async () => {
    const remove = await installQuitGuard();
    order.length = 0;
    const unregister = registerUnsavedCheck(async () => {
      order.push('dialog');
      return false;
    });
    quitHandler?.();
    for (let i = 0; i < 5; i += 1) await tick();
    expect(order).toEqual(['quit_acknowledged', 'dialog', 'cancel_quit']);
    unregister();
    remove();
  });

  it('runs the same flow for Ctrl+Q outside macOS (jsdom reports no Mac platform)', async () => {
    const remove = await installQuitGuard();
    order.length = 0;
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Q', code: 'KeyQ', ctrlKey: true, shiftKey: true }));
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'q', code: 'KeyQ', ctrlKey: true, altKey: true }));
    for (let i = 0; i < 5; i += 1) await tick();
    // Another letter on the Q key's position does not matter: on an AZERTY layout it types "a".
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'q', code: 'KeyA', ctrlKey: true }));
    expect(order).toEqual([]);
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', code: 'KeyQ', ctrlKey: true }));
    for (let i = 0; i < 5; i += 1) await tick();
    expect(order).toEqual(['quit_acknowledged', 'confirm_quit']);
    remove();
  });
});

describe('installQuitGuard failure', () => {
  it('removes the listeners it attached when a later step fails', async () => {
    order.length = 0;
    onCloseRequested.mockRejectedValueOnce(new Error('no window'));
    await expect(installQuitGuard()).rejects.toThrow('no window');
    expect(order).toContain('unlisten');
    expect(order).not.toContain('quit_ready');
    // The Ctrl+Q handler is gone too.
    order.length = 0;
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'q', code: 'KeyQ', ctrlKey: true }));
    await tick();
    expect(order).toEqual([]);
  });
});

describe('requestQuit', () => {
  beforeEach(() => {
    order.length = 0;
    invoke.mockClear();
  });

  it('quits through confirm_quit when nothing is unsaved', async () => {
    await expect(requestQuit()).resolves.toBe(true);
    expect(order).toEqual(['quit_acknowledged', 'confirm_quit']);
  });

  it('keeps the app open, shows the error and resolves false when confirm_quit fails', async () => {
    invoke.mockImplementation(async (command: string) => {
      order.push(command);
      if (command === 'confirm_quit') throw new Error('command confirm_quit not found');
    });
    await expect(requestQuit()).resolves.toBe(false);
    expect(order.at(-1)).toBe('cancel_quit');
    const toast = useToasts().toasts.at(-1);
    expect(toast?.title).toBe('Could not quit');
    expect(toast?.message).toContain('confirm_quit');
    invoke.mockImplementation(async (command: string) => {
      order.push(command);
    });
  });

  it('stays and reports it when an unsaved check fails', async () => {
    const unregister = registerUnsavedCheck(async () => {
      throw new Error('the dialog could not open');
    });
    await expect(requestQuit()).resolves.toBe(false);
    expect(order).toEqual(['quit_acknowledged', 'cancel_quit']);
    expect(useToasts().toasts.at(-1)?.message).toContain('the dialog could not open');
    unregister();
  });
});
