import { invoke } from '@tauri-apps/api/core';

/** Matches `AppInfo` in app/src-tauri/src/lib.rs. */
export interface AppInfo {
  edition: 'community' | 'pro';
  productName: string;
  version: string;
  dataDir: string;
  extensions: { id: string; name: string }[];
  capabilities: string[];
  commands: string[];
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

/** Reads edition, version and registered extensions from the backend. */
export function fetchAppInfo(): Promise<AppInfo> {
  return invoke<AppInfo>('app_info');
}

/** Calls an extension command by its full name, for example `pro.test.ping`. */
export function extInvoke<T = unknown>(command: string, payload?: unknown): Promise<T> {
  return invoke<T>('ext_invoke', { command, payload: payload ?? null });
}
