import {
  createFolder,
  deleteConnection,
  deleteFolder,
  deleteQuery,
  duplicateConnection,
  folderContents,
  inputFromProfile,
  inputFromQuery,
  moveConnection,
  moveFolder,
  reorder,
  updateConnection,
  updateFolder,
  updateQuery,
} from '../api/records';
import type { ConnectionProfile, Engine, Folder, FolderContents, SavedQuery } from '../api/records';
import { findNode } from '../lib/tree';
import type { DropOperation, TreeNode } from '../lib/tree';
import { forgetTypedSecrets } from './useConnect';
import { useDialogs } from './useDialogs';
import { useQuerySave } from './useQuerySave';
import { useTabs } from './useTabs';
import { useToasts } from './useToasts';
import { useWorkspace } from './useWorkspace';

/** Runs a record change, shows failures as a toast and reloads the tree either way. */
async function mutate<T>(work: () => Promise<T>, failureTitle: string): Promise<T | null> {
  const workspace = useWorkspace();
  try {
    return await work();
  } catch (error: unknown) {
    useToasts().pushError(error, failureTitle);
    return null;
  } finally {
    await workspace.refresh();
  }
}

async function newConnection(folderId: string | null = null, engine: Engine = 'postgres'): Promise<ConnectionProfile | null> {
  const profile = await useDialogs().editConnection({ profile: null, engine, folderId });
  if (profile) await useWorkspace().refresh();
  return profile;
}

async function editConnection(profile: ConnectionProfile): Promise<void> {
  const saved = await useDialogs().editConnection({ profile, engine: profile.engine, folderId: profile.folderId });
  if (saved) await useWorkspace().refresh();
}

async function duplicate(profile: ConnectionProfile): Promise<void> {
  const copy = await mutate(() => duplicateConnection(profile.id), 'Could not duplicate the connection');
  if (copy) useToasts().push('success', `Duplicated as ${copy.name}`, 'Saved passwords are not copied; you are asked on first connect.');
}

async function removeConnection(profile: ConnectionProfile): Promise<void> {
  const ok = await useDialogs().askConfirm({
    title: `Delete connection "${profile.name}"?`,
    message:
      'The connection profile and its keychain entries on this device are removed. Saved queries attached to it stay in their folders. Open tabs of this connection close.',
    confirmLabel: 'Delete connection',
    danger: true,
  });
  if (!ok) return;
  // Unsaved editor text is asked about first; tabs close only after the delete worked.
  const tabIds = useTabs().tabs.filter((t) => t.connectionId === profile.id).map((t) => t.id);
  if (!(await useQuerySave().confirmUnsavedTabs(tabIds))) return;
  const done = await mutate(() => deleteConnection(profile.id).then(() => true), 'Could not delete the connection');
  if (done) {
    // Forget in-memory secrets at once, before awaiting the tab closes.
    forgetTypedSecrets(profile.id);
    await useTabs().closeConnectionTabs(profile.id);
    useToasts().push('success', `Deleted ${profile.name}`);
  }
}

async function newFolder(parentId: string | null): Promise<Folder | null> {
  const folder = await mutate(() => createFolder(parentId, 'New folder'), 'Could not create the folder');
  if (folder && parentId) useWorkspace().setExpanded(`folder:${parentId}`, true);
  return folder;
}

async function rename(node: { kind: 'folder'; folder: Folder } | { kind: 'connection'; connection: ConnectionProfile } | { kind: 'query'; query: SavedQuery }, name: string): Promise<boolean> {
  const trimmed = name.trim();
  if (trimmed === '') {
    useToasts().push('warning', 'Names cannot be empty');
    return false;
  }
  const result = await mutate(async () => {
    if (node.kind === 'folder') await updateFolder(node.folder.id, trimmed, node.folder.color);
    else if (node.kind === 'connection') await updateConnection(node.connection.id, { ...inputFromProfile(node.connection), name: trimmed });
    else await updateQuery(node.query.id, { ...inputFromQuery(node.query), name: trimmed });
    return true;
  }, 'Could not rename');
  return result === true;
}

/** Ids of the connections inside a folder, through every subfolder. */
function connectionsInFolder(folderId: string): string[] {
  const node = findNode(useWorkspace().tree.value, `folder:${folderId}`);
  const out: string[] = [];
  const walk = (n: TreeNode): void => {
    if (n.kind === 'connection') out.push(n.id);
    if (n.kind === 'folder') n.children.forEach(walk);
  };
  if (node) walk(node);
  return out;
}

/** Ids of the folders in `parentId` (`null`: the top level), in the order the tree shows them. */
function folderIdsIn(roots: readonly TreeNode[], parentId: string | null): string[] {
  const parent = parentId === null ? null : findNode(roots, `folder:${parentId}`);
  const list = parentId === null ? roots : (parent?.children ?? []);
  return list.filter((n) => n.kind === 'folder').map((n) => n.id);
}

/**
 * Where a restored folder goes among the folders next to it now: right after
 * the nearest folder that stood before it when it was deleted and is still
 * there, or first when none is. `before` is the order at deletion, with the
 * deleted folder's id; `current` the folders there now, without the restored
 * one; `restoredId` the restored folder's new id.
 */
