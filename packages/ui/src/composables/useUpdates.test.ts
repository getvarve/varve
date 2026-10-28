import type { UpdateCheck, UpdateInfo, UpdateProgress } from '../api/updates';
import type * as UpdatesModule from './useUpdates';
import type * as ToastsModule from './useToasts';

/** Answers per command; a test replaces the ones it needs. */
let answers: Record<string, (args?: Record<string, unknown>) => unknown> = {};
const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
  const answer = answers[command];
  if (!answer) throw new Error(`unexpected command ${command}`);
  return answer(args);
});
const handlers = new Map<string, (event: { payload: unknown }) => void>();
const unlisten = vi.fn();
const listen = vi.fn(async (event: string, handler: (event: { payload: unknown }) => void) => {
  handlers.set(event, handler);
  return () => {
    unlisten(event);
    handlers.delete(event);
  };
});
const confirmAllUnsaved = vi.fn(async () => true);

vi.mock('@tauri-apps/api/core', async (original) => ({
  ...(await original<object>()),
  invoke: (command: string, args?: Record<string, unknown>) => invoke(command, args),
  isTauri: () => true,
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: (event: string, handler: (event: { payload: unknown }) => void) => listen(event, handler),
}));
vi.mock('./useQuitGuard', () => ({ confirmAllUnsaved: () => confirmAllUnsaved() }));

const AVAILABLE: UpdateCheck = { kind: 'available', version: '0.2.0', notes: '## Fixed\n- Things', date: null };

function info(overrides: Partial<UpdateInfo> = {}): UpdateInfo {
  return { mechanism: 'tauri', currentVersion: '0.1.5', autoCheck: true, relaunchPending: false, ...overrides };
}

