import { CommandFailure, secretLabel, toCmdError } from '../api/errors';
import { saveSecret, trustSshHostKey } from '../api/records';
import type { ConnectionProfile, SecretKindName } from '../api/records';
import type { TypedFor, TypedSecrets } from '../api/sessions';
import { identityFingerprint, nextConnectStep, reusableTypedSecrets } from '../lib/connectFlow';
import { hostPort } from '../lib/display';
import { useDialogs } from './useDialogs';
import type { SecretAnswer } from './useDialogs';
import { useToasts } from './useToasts';
import { useWorkspace } from './useWorkspace';

/** A field of `TypedSecrets` that holds a secret. */
type Slot = 'password' | 'sshSecret' | 'tlsPassphrase';

const SLOTS: readonly Slot[] = ['password', 'sshSecret', 'tlsPassphrase'];

/** A typed secret and the profile `version` it was typed for. */
interface KeptSecret {
  value: string;
  typedFor: number;
}

type KeptSecrets = Partial<Record<Slot, KeptSecret>>;

/**
 * Secrets typed in this app run without "Save in keychain". They live only in
 * memory, are passed to `open_session` and `test_connection`, each with the
 * profile version it was typed for (the backend refuses one typed before an
 * edit moved the connection to another server), and are gone when the app
 * quits.
 */
const typedSecrets = new Map<string, KeptSecrets>();

/**
 * Bumped whenever a connection's identity changes (an edit that moves it to
 * another server or route) or it is deleted. A connect that started before
 * that writes nothing back: no secrets in memory, no keychain entries.
 */
const generations = new Map<string, number>();

/**
 * The version the connection editor last saved a connection as in this
 * window (`retainTypedSecretsAfterEdit`): what secrets typed in its form
 * belong to when the editor remembers them.
 */
const editedTo = new Map<string, number>();

function generationOf(connectionId: string): number {
  return generations.get(connectionId) ?? 0;
}

function bumpGeneration(connectionId: string): void {
  generations.set(connectionId, generationOf(connectionId) + 1);
}

function keep(connectionId: string, kept: KeptSecrets): void {
  if (SLOTS.some((slot) => kept[slot] !== undefined)) typedSecrets.set(connectionId, kept);
  else typedSecrets.delete(connectionId);
}

/**
 * Remembers secrets typed in the connection editor (unsaved ones only stay in
 * memory). `typedFor` is the profile version they were typed for; left out,
 * it is the version the editor saved the connection as in this window (see
 * `retainTypedSecretsAfterEdit`), and for a connection the editor only
 * created the lowest one: any later edit to another server makes them
 * unusable.
 */
export function rememberTypedSecrets(connectionId: string, secrets: TypedSecrets, typedFor?: number): void {
  const version = typedFor ?? editedTo.get(connectionId) ?? 0;
  const kept: KeptSecrets = { ...typedSecrets.get(connectionId) };
  for (const slot of SLOTS) {
    const value = secrets[slot];
    if (value !== undefined) kept[slot] = { value, typedFor: version };
  }
  keep(connectionId, kept);
}

/**
 * The connections the window holds something for: typed secrets, or the
 * version the editor saved. After the records were reloaded, the ones that
 * are no longer a connection are forgotten.
 */
export function typedSecretIds(): string[] {
  return [...new Set([...typedSecrets.keys(), ...editedTo.keys()])];
}

/** Secrets kept in memory for a connection (for tests and the editor). */
export function typedSecretsOf(connectionId: string): TypedSecrets {
  const kept = typedSecrets.get(connectionId);
  const out: TypedSecrets = {};
  for (const slot of SLOTS) {
    const secret = kept?.[slot];
    if (secret) out[slot] = secret.value;
  }
  return out;
}

/**
 * Applies a change of a connection's stored settings to the typed secrets:
 * the ones typed for `before` or earlier are kept only while they still
 * belong to the same server and tunnel (see `reusableTypedSecrets`); the
 * ones typed for a later version (the new settings) stay. A connect that
 * started before a change of server or route writes nothing back.
 */
function followEdit(before: ConnectionProfile, after: ConnectionProfile): void {
  if (identityFingerprint(before) !== identityFingerprint(after)) bumpGeneration(before.id);
  const current = typedSecrets.get(before.id);
  if (!current) return;
  const older: TypedSecrets = {};
  for (const slot of SLOTS) {
    const secret = current[slot];
    if (secret && secret.typedFor <= before.version) older[slot] = secret.value;
  }
  const reusable = reusableTypedSecrets(before, after, older);
  const kept: KeptSecrets = {};
  for (const slot of SLOTS) {
    const secret = current[slot];
    if (!secret) continue;
    if (secret.typedFor > before.version || reusable[slot] !== undefined) kept[slot] = secret;
  }
  keep(before.id, kept);
}

/**
 * After the connection editor saved a profile with new settings, keeps only
 * the typed secrets that still belong to the same server and tunnel (see
 * `reusableTypedSecrets`).
 */
export function retainTypedSecretsAfterEdit(before: ConnectionProfile, after: ConnectionProfile): void {
  editedTo.set(after.id, after.version);
  followEdit(before, after);
}

/**
 * The same for a change made outside this window (an MCP client): the
 * window learns of it only by reloading the records, and must not keep a
 * secret typed for the old server.
 */
