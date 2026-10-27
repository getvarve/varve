import { call } from './invoke';

/** `SecretSettings` in app/src-tauri/src/settings.rs. */
export interface SecretSettings {
  /**
   * Keep each saved secret in memory after its first keychain read, until the
   * app quits or the user forgets them, so every session of a connection
   * shares one keychain access. On by default.
   */
  rememberForSession: boolean;
}

export function fetchSecretSettings(): Promise<SecretSettings> {
  return call<SecretSettings>('secret_settings');
}

/** Stores the settings and applies them at once; switching remembering off forgets the cached secrets. */
export async function saveSecretSettings(settings: SecretSettings): Promise<void> {
  await call<unknown>('set_secret_settings', { settings });
}

/** Drops the cached secrets; resolves how many were dropped. */
export function forgetCachedSecrets(): Promise<number> {
  return call<number>('forget_cached_secrets');
}
