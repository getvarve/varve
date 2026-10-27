import { call } from './invoke';

/** `Engine` in crates/core (lowercase). */
export type Engine = 'postgres' | 'mysql' | 'mariadb' | 'redis';

/** `Environment` in crates/records/src/model.rs. */
export type Environment = 'development' | 'test' | 'staging' | 'production';

/** `TlsMode` in crates/driver/src/lib.rs. */
export type TlsMode = 'disable' | 'prefer' | 'require' | 'verify_ca' | 'verify_full';

/** `SshAuthMethod` in crates/records/src/model.rs. */
export type SshAuthMethod = 'password' | 'key_file' | 'agent';

/** `SecretKindName` in crates/records/src/model.rs. */
export type SecretKindName = 'db_password' | 'ssh_password' | 'ssh_key_passphrase' | 'tls_key_passphrase';

export const ENGINES: readonly Engine[] = ['postgres', 'mysql', 'mariadb', 'redis'];
export const ENVIRONMENTS: readonly Environment[] = ['development', 'test', 'staging', 'production'];

export interface Folder {
  id: string;
  workspaceId: string;
  parentId: string | null;
  name: string;
  color: string | null;
  sortOrder: number;
  createdAt: number;
  updatedAt: number;
  version: number;
}

export interface TlsSettings {
  mode: TlsMode;
  caPath: string | null;
  clientCertPath: string | null;
  clientKeyPath: string | null;
}

export interface SshSettings {
  enabled: boolean;
  host: string;
  port: number;
  username: string;
  auth: SshAuthMethod;
  keyPath: string | null;
}

export interface ConnectionOptions {
  readOnly: boolean;
  confirmDestructive: boolean;
  statementTimeoutMs: number | null;
  keepaliveS: number | null;
  initSql: string | null;
  showSystemObjects: boolean;
  redisDb: number;
}

export interface ConnectionProfile {
  id: string;
  workspaceId: string;
  folderId: string | null;
  name: string;
  color: string | null;
  sortOrder: number;
  engine: Engine;
  host: string;
  port: number;
  database: string | null;
  username: string | null;
  environment: Environment;
  tls: TlsSettings;
  ssh: SshSettings;
  options: ConnectionOptions;
  savedSecrets: SecretKindName[];
  createdAt: number;
  updatedAt: number;
  version: number;
}

export interface ConnectionInput {
  folderId: string | null;
  name: string;
  color: string | null;
  engine: Engine;
  host: string;
  port: number | null;
  database: string | null;
  username: string | null;
  environment: Environment;
  tls: TlsSettings;
  ssh: SshSettings;
  options: ConnectionOptions;
}

export interface SavedQuery {
  id: string;
  workspaceId: string;
  folderId: string | null;
  connectionId: string | null;
  database: string | null;
  name: string;
  sql: string;
  tags: string[];
  sortOrder: number;
  createdAt: number;
  updatedAt: number;
  version: number;
}

export interface SavedQueryInput {
  folderId: string | null;
  connectionId: string | null;
  database: string | null;
  name: string;
  sql: string;
  tags: string[];
}

export interface FolderContents {
  folders: number;
  connections: number;
  queries: number;
}

export type FolderDeleteMode = 'move_contents_to_parent' | 'delete_contents';
export type ReorderKind = 'folders' | 'connections' | 'queries';

/** `Tree` in app/src-tauri/src/commands.rs. */
export interface WorkspaceRecords {
  folders: Folder[];
  connections: ConnectionProfile[];
  queries: SavedQuery[];
}

/** Default port of an engine (`default_port` in model.rs). */
export function defaultPort(engine: Engine): number {
  switch (engine) {
    case 'postgres':
      return 5432;
    case 'mysql':
    case 'mariadb':
      return 3306;
    case 'redis':
      return 6379;
  }
}

/** Input that recreates a saved profile, for partial updates such as rename or move. */
export function inputFromProfile(profile: ConnectionProfile): ConnectionInput {
  return {
    folderId: profile.folderId,
    name: profile.name,
    color: profile.color,
    engine: profile.engine,
    host: profile.host,
    port: profile.port,
    database: profile.database,
    username: profile.username,
    environment: profile.environment,
    tls: { ...profile.tls },
    ssh: { ...profile.ssh },
    options: { ...profile.options },
  };
}

/** Input that recreates a saved query. */
export function inputFromQuery(query: SavedQuery): SavedQueryInput {
  return {
    folderId: query.folderId,
    connectionId: query.connectionId,
    database: query.database,
    name: query.name,
    sql: query.sql,
    tags: [...query.tags],
  };
}

export function fetchTree(): Promise<WorkspaceRecords> {
  return call<WorkspaceRecords>('tree');
}

export function createFolder(parentId: string | null, name: string, color: string | null = null): Promise<Folder> {
  return call<Folder>('create_folder', { parentId, name, color });
}

export function updateFolder(id: string, name: string, color: string | null): Promise<Folder> {
  return call<Folder>('update_folder', { id, name, color });
}

export function moveFolder(id: string, parentId: string | null): Promise<Folder> {
  return call<Folder>('move_folder', { id, parentId });
}

export function folderContents(id: string): Promise<FolderContents> {
  return call<FolderContents>('folder_contents', { id });
}

export async function deleteFolder(id: string, mode: FolderDeleteMode): Promise<void> {
  await call<unknown>('delete_folder', { id, mode });
}

export async function reorder(kind: ReorderKind, ids: string[]): Promise<void> {
  await call<unknown>('reorder', { kind, ids });
}

export function createConnection(input: ConnectionInput): Promise<ConnectionProfile> {
  return call<ConnectionProfile>('create_connection', { input });
}

export function updateConnection(id: string, input: ConnectionInput): Promise<ConnectionProfile> {
  return call<ConnectionProfile>('update_connection', { id, input });
}

export function moveConnection(id: string, folderId: string | null): Promise<ConnectionProfile> {
  return call<ConnectionProfile>('move_connection', { id, folderId });
}

export function duplicateConnection(id: string): Promise<ConnectionProfile> {
  return call<ConnectionProfile>('duplicate_connection', { id });
}

export async function deleteConnection(id: string): Promise<void> {
  await call<unknown>('delete_connection', { id });
}

/**
 * Saves a secret in the keychain. `expectedVersion` is the profile `version`
 * the secret was entered for: the backend refuses it (`invalid`) when an edit
 * since then moved the connection to another server, account or route.
 */
export function saveSecret(id: string, kind: SecretKindName, value: string, expectedVersion?: number): Promise<ConnectionProfile> {
  return call<ConnectionProfile>('save_secret', { id, kind, value, expectedVersion: expectedVersion ?? null });
}

export function forgetSecret(id: string, kind: SecretKindName): Promise<ConnectionProfile> {
  return call<ConnectionProfile>('forget_secret', { id, kind });
}

/**
 * Trusts an SSH host key for the host and port that presented it (taken from
 * the host key error). The backend accepts only plain host names and IP addresses.
 */
export async function trustSshHostKey(host: string, port: number, presentedKey: string): Promise<void> {
  await call<unknown>('trust_ssh_host_key', { host, port, presentedKey });
}

export function createQuery(input: SavedQueryInput): Promise<SavedQuery> {
  return call<SavedQuery>('create_query', { input });
}

export function updateQuery(id: string, input: SavedQueryInput): Promise<SavedQuery> {
  return call<SavedQuery>('update_query', { id, input });
}

export async function deleteQuery(id: string): Promise<void> {
  await call<unknown>('delete_query', { id });
}