/** A promise the test settles by hand. */
function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void; reject: (error: unknown) => void } {
  let resolve: (value: T) => void = () => undefined;
  let reject: (error: unknown) => void = () => undefined;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const flush = async (): Promise<void> => {
  for (let i = 0; i < 10; i += 1) await Promise.resolve();
};

function calls(command: string): number {
  return invoke.mock.calls.filter(([c]) => c === command).length;
}

/** A fresh module per test: the update state is module state, like the toasts. */
async function load(updateInfo: UpdateInfo = info()): Promise<{
  updates: ReturnType<typeof UpdatesModule.useUpdates>;
  mod: typeof UpdatesModule;
  toasts: ReturnType<typeof ToastsModule.useToasts>;
  remove: () => void;
}> {
  vi.resetModules();
  answers.update_info = () => updateInfo;
  answers.update_listening = () => undefined;
  const mod = await import('./useUpdates');
  const { useToasts } = await import('./useToasts');
  const remove = await mod.installUpdates();
  return { updates: mod.useUpdates(), mod, toasts: useToasts(), remove };
}

beforeEach(() => {
  answers = {};
  invoke.mockClear();
  listen.mockClear();
  unlisten.mockClear();
  handlers.clear();
  confirmAllUnsaved.mockReset();
  confirmAllUnsaved.mockResolvedValue(true);
  localStorage.clear();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe('the update flow (tauri)', () => {
  it('goes from checking to available, downloading with progress, ready, and installs', async () => {
    const { updates, remove } = await load(info({ autoCheck: false }));
    const check = deferred<UpdateCheck>();
    answers.update_check = () => check.promise;
    const done = updates.checkNow();
    expect(updates.state.dialogOpen).toBe(true);
    expect(updates.state.status.kind).toBe('checking');
    check.resolve(AVAILABLE);
    await done;
    expect(updates.state.status).toEqual({ kind: 'available', update: { version: '0.2.0', notes: '## Fixed\n- Things', date: null } });
    expect(updates.notice.value).toBe('Update available');

    const download = deferred<undefined>();
    answers.update_download = () => download.promise;
    const downloading = updates.download();
    await flush();
    expect(updates.state.status.kind).toBe('downloading');
    const progress: UpdateProgress = { downloaded: 1024 * 1024, total: 4 * 1024 * 1024 };
    handlers.get('varve://update-progress')?.({ payload: progress });
    expect(updates.state.status).toMatchObject({ kind: 'downloading', progress });
    download.resolve(undefined);
    await downloading;
    expect(updates.state.status.kind).toBe('ready');
    expect(unlisten).toHaveBeenCalledWith('varve://update-progress');

    answers.update_install = () => undefined;
    await updates.installAndRelaunch();
    expect(confirmAllUnsaved).toHaveBeenCalledTimes(1);
    expect(calls('update_install')).toBe(1);
    remove();
  });

  it('shows the newest version on a manual check, and closing returns to idle', async () => {
    const { updates, remove } = await load(info({ autoCheck: false }));
    answers.update_check = () => ({ kind: 'upToDate' });
    await updates.checkNow();
    expect(updates.state.status.kind).toBe('upToDate');
    updates.closeDialog();
    expect(updates.state.dialogOpen).toBe(false);
    expect(updates.state.status.kind).toBe('idle');
    remove();
  });

  it('shows the error of a manual check', async () => {
    const { updates, remove } = await load(info({ autoCheck: false }));
    answers.update_check = () => {
      throw new Error('network is down');
    };
    await updates.checkNow();
    expect(updates.state.status.kind).toBe('error');
    remove();
  });

  it('keeps the update after a failed download, and tells by toast when the dialog is closed', async () => {
    const { updates, toasts, remove } = await load(info({ autoCheck: false }));
    answers.update_check = () => AVAILABLE;
    await updates.checkNow();
    updates.closeDialog();
    answers.update_download = () => {
      throw new Error('disk full');
    };
    await updates.download();
    expect(updates.state.status).toMatchObject({ kind: 'error', update: { version: '0.2.0' } });
    expect(toasts.toasts.at(-1)?.title).toBe('Could not download the update');
    updates.closeDialog();
    expect(updates.state.status.kind).toBe('available');
    remove();
  });

  it('ignores an answer that comes after the dialog was closed while checking, unless an update was found', async () => {
    const { updates, remove } = await load(info({ autoCheck: false }));
    const check = deferred<UpdateCheck>();
    answers.update_check = () => check.promise;
    const done = updates.checkNow();
    updates.closeDialog();
    check.resolve(AVAILABLE);
    await done;
    expect(updates.state.dialogOpen).toBe(false);
    expect(updates.state.status.kind).toBe('available');
    remove();
  });
});

describe('unsaved work before installing', () => {
  async function readyUpdates(): Promise<Awaited<ReturnType<typeof load>>> {
    const loaded = await load(info({ autoCheck: false }));
    answers.update_check = () => AVAILABLE;
    answers.update_download = () => undefined;
    answers.update_install = () => undefined;
    await loaded.updates.checkNow();
    await loaded.updates.download();
    expect(loaded.updates.state.status.kind).toBe('ready');
    return loaded;
  }

  it('never installs when the unsaved check says no', async () => {
    const { updates, remove } = await readyUpdates();
    confirmAllUnsaved.mockResolvedValue(false);
    await updates.installAndRelaunch();
    expect(calls('update_install')).toBe(0);
    expect(updates.state.status.kind).toBe('ready');
    expect(updates.state.installing).toBe(false);
    remove();
  });

  it('never installs when the unsaved check fails, and shows the error', async () => {
    const { updates, toasts, remove } = await readyUpdates();
    confirmAllUnsaved.mockRejectedValue(new Error('save failed'));
    await updates.installAndRelaunch();
    expect(calls('update_install')).toBe(0);
    expect(toasts.toasts.at(-1)?.title).toBe('Could not check for unsaved work');
    remove();
  });
});

describe('Skip This Version', () => {
  it('is stored, keeps automatic checks quiet, and a manual check still shows it', async () => {
    vi.useFakeTimers();
    const first = await load();
    answers.update_check = () => AVAILABLE;
    await first.updates.checkNow();
    first.updates.skipVersion();
    expect(first.updates.state.dialogOpen).toBe(false);
    expect(first.updates.notice.value).toBeNull();
    expect(localStorage.getItem('varve.update.skippedVersion')).toBe('"0.2.0"');
    first.remove();

    // A new app start reads the skipped version back.
    const { updates, mod, remove } = await load();
    await vi.advanceTimersByTimeAsync(mod.FIRST_CHECK_DELAY_MS);
    expect(calls('update_check')).toBe(2);
    expect(updates.state.status.kind).toBe('idle');
    expect(updates.notice.value).toBeNull();

    await updates.checkNow();
    expect(updates.state.status.kind).toBe('available');
    // Remind Me Later keeps it, but the status bar stays quiet about a skipped version.
    updates.closeDialog();
    expect(updates.notice.value).toBeNull();
    remove();
  });

  it('notifies about a newer version than the skipped one', async () => {
    vi.useFakeTimers();
    localStorage.setItem('varve.update.skippedVersion', '"0.1.9"');
    const { updates, mod, remove } = await load();
    answers.update_check = () => AVAILABLE;
    await vi.advanceTimersByTimeAsync(mod.FIRST_CHECK_DELAY_MS);
    expect(updates.notice.value).toBe('Update available');
    expect(updates.state.dialogOpen).toBe(false);
    remove();
  });

  it('works without storage', async () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    const { updates, remove } = await load(info({ autoCheck: false }));
    answers.update_check = () => AVAILABLE;
    await updates.checkNow();
    expect(() => updates.skipVersion()).not.toThrow();
    expect(updates.state.skipped).toBe('0.2.0');
    remove();
  });
});

describe('automatic checks', () => {
  it('run 10 seconds after start, then every 6 hours', async () => {
    vi.useFakeTimers();
    const { mod, remove } = await load();
    answers.update_check = () => ({ kind: 'upToDate' });
    await vi.advanceTimersByTimeAsync(mod.FIRST_CHECK_DELAY_MS - 1);
    expect(calls('update_check')).toBe(0);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls('update_check')).toBe(1);
    await vi.advanceTimersByTimeAsync(mod.CHECK_INTERVAL_MS - 1);
    expect(calls('update_check')).toBe(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls('update_check')).toBe(2);
    remove();
    // The cleanup stops them.
    await vi.advanceTimersByTimeAsync(mod.CHECK_INTERVAL_MS * 2);
    expect(calls('update_check')).toBe(2);
  });

  it('are silent when nothing is found and when the check fails', async () => {
    vi.useFakeTimers();
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const { updates, toasts, mod, remove } = await load();
    const toastCount = toasts.toasts.length;
    answers.update_check = () => ({ kind: 'upToDate' });
    await vi.advanceTimersByTimeAsync(mod.FIRST_CHECK_DELAY_MS);
    expect(updates.state.status.kind).toBe('idle');
    answers.update_check = () => {
      throw new Error('offline');
    };
    await vi.advanceTimersByTimeAsync(mod.CHECK_INTERVAL_MS);
    expect(calls('update_check')).toBe(2);
    expect(updates.state.status.kind).toBe('idle');
    expect(updates.state.dialogOpen).toBe(false);
    expect(toasts.toasts.length).toBe(toastCount);
    expect(warn).toHaveBeenCalled();
    remove();
  });

  it('do not run when switched off, and start again when switched on', async () => {
    vi.useFakeTimers();
    const { updates, mod, remove } = await load(info({ autoCheck: false }));
    answers.update_check = () => ({ kind: 'upToDate' });
    answers.set_update_auto_check = () => undefined;
    await vi.advanceTimersByTimeAsync(mod.CHECK_INTERVAL_MS);
    expect(calls('update_check')).toBe(0);
    await updates.setAutoCheck(true);
    expect(invoke).toHaveBeenCalledWith('set_update_auto_check', { autoCheck: true });
    await vi.advanceTimersByTimeAsync(mod.FIRST_CHECK_DELAY_MS);
    expect(calls('update_check')).toBe(1);
    await updates.setAutoCheck(false);
    await vi.advanceTimersByTimeAsync(mod.CHECK_INTERVAL_MS * 2);
    expect(calls('update_check')).toBe(1);
    remove();
  });

  it('flip the switch back when the setting cannot be saved', async () => {
    const { updates, toasts, remove } = await load(info({ autoCheck: true }));
    answers.set_update_auto_check = () => {
      throw new Error('read-only');
    };
    await updates.setAutoCheck(false);
    expect(updates.state.info?.autoCheck).toBe(true);
    expect(toasts.toasts.at(-1)?.title).toBe('Could not save the update setting');
    remove();
  });

  it('leave a download alone', async () => {
    vi.useFakeTimers();
    const { updates, mod, remove } = await load();
    answers.update_check = () => AVAILABLE;
    await updates.checkNow();
    answers.update_download = () => new Promise(() => undefined);
    void updates.download();
    await flush();
    const before = calls('update_check');
    await vi.advanceTimersByTimeAsync(mod.FIRST_CHECK_DELAY_MS);
    expect(calls('update_check')).toBe(before);
    expect(updates.state.status.kind).toBe('downloading');
    remove();
  });

  it('are never scheduled on sparkle or none', async () => {
    vi.useFakeTimers();
    for (const mechanism of ['sparkle', 'none'] as const) {
      const { mod, remove } = await load(info({ mechanism }));
      await vi.advanceTimersByTimeAsync(mod.CHECK_INTERVAL_MS);
      remove();
    }
    expect(calls('update_check')).toBe(0);
  });
});

describe('Sparkle', () => {
  it('only asks Sparkle to check, and shows nothing itself', async () => {
    const { updates, remove } = await load(info({ mechanism: 'sparkle' }));
    answers.update_check = () => ({ kind: 'sparkle' });
    await updates.checkNow();
    expect(calls('update_check')).toBe(1);
    expect(updates.state.dialogOpen).toBe(false);
    expect(updates.state.status.kind).toBe('idle');
    remove();
  });

  it('answers the relaunch request after the unsaved check', async () => {
    const { mod, remove } = await load(info({ mechanism: 'sparkle' }));
    answers.update_relaunch = () => undefined;
    expect(handlers.has('varve://update-relaunch-requested')).toBe(true);

    confirmAllUnsaved.mockResolvedValueOnce(true);
    await mod.answerRelaunchRequest();
    expect(invoke).toHaveBeenLastCalledWith('update_relaunch', { proceed: true });

    confirmAllUnsaved.mockResolvedValueOnce(false);
    handlers.get('varve://update-relaunch-requested')?.({ payload: null });
    await flush();
    expect(invoke).toHaveBeenLastCalledWith('update_relaunch', { proceed: false });
    remove();
    expect(handlers.has('varve://update-relaunch-requested')).toBe(false);
  });

  it('tells the backend it listens only after the listener is in place', async () => {
    const { remove } = await load(info({ mechanism: 'sparkle' }));
    expect(calls('update_listening')).toBe(1);
    const listened = listen.mock.invocationCallOrder[0] ?? Infinity;
    const acknowledged = invoke.mock.invocationCallOrder[invoke.mock.calls.findIndex(([c]) => c === 'update_listening')] ?? -1;
    expect(listened).toBeLessThan(acknowledged);
    remove();
  });

  it('offers a declined relaunch again from the status bar, with a new unsaved check', async () => {
    const { mod, updates, remove } = await load(info({ mechanism: 'sparkle' }));
    answers.update_relaunch = () => undefined;
    expect(updates.notice.value).toBeNull();

    confirmAllUnsaved.mockResolvedValueOnce(false);
    await mod.answerRelaunchRequest();
    expect(updates.notice.value).toBe('Restart to update');

    confirmAllUnsaved.mockResolvedValueOnce(true);
    updates.openNotice();
    await flush();
    expect(confirmAllUnsaved).toHaveBeenCalledTimes(2);
    expect(invoke).toHaveBeenLastCalledWith('update_relaunch', { proceed: true });
    expect(updates.notice.value).toBeNull();
    remove();
  });

  it('drops the restart button when Sparkle gave the relaunch up', async () => {
    const { mod, updates, remove } = await load(info({ mechanism: 'sparkle', relaunchPending: true }));
    answers.update_relaunch = () => {
      throw { kind: 'internal', message: 'No update is waiting to relaunch' };
    };
    answers.update_info = () => info({ mechanism: 'sparkle', relaunchPending: false });
    expect(updates.notice.value).toBe('Restart to update');
    await mod.answerRelaunchRequest();
    expect(updates.notice.value).toBeNull();
    remove();
  });

  it('shows a relaunch held from before a reload', async () => {
    const { updates, remove } = await load(info({ mechanism: 'sparkle', relaunchPending: true }));
    expect(updates.notice.value).toBe('Restart to update');
    remove();
  });

  it('answers false when the unsaved check fails, and shows the error', async () => {
    const { mod, toasts, remove } = await load(info({ mechanism: 'sparkle' }));
    answers.update_relaunch = () => undefined;
    confirmAllUnsaved.mockRejectedValueOnce(new Error('save failed'));
    await mod.answerRelaunchRequest();
    expect(invoke).toHaveBeenLastCalledWith('update_relaunch', { proceed: false });
    expect(toasts.toasts.at(-1)?.title).toBe('Could not check for unsaved work');
    remove();
  });

  it('answers a repeated request once, after the open question', async () => {
    const { mod, remove } = await load(info({ mechanism: 'sparkle' }));
    answers.update_relaunch = () => undefined;
    const question = deferred<boolean>();
    confirmAllUnsaved.mockReturnValueOnce(question.promise);
    const first = mod.answerRelaunchRequest();
    const second = mod.answerRelaunchRequest();
    question.resolve(false);
    await Promise.all([first, second]);
    expect(confirmAllUnsaved).toHaveBeenCalledTimes(1);
    expect(calls('update_relaunch')).toBe(1);
    remove();
  });
});

describe('formatProgress', () => {
  it('shows megabytes, without a total when the size is unknown', async () => {
    const { formatProgress, progressPercent } = await import('./useUpdates');
    expect(formatProgress({ downloaded: 1.5 * 1024 * 1024, total: 40 * 1024 * 1024 })).toBe('1.5 of 40.0 MB');
    expect(formatProgress({ downloaded: 0, total: null })).toBe('0.0 MB');
    expect(progressPercent({ downloaded: 1, total: 4 })).toBe(25);
    expect(progressPercent({ downloaded: 1, total: null })).toBeNull();
  });
});