export function restoredFolderOrder(before: readonly string[], deletedId: string, current: readonly string[], restoredId: string): string[] {
  const at = before.indexOf(deletedId);
  const ids = current.filter((id) => id !== restoredId);
  let insert = 0;
  for (let i = at - 1; i >= 0; i -= 1) {
    const found = ids.indexOf(before[i] ?? '');
    if (found >= 0) {
      insert = found + 1;
      break;
    }
  }
  ids.splice(insert, 0, restoredId);
  return ids;
}

/** Makes a deleted empty folder again: same name, colour, parent and place among its sibling folders. */
async function restoreFolder(folder: Folder, siblingsBefore: readonly string[]): Promise<void> {
  const workspace = useWorkspace();
  let placed = true;
  const restored = await mutate(async () => {
    const created = await createFolder(folder.parentId, folder.name, folder.color);
    const ids = restoredFolderOrder(siblingsBefore, folder.id, folderIdsIn(workspace.tree.value, folder.parentId), created.id);
    // The new folder is created last among its siblings; put it back where it was.
    if (ids.at(-1) !== created.id) {
      try {
        await reorder('folders', ids);
      } catch {
        // The folder is back all the same (a folder next to it went meanwhile); only its place differs.
        placed = false;
      }
    }
    return created;
  }, 'Could not restore the folder');
  if (!restored) return;
  if (placed) useToasts().push('success', `Folder ${restored.name} restored`, '', 2500);
  else useToasts().push('info', `Folder ${restored.name} restored at the end of the list`, 'Its old place could not be used.');
}

/**
 * Deletes a folder with nothing in it at once: there is nothing to decide
 * about. The toast offers Undo for as long as it shows (6 seconds).
 */
async function removeEmptyFolder(folder: Folder): Promise<void> {
  const workspace = useWorkspace();
  const siblings = folderIdsIn(workspace.tree.value, folder.parentId);
  // Something added since the check moves to the parent rather than being deleted.
  const done = await mutate(() => deleteFolder(folder.id, 'move_contents_to_parent').then(() => true), 'Could not delete the folder');
  if (!done) return;
  workspace.collapseKeys([`folder:${folder.id}`]);
  useToasts().push('success', `Folder ${folder.name} deleted`, '', 6000, {
    label: 'Undo',
    run: () => void restoreFolder(folder, siblings),
  });
}

async function removeFolder(folder: Folder): Promise<void> {
  let contents: FolderContents;
  try {
    contents = await folderContents(folder.id);
  } catch (error: unknown) {
    useToasts().pushError(error, 'Could not read the folder');
    return;
  }
  if (contents.folders === 0 && contents.connections === 0 && contents.queries === 0) {
    await removeEmptyFolder(folder);
    return;
  }
  const mode = await useDialogs().askDeleteFolder({ folder, contents });
  if (!mode) return;
  const ids = new Set(mode === 'delete_contents' ? connectionsInFolder(folder.id) : []);
  if (ids.size > 0) {
    const tabIds = useTabs().tabs.filter((t) => ids.has(t.connectionId)).map((t) => t.id);
    if (!(await useQuerySave().confirmUnsavedTabs(tabIds))) return;
  }
  const done = await mutate(() => deleteFolder(folder.id, mode).then(() => true), 'Could not delete the folder');
  if (done && ids.size > 0) {
    // The connections inside were gathered before the delete; use those,
    // not a tree refresh that may have failed.
    for (const id of ids) forgetTypedSecrets(id);
    for (const id of ids) await useTabs().closeConnectionTabs(id);
  }
}

async function removeQuery(query: SavedQuery): Promise<void> {
  const ok = await useDialogs().askConfirm({
    title: `Delete saved query "${query.name}"?`,
    message: 'The saved query is removed. Open editor tabs keep their text.',
    confirmLabel: 'Delete query',
    danger: true,
  });
  if (!ok) return;
  await mutate(() => deleteQuery(query.id), 'Could not delete the saved query');
}

/** Opens a query tab for a connection. */
function openConnection(profile: ConnectionProfile): void {
  useTabs().openQueryTab(profile);
}

/** Opens a saved query in a tab of its connection, or asks which connection to use. */
async function openQuery(query: SavedQuery): Promise<void> {
  const workspace = useWorkspace();
  let profile = workspace.connection(query.connectionId);
  if (!profile) {
    profile = await useDialogs().pickConnection({ title: `Open "${query.name}" on` });
    if (!profile) return;
  }
  useTabs().openQueryTab(profile, query);
}

/** Applies a drag and drop plan in order; a folder cycle error from the backend is shown as a toast. */
async function applyDrop(operations: readonly DropOperation[]): Promise<void> {
  const workspace = useWorkspace();
  await mutate(async () => {
    for (const op of operations) {
      if (op.op === 'moveFolder') await moveFolder(op.id, op.parentId);
      else if (op.op === 'moveConnection') await moveConnection(op.id, op.folderId);
      else if (op.op === 'moveQuery') {
        const query = workspace.query(op.id);
        if (query) await updateQuery(query.id, { ...inputFromQuery(query), folderId: op.folderId });
      } else await reorder(op.kind, op.ids);
    }
  }, 'Could not move');
}

/** Tree and menu actions, shared by the sidebar, the welcome screen and the activity panel. */
export function useWorkspaceActions() {
  return {
    newConnection,
    editConnection,
    duplicate,
    removeConnection,
    newFolder,
    rename,
    removeFolder,
    removeQuery,
    openConnection,
    openQuery,
    applyDrop,
  };
}
