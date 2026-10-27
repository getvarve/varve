import { invoke } from '@tauri-apps/api/core';
import { CommandFailure, toCmdError } from './errors';

/**
 * Calls a backend command. Rejects with a `CommandFailure` whose `cmd` is the
 * parsed `CmdError`, so every caller handles errors the same way.
 */
export async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error: unknown) {
    throw new CommandFailure(toCmdError(error));
  }
}
