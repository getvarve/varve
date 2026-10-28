import { isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { computed, reactive, readonly } from 'vue';
import { presentError, toCmdError } from '../api/errors';
import {
  answerUpdateRelaunch,
  checkForUpdate,
  downloadUpdate,
  fetchUpdateInfo,
  installUpdate,
  markUpdateListening,
  setUpdateAutoCheck,
  UPDATE_PROGRESS,
  UPDATE_RELAUNCH_REQUESTED,
} from '../api/updates';
import type { UpdateCheck, UpdateInfo, UpdateProgress } from '../api/updates';
import { readStored, writeStored } from '../lib/storage';
import { confirmAllUnsaved } from './useQuitGuard';
import { useToasts } from './useToasts';

/** The first automatic check runs this long after start, so it never slows the launch. */
export const FIRST_CHECK_DELAY_MS = 10_000;
/** Automatic checks repeat this often while the app runs. */
export const CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
/** The version the user chose to skip, in localStorage. */
export const SKIPPED_VERSION_KEY = 'varve.update.skippedVersion';

export interface AvailableUpdate {
  version: string;
  /** Release notes as Markdown text. */
  notes: string | null;
  date: string | null;
}

/** The update flow of the `tauri` mechanism; `sparkle` and `none` stay `idle`. */
export type UpdateStatus =
  | { kind: 'idle' }
  | { kind: 'checking' }
  | { kind: 'upToDate' }
  | { kind: 'available'; update: AvailableUpdate }
  | { kind: 'downloading'; update: AvailableUpdate; progress: UpdateProgress }
  | { kind: 'ready'; update: AvailableUpdate }
  /** `update` is the update a download or install failed for, so it can be offered again. */
  | { kind: 'error'; message: string; update: AvailableUpdate | null };

function isVersionOrNull(value: unknown): value is string | null {
  return value === null || typeof value === 'string';
}

const state = reactive<{
  /** Null until the backend answered, and outside Tauri. */
  info: UpdateInfo | null;
  status: UpdateStatus;
  dialogOpen: boolean;
  /** The unsaved check or the install runs. */
  installing: boolean;
  skipped: string | null;
}>({
  info: null,
  status: { kind: 'idle' },
  dialogOpen: false,
  installing: false,
  skipped: readStored<string | null>(SKIPPED_VERSION_KEY, null, isVersionOrNull),
});

/** The check in flight; a manual check joins an automatic one instead of starting a second. */
let running: Promise<UpdateCheck> | null = null;
/** Bumped when the dialog closes, so a manual check's late answer counts as an automatic one. */
let dialogGeneration = 0;
let timer: ReturnType<typeof setTimeout> | null = null;
/** The open answer to Sparkle's relaunch request; a repeated request joins it. */
let relaunch: Promise<void> | null = null;

/** The status now, read through a call: it changes across an `await`. */
function statusKind(): UpdateStatus['kind'] {
  return state.status.kind;
}

function messageOf(error: unknown): string {
  return presentError(toCmdError(error)).message;
}

function toUpdate(result: Extract<UpdateCheck, { kind: 'available' }>): AvailableUpdate {
  return { version: result.version, notes: result.notes, date: result.date };
}

function runCheck(): Promise<UpdateCheck> {
  if (!running) {
    const check = checkForUpdate();
    running = check;
    check
      .finally(() => {
        if (running === check) running = null;
      })
      .catch(() => undefined);
  }
  return running;
}

/** States an automatic check may replace: nothing is downloading or waiting to install. */
function quiet(): boolean {
  const kind = state.status.kind;
  return !state.dialogOpen && (kind === 'idle' || kind === 'available');
}

/** Applies a check nobody is watching: only an update that was not skipped is kept. */
function applyQuietly(result: UpdateCheck): void {
  state.status = result.kind === 'available' && result.version !== state.skipped ? { kind: 'available', update: toUpdate(result) } : { kind: 'idle' };
}

/** An automatic check: silent unless it finds a version that was not skipped; errors are only logged. */
async function automaticCheck(): Promise<void> {
  if (state.info?.mechanism !== 'tauri' || !state.info.autoCheck || !quiet()) return;
  let result: UpdateCheck;
  try {
    result = await runCheck();
  } catch (error: unknown) {
    console.warn('The automatic update check failed', error);
    return;
  }
  // A manual check joined, or the user started something meanwhile: that wins.
  if (quiet()) applyQuietly(result);
}

function stopSchedule(): void {
  if (timer !== null) clearTimeout(timer);
  timer = null;
}

function scheduleCheck(delayMs: number): void {
  stopSchedule();
  timer = setTimeout(() => {
    scheduleCheck(CHECK_INTERVAL_MS);
    void automaticCheck();
  }, delayMs);
}

/** Starts the automatic checks when they are on (tauri only), or stops them. */
function reschedule(): void {
  stopSchedule();
  if (state.info?.mechanism === 'tauri' && state.info.autoCheck) scheduleCheck(FIRST_CHECK_DELAY_MS);
}

/** Shows a failure in the dialog, or as a toast when the dialog is closed. */
function fail(error: unknown, update: AvailableUpdate | null, title: string): void {
  state.status = { kind: 'error', message: messageOf(error), update };
  if (!state.dialogOpen) useToasts().pushError(error, title);
}

/**
 * "Check for Updates…": on sparkle, Sparkle shows its own window; on tauri
 * the dialog opens in its checking state and always shows the result, also
 * for a skipped version or an error.
 */
async function checkNow(): Promise<void> {
  const mechanism = state.info?.mechanism ?? 'none';
  if (mechanism === 'none') return;
  if (mechanism === 'sparkle') {
    try {
      await checkForUpdate();
    } catch (error: unknown) {
      useToasts().pushError(error, 'Could not check for updates');
    }
    return;
  }
  state.dialogOpen = true;
  // A download or a downloaded update is shown as it is, not checked again.
  if (state.status.kind === 'downloading' || state.status.kind === 'ready') return;
  state.status = { kind: 'checking' };
  const generation = dialogGeneration;
  let result: UpdateCheck;
  try {
    result = await runCheck();
  } catch (error: unknown) {
    if (generation === dialogGeneration) state.status = { kind: 'error', message: messageOf(error), update: null };
    else console.warn('The update check failed', error);
    return;
  }
  if (generation !== dialogGeneration) {
    // The dialog was closed while checking: nobody waits for this answer.
    if (statusKind() === 'idle') applyQuietly(result);
    return;
  }
  if (result.kind === 'available') state.status = { kind: 'available', update: toUpdate(result) };
  else if (result.kind === 'upToDate') state.status = { kind: 'upToDate' };
  else closeDialog();
}

function openDialog(): void {
  state.dialogOpen = true;
}

/** Closes the dialog; a found update stays reachable from the status bar. */
function closeDialog(): void {
  state.dialogOpen = false;
  dialogGeneration += 1;
  const status = state.status;
  if (status.kind === 'checking' || status.kind === 'upToDate') state.status = { kind: 'idle' };
  else if (status.kind === 'error') state.status = status.update ? { kind: 'available', update: status.update } : { kind: 'idle' };
}

/** "Skip This Version": automatic checks stay quiet about it; a manual check still shows it. */
function skipVersion(): void {
  if (state.status.kind !== 'available') return;
  state.skipped = state.status.update.version;
  writeStored(SKIPPED_VERSION_KEY, state.skipped);
  state.status = { kind: 'idle' };
  closeDialog();
}

/** "Install Update": downloads the update; the dialog then offers to install and relaunch. */
async function download(): Promise<void> {
  if (state.status.kind !== 'available') return;
  const update = state.status.update;
  state.status = { kind: 'downloading', update, progress: { downloaded: 0, total: null } };
  let unlisten: (() => void) | null = null;
  try {
    unlisten = await listen<UpdateProgress>(UPDATE_PROGRESS, (event) => {
      if (state.status.kind === 'downloading') state.status.progress = event.payload;
    });
    await downloadUpdate();
    state.status = { kind: 'ready', update };
  } catch (error: unknown) {
    fail(error, update, 'Could not download the update');
  } finally {
    unlisten?.();
  }
}

/**
 * "Install and Relaunch": asks about unsaved work first and installs only
 * when that resolves `true`. The backend then restarts the app.
 */
async function installAndRelaunch(): Promise<void> {
  if (state.status.kind !== 'ready' || state.installing) return;
  const update = state.status.update;
  state.installing = true;
  try {
    let ok: boolean;
    try {
      ok = await confirmAllUnsaved();
    } catch (error: unknown) {
      useToasts().pushError(error, 'Could not check for unsaved work');
      return;
    }
    if (!ok) return;
    try {
      await installUpdate();
    } catch (error: unknown) {
      fail(error, update, 'Could not install the update');
    }
  } finally {
    state.installing = false;
  }
}

/** Stores the setting; on failure the switch flips back and the error is shown. */
async function setAutoCheck(autoCheck: boolean): Promise<void> {
  const info = state.info;
  if (!info) return;
  const before = info.autoCheck;
  info.autoCheck = autoCheck;
  try {
    await setUpdateAutoCheck(autoCheck);
  } catch (error: unknown) {
    info.autoCheck = before;
    useToasts().pushError(error, 'Could not save the update setting');
  }
  reschedule();
}

/**
 * Sparkle's "Install and Relaunch", and the status bar's "Restart to update":
 * answers `update_relaunch(true)` only when the unsaved check resolved
 * `true`. A cancelled or failing check answers `false`: the app stays open,
 * Sparkle keeps the relaunch held, and the status bar offers it again.
 */
export function answerRelaunchRequest(): Promise<void> {
  relaunch ??= (async () => {
    let proceed = false;
    try {
      proceed = await confirmAllUnsaved();
    } catch (error: unknown) {
      useToasts().pushError(error, 'Could not check for unsaved work');
    }
    try {
      await answerUpdateRelaunch(proceed);
      if (state.info) state.info.relaunchPending = !proceed;
    } catch (error: unknown) {
      useToasts().pushError(error, 'Could not answer the update');
      // Sparkle may have given the relaunch up (it aborted): show what is left.
      try {
        state.info = await fetchUpdateInfo();
      } catch {
        if (state.info) state.info.relaunchPending = false;
      }
    }
  })().finally(() => {
    relaunch = null;
  });
  return relaunch;
}

/**
 * Reads how this build updates and starts what it needs: the automatic
 * checks on tauri, the relaunch listener on sparkle. Outside Tauri it does
 * nothing. Resolves to a cleanup function.
 */
export async function installUpdates(): Promise<() => void> {
  if (!isTauri()) return () => undefined;
  const info = await fetchUpdateInfo();
  state.info = info;
  const cleanups: (() => void)[] = [];
  if (info.mechanism === 'sparkle') {
    cleanups.push(
      await listen(UPDATE_RELAUNCH_REQUESTED, () => {
        void answerRelaunchRequest();
      }),
    );
    // Only now may the backend hold a relaunch back for this page's answer.
    await markUpdateListening();
  } else if (info.mechanism === 'tauri') {
    reschedule();
    cleanups.push(stopSchedule);
  }
  return () => {
    for (const cleanup of cleanups.splice(0)) cleanup();
  };
}

/** "12.3 of 45.6 MB", or "12.3 MB" when the size is unknown. */
export function formatProgress(progress: UpdateProgress): string {
  const mb = (bytes: number): string => (bytes / (1024 * 1024)).toFixed(1);
  return progress.total === null ? `${mb(progress.downloaded)} MB` : `${mb(progress.downloaded)} of ${mb(progress.total)} MB`;
}

/** Whole percent done, or null when the size is unknown. */
export function progressPercent(progress: UpdateProgress): number | null {
  if (progress.total === null || progress.total <= 0) return null;
  return Math.min(100, Math.round((progress.downloaded / progress.total) * 100));
}

/** App-wide update state, kept in module state like the toasts. */
export function useUpdates() {
  /** The status bar's label, or null when there is nothing to show. */
  const notice = computed(() => {
    if (state.info?.mechanism === 'sparkle') return state.info.relaunchPending ? 'Restart to update' : null;
    const status = state.status;
    if (status.kind === 'available') return status.update.version === state.skipped ? null : 'Update available';
    if (status.kind === 'downloading') return 'Downloading update…';
    if (status.kind === 'ready') return 'Update ready';
    return null;
  });
  /** The status bar button: Sparkle's held relaunch, or the update dialog. */
  function openNotice(): void {
    if (state.info?.mechanism === 'sparkle') void answerRelaunchRequest();
    else openDialog();
  }
  return {
    state: readonly(state),
    notice,
    openNotice,
    checkNow,
    openDialog,
    closeDialog,
    skipVersion,
    download,
    installAndRelaunch,
    setAutoCheck,
  };
}
