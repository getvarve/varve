import { CommandFailure, secretLabel, toCmdError } from '../api/errors';
import { saveSecret, trustSshHostKey } from '../api/records';
import type { ConnectionProfile, SecretKindName } from '../api/records';
import type { TypedSecrets } from '../api/sessions';
import { identityFingerprint, nextConnectStep, reusableTypedSecrets } from '../lib/connectFlow';
import { hostPort } from '../lib/display';
import { useDialogs } from './useDialogs';
import type { SecretAnswer } from './useDialogs';
import { useToasts } from './useToasts';
import { useWorkspace } from './useWorkspace';

/**
 * Secrets typed in this app run without "Save in keychain". They live only in
 * memory, are passed to `open_session` and `test_connection`, and are gone
 * when the app quits.
 */
const typedSecrets = new Map<string, TypedSecrets>();

/**
 * Bumped whenever a connection's identity changes (an edit that moves it to
 * another server or route) or it is deleted. A connect that started before
 * that writes nothing back: no secrets in memory, no keychain entries.
 */
const generations = new Map<string, number>();

function generationOf(connectionId: string): number {
  return generations.get(connectionId) ?? 0;
}

function bumpGeneration(connectionId: string): void {
  generations.set(connectionId, generationOf(connectionId) + 1);
}

/** Remembers secrets typed in the connection editor (unsaved ones only stay in memory). */
export function rememberTypedSecrets(connectionId: string, secrets: TypedSecrets): void {
  const current = typedSecrets.get(connectionId) ?? {};
  typedSecrets.set(connectionId, { ...current, ...secrets });
}

/** Secrets kept in memory for a connection (for tests and the editor). */
export function typedSecretsOf(connectionId: string): TypedSecrets {
  return { ...typedSecrets.get(connectionId) };
}

/**
 * After a profile was saved with new settings, keeps only the typed secrets
 * that still belong to the same server and tunnel (see `reusableTypedSecrets`).
 */
export function retainTypedSecretsAfterEdit(before: ConnectionProfile, after: ConnectionProfile): void {
  if (identityFingerprint(before) !== identityFingerprint(after)) bumpGeneration(before.id);
  const current = typedSecrets.get(before.id);
  if (!current) return;
  const kept = reusableTypedSecrets(before, after, current);
  if (kept.password === undefined && kept.sshSecret === undefined && kept.tlsPassphrase === undefined) {
    typedSecrets.delete(before.id);
  }
  else typedSecrets.set(before.id, kept);
}

export function forgetTypedSecrets(connectionId: string): void {
  bumpGeneration(connectionId);
  typedSecrets.delete(connectionId);
}

function secretSlot(kind: SecretKindName): keyof TypedSecrets | null {
  if (kind === 'db_password') return 'password';
  if (kind === 'ssh_password' || kind === 'ssh_key_passphrase') return 'sshSecret';
  if (kind === 'tls_key_passphrase') return 'tlsPassphrase';
  return null;
}

const MAX_ROUNDS = 6;

/**
 * Runs a call that connects (`open_session`, `test_connection` on a saved
 * profile) and answers what the backend asks for along the way: a password
 * or passphrase prompt (saved in the keychain after success when the user
 * ticked "Save in keychain"), trusting an SSH host key, or a new password
 * after an authentication failure. Resolves `null` when the user cancels.
 */
export interface ConnectPromptOptions {
  /** Secrets typed elsewhere (the connection editor) for the first attempt; prompt answers replace them. */
  initial?: TypedSecrets;
  /** Called after the user answered a prompt, so a form can clear its now outdated field. */
  onAnswered?: (secret: SecretKindName, answer: SecretAnswer) => void;
  /**
   * Whether typed secrets are kept in memory for later connects (default
   * true). The editor's Test sets false: a password typed for a test of an
   * edit that is then discarded must not be used again.
   */
  remember?: boolean;
}

export async function withConnectPrompts<T>(
  profile: ConnectionProfile,
  run: (secrets: TypedSecrets) => Promise<T>,
  options: ConnectPromptOptions = {},
): Promise<T | null> {
  const dialogs = useDialogs();
  const secrets: TypedSecrets = { ...typedSecrets.get(profile.id), ...options.initial };
  const generation = generationOf(profile.id);
  const toSave = new Map<SecretKindName, string>();

  const stale = (): boolean => generationOf(profile.id) !== generation;
  const settingsChanged = (): CommandFailure =>
    new CommandFailure({ kind: 'internal', message: 'The connection settings changed while connecting. Connect again.' });

  for (let round = 0; round < MAX_ROUNDS; round += 1) {
    // Secrets and answers captured so far belong to the settings this attempt
    // started with; after an edit to another server or route they are never sent.
    if (stale()) throw settingsChanged();
    try {
      const result = await run(secrets);
      // The connection was edited to another server or route (or deleted)
      // while this attempt ran: keep nothing from it.
      if (generationOf(profile.id) !== generation) return result;
      // The connection works. Saving a typed secret in the keychain is a
      // separate step: if it fails, the session stays open, the secret is
      // kept in memory for this app run only, and the keychain error is
      // reported on its own (without the secret).
      const remembered: TypedSecrets = {};
      let savedAny = false;
      for (const [kind, value] of toSave) {
        try {
          await saveSecret(profile.id, kind, value, profile.version);
          savedAny = true;
        } catch (saveError: unknown) {
          const slot = secretSlot(kind);
          if (slot) remembered[slot] = value;
          useToasts().pushError(saveError, `Connected, but the ${secretLabel(kind)} could not be saved in the keychain`);
        }
      }
      if (secrets.password !== undefined && !toSave.has('db_password')) remembered.password = secrets.password;
      if (secrets.sshSecret !== undefined && !toSave.has('ssh_password') && !toSave.has('ssh_key_passphrase')) {
        remembered.sshSecret = secrets.sshSecret;
      }
      if (secrets.tlsPassphrase !== undefined && !toSave.has('tls_key_passphrase')) {
        remembered.tlsPassphrase = secrets.tlsPassphrase;
      }
      if (options.remember !== false) typedSecrets.set(profile.id, remembered);
      if (savedAny) await useWorkspace().refresh();
      return result;
    } catch (error: unknown) {
      const cmd = toCmdError(error);
      const step = nextConnectStep(cmd, profile);
      if (step.step === 'fail') throw error instanceof CommandFailure ? error : new CommandFailure(cmd);
      if (step.step === 'trustHostKey') {
        const trusted = await dialogs.askHostKey({
          server: hostPort(step.host, step.port),
          code: step.code,
          fingerprints: step.fingerprints,
        });
        if (!trusted) return null;
        if (stale()) throw settingsChanged();
        // Trust the key for the host and port that presented it (from the
        // error), never whatever a profile says by now.
        await trustSshHostKey(step.host, step.port, step.presentedKey);
        continue;
      }
      const slot = secretSlot(step.secret);
      if (slot === null) throw error instanceof CommandFailure ? error : new CommandFailure(cmd);
      const answer = await dialogs.askSecret({ profile, secret: step.secret, reason: step.reason });
      if (!answer) return null;
      if (stale()) throw settingsChanged();
      secrets[slot] = answer.value;
      options.onAnswered?.(step.secret, answer);
      if (answer.save) toSave.set(step.secret, answer.value);
      else toSave.delete(step.secret);
    }
  }
  throw new CommandFailure({ kind: 'internal', message: 'Too many attempts to connect. Check the connection settings.' });
}