export function followExternalEdit(before: ConnectionProfile, after: ConnectionProfile): void {
  followEdit(before, after);
}

export function forgetTypedSecrets(connectionId: string): void {
  bumpGeneration(connectionId);
  typedSecrets.delete(connectionId);
  editedTo.delete(connectionId);
}

/**
 * Forgets every typed secret: when the stored settings could not be read
 * after an outside change, none can be checked against them.
 */
export function forgetAllTypedSecrets(): void {
  for (const id of [...typedSecrets.keys()]) forgetTypedSecrets(id);
}

function secretSlot(kind: SecretKindName): Slot | null {
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
  const secrets: TypedSecrets = {};
  /** The profile version each secret was typed for. */
  const typedFor: Partial<Record<Slot, number>> = {};
  const kept = typedSecrets.get(profile.id);
  for (const slot of SLOTS) {
    const secret = kept?.[slot];
    if (!secret) continue;
    secrets[slot] = secret.value;
    typedFor[slot] = secret.typedFor;
  }
  for (const slot of SLOTS) {
    const value = options.initial?.[slot];
    if (value === undefined) continue;
    secrets[slot] = value;
    typedFor[slot] = profile.version;
  }
  const generation = generationOf(profile.id);
  const toSave = new Map<SecretKindName, string>();

  const stale = (): boolean => generationOf(profile.id) !== generation;
  const settingsChanged = (): CommandFailure =>
    new CommandFailure({ kind: 'internal', message: 'The connection settings changed while connecting. Connect again.' });
  /**
   * What a connect call gets: the secrets and, for each of them, the profile
   * version it was typed for, which the backend checks one by one.
   */
  const sent = (): TypedSecrets => {
    const versions: TypedFor = {};
    let any = false;
    for (const slot of SLOTS) {
      if (secrets[slot] === undefined) continue;
      versions[slot] = typedFor[slot] ?? profile.version;
      any = true;
    }
    return any ? { ...secrets, typedFor: versions } : { ...secrets };
  };

  for (let round = 0; round < MAX_ROUNDS; round += 1) {
    // Secrets and answers captured so far belong to the settings this attempt
    // started with; after an edit to another server or route they are never sent.
    if (stale()) throw settingsChanged();
    try {
      const result = await run(sent());
      // The connection was edited to another server or route (or deleted)
      // while this attempt ran: keep nothing from it.
      if (generationOf(profile.id) !== generation) return result;
      // The connection works. Saving a typed secret in the keychain is a
      // separate step: if it fails, the session stays open, the secret is
      // kept in memory for this app run only, and the keychain error is
      // reported on its own (without the secret).
      const remembered: KeptSecrets = {};
      const remember = (slot: Slot): void => {
        const value = secrets[slot];
        if (value !== undefined) remembered[slot] = { value, typedFor: typedFor[slot] ?? profile.version };
      };
      let savedAny = false;
      for (const [kind, value] of toSave) {
        try {
          await saveSecret(profile.id, kind, value, profile.version);
          savedAny = true;
        } catch (saveError: unknown) {
          const slot = secretSlot(kind);
          if (slot) remembered[slot] = { value, typedFor: profile.version };
          useToasts().pushError(saveError, `Connected, but the ${secretLabel(kind)} could not be saved in the keychain`);
        }
      }
      if (!toSave.has('db_password')) remember('password');
      if (!toSave.has('ssh_password') && !toSave.has('ssh_key_passphrase')) remember('sshSecret');
      if (!toSave.has('tls_key_passphrase')) remember('tlsPassphrase');
      if (options.remember !== false) keep(profile.id, remembered);
      if (savedAny) await useWorkspace().refresh();
      return result;
    } catch (error: unknown) {
      const cmd = toCmdError(error);
      if (cmd.kind === 'secretRequired') {
        const slot = secretSlot(cmd.secret);
        if (slot !== null && secrets[slot] !== undefined) {
          // The backend refused a secret that was sent: it was typed before
          // an edit that made secrets of its kind unusable (another server,
          // account or route).
          if ((typedFor[slot] ?? profile.version) >= profile.version) {
            // Typed for this very profile, so the profile shown is outdated.
            throw settingsChanged();
          }
          // Typed earlier in this app run for older settings: it is not used
          // again, and the prompt below asks for this one. The other typed
          // secrets stay: the backend checks each against the edits that
          // matter for its own kind, and refuses it in its turn if needed.
          Reflect.deleteProperty(secrets, slot);
          Reflect.deleteProperty(typedFor, slot);
          const current = typedSecrets.get(profile.id);
          const held = current?.[slot];
          if (current && held && held.typedFor < profile.version) {
            const left: KeptSecrets = {};
            for (const other of SLOTS) {
              const secret = current[other];
              if (secret && other !== slot) left[other] = secret;
            }
            keep(profile.id, left);
          }
        }
      }
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
      typedFor[slot] = profile.version;
      options.onAnswered?.(step.secret, answer);
      if (answer.save) toSave.set(step.secret, answer.value);
      else toSave.delete(step.secret);
    }
  }
  throw new CommandFailure({ kind: 'internal', message: 'Too many attempts to connect. Check the connection settings.' });
}
