import { invoke } from '@tauri-apps/api/core';

/** Matches `AppInfo` in app/src-tauri/src/lib.rs. */
export interface AppInfo {
  version: string;
  dataDir: string;
  extensions: { id: string; name: string }[];
  capabilities: string[];
  commands: string[];
  /** A debug build (`tauri dev`): the webview's own context menu and reload stay available. */
  debugBuild: boolean;
  /**
   * The native glass of the window (app/src-tauri/src/glass.rs). `liquidGlass` is glass behind the shapes the UI
   * reports with `setGlassShapes`, over an opaque window; `vibrancy` and `mica` lie behind the whole window; `none`
   * keeps the UI opaque.
   */
  glass: 'liquidGlass' | 'vibrancy' | 'mica' | 'none';
}

/** Error shape returned by extension commands. */
export interface CommandError {
  code: string;
  message: string;
}

export function isCommandError(value: unknown): value is CommandError {
  return (
    typeof value === 'object' &&
    value !== null &&
    typeof (value as Record<string, unknown>).code === 'string' &&
    typeof (value as Record<string, unknown>).message === 'string'
  );
}

/** Reads version, data folder and registered extensions from the backend. */
export function fetchAppInfo(): Promise<AppInfo> {
  return invoke<AppInfo>('app_info');
}

/** One glass shape: CSS pixels from the top-left corner of the page. Matches `GlassShape` in src/glass.rs. */
export interface GlassShape {
  id: string;
  x: number;
  y: number;
  width: number;
  height: number;
  radius: number;
}

/**
 * Shows exactly these Liquid Glass shapes behind the page (macOS 26). Answers false where the window has no such
 * glass; the UI then keeps what it would have put glass behind opaque.
 */
export function setGlassShapes(shapes: GlassShape[]): Promise<boolean> {
  return invoke<boolean>('set_glass_shapes', { shapes });
}

/** Calls an extension command by its full name, for example `acme.test.ping`. */
export function extInvoke<T = unknown>(command: string, payload?: unknown): Promise<T> {
  return invoke<T>('ext_invoke', { command, payload: payload ?? null });
}
