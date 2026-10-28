import { call } from './invoke';

/**
 * How this build updates: `sparkle` on macOS (Sparkle owns its windows and
 * checks), `tauri` on Windows and Linux (this UI runs the flow), `none` when
 * the build has no update feed (dev builds).
 */
export type UpdateMechanism = 'sparkle' | 'tauri' | 'none';

/** `UpdateInfo` in app/src-tauri. */
export interface UpdateInfo {
  mechanism: UpdateMechanism;
  currentVersion: string;
  autoCheck: boolean;
  /** Sparkle holds a downloaded update's relaunch back until the UI continues it. */
  relaunchPending: boolean;
}

/** The answer of `update_check`. */
export type UpdateCheck =
  /** macOS: Sparkle opened its own window, nothing to show here. */
  | { kind: 'sparkle' }
  | { kind: 'upToDate' }
  | { kind: 'available'; version: string; notes: string | null; date: string | null };

/** Bytes downloaded so far; `total` is null when the server sends no length. */
export interface UpdateProgress {
  downloaded: number;
  total: number | null;
}

/** Sent while `update_download` runs, with an `UpdateProgress`. */
export const UPDATE_PROGRESS = 'varve://update-progress';

/** Sparkle: the user clicked "Install and Relaunch"; answer with `answerUpdateRelaunch`. */
export const UPDATE_RELAUNCH_REQUESTED = 'varve://update-relaunch-requested';

export function fetchUpdateInfo(): Promise<UpdateInfo> {
  return call<UpdateInfo>('update_info');
}

/** Checks the feed (tauri), or opens Sparkle's own window (sparkle). */
export function checkForUpdate(): Promise<UpdateCheck> {
  return call<UpdateCheck>('update_check');
}

/** Tauri only: downloads the update found by the last check, sending `UPDATE_PROGRESS`. */
export async function downloadUpdate(): Promise<void> {
  await call<unknown>('update_download');
}

/**
 * Tauri only: installs the downloaded update and restarts the app. Call it
 * only after the unsaved-work check said yes.
 */
export async function installUpdate(): Promise<void> {
  await call<unknown>('update_install');
}

export async function setUpdateAutoCheck(autoCheck: boolean): Promise<void> {
  await call<unknown>('set_update_auto_check', { autoCheck });
}

/** Sparkle only: the page listens for `UPDATE_RELAUNCH_REQUESTED` from now on. */
export async function markUpdateListening(): Promise<void> {
  await call<unknown>('update_listening');
}

/**
 * Sparkle only: `true` lets Sparkle install and relaunch; `false` keeps the
 * app open, and Sparkle keeps the relaunch held (`relaunchPending`).
 */
export async function answerUpdateRelaunch(proceed: boolean): Promise<void> {
  await call<unknown>('update_relaunch', { proceed });
}
