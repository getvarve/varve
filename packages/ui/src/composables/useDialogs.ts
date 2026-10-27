import { markRaw, reactive } from 'vue';
import type {
  ConnectionProfile,
  Engine,
  Environment,
  Folder,
  FolderContents,
  FolderDeleteMode,
  SavedQuery,
  SecretKindName,
} from '../api/records';
import type { TreeNode } from '../lib/tree';

export interface SecretAnswer {
  value: string;
  save: boolean;
}

export interface SaveQueryRequest {
  connection: ConnectionProfile;
  sql: string;
  database: string | null;
  /** Prefilled from the open saved query, for "Save as". */
  name: string;
  folderId: string | null;
  tags: string[];
}

/** Every dialog the app can show, with the type of its answer. */
export type DialogSpec =
  | { type: 'secret'; profile: ConnectionProfile; secret: SecretKindName; reason: string | null; answer: SecretAnswer | null }
  | {
      type: 'hostKey';
      server: string;
      code: 'host_key_unknown' | 'host_key_changed';
      fingerprints: string[];
      answer: boolean;
    }
  | {
      type: 'destructive';
      connectionName: string;
      environment: Environment;
      statements: string[];
      /** Sentences shown above the statements (for example why a change cannot be checked). */
      warnings?: string[];
      answer: boolean;
    }
  | { type: 'confirm'; title: string; message: string; confirmLabel: string; danger: boolean; answer: boolean }
  | { type: 'deleteFolder'; folder: Folder; contents: FolderContents; answer: FolderDeleteMode | null }
  | {
      type: 'connectionEditor';
      profile: ConnectionProfile | null;
      engine: Engine;
      folderId: string | null;
      answer: ConnectionProfile | null;
    }
  | { type: 'saveQuery'; request: SaveQueryRequest; answer: SavedQuery | null }
  | { type: 'pickConnection'; title: string; answer: ConnectionProfile | null }
  | { type: 'moveTo'; node: TreeNode; answer: { folderId: string | null } | null }
  | { type: 'unsavedClose'; title: string; saved: boolean; answer: 'save' | 'discard' | null };

type Spec<K extends DialogSpec['type']> = Extract<DialogSpec, { type: K }>;
type Request<K extends DialogSpec['type']> = Omit<Spec<K>, 'answer' | 'type'>;

export interface OpenDialog {
  id: number;
  spec: DialogSpec;
  resolve: (answer: unknown) => void;
}

const state = reactive<{ open: OpenDialog[] }>({ open: [] });
let nextId = 1;

/** Opens a dialog and resolves with its answer when it closes. */
function ask<K extends DialogSpec['type']>(type: K, request: Request<K>): Promise<Spec<K>['answer']> {
  return new Promise((resolve) => {
    const id = nextId;
    nextId += 1;
    const spec = { ...request, type, answer: null } as unknown as DialogSpec;
    state.open.push({
      id,
      spec: markRaw(spec),
      resolve: (answer: unknown) => resolve(answer as never),
    });
  });
}

/** Closes a dialog with its answer. */
function close(id: number, answer: unknown): void {
  const at = state.open.findIndex((d) => d.id === id);
  if (at < 0) return;
  const [dialog] = state.open.splice(at, 1);
  dialog?.resolve(answer);
}

/** Promise-based dialogs, rendered by DialogHost. */
export function useDialogs() {
  return {
    dialogs: state.open,
    close,
    askSecret: (r: Request<'secret'>) => ask('secret', r),
    askHostKey: (r: Request<'hostKey'>) => ask('hostKey', r),
    askDestructive: (r: Request<'destructive'>) => ask('destructive', r),
    askConfirm: (r: Request<'confirm'>) => ask('confirm', r),
    askDeleteFolder: (r: Request<'deleteFolder'>) => ask('deleteFolder', r),
    editConnection: (r: Request<'connectionEditor'>) => ask('connectionEditor', r),
    saveQuery: (r: Request<'saveQuery'>) => ask('saveQuery', r),
    pickConnection: (r: Request<'pickConnection'>) => ask('pickConnection', r),
    moveTo: (r: Request<'moveTo'>) => ask('moveTo', r),
    askUnsavedClose: (r: Request<'unsavedClose'>) => ask('unsavedClose', r),
  };
}
