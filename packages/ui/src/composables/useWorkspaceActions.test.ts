import type * as RecordsModule from '../api/records';
import type { Folder, FolderContents, WorkspaceRecords } from '../api/records';
import { useDialogs } from './useDialogs';
import { useToasts } from './useToasts';
import { useWorkspace } from './useWorkspace';
import { restoredFolderOrder, useWorkspaceActions } from './useWorkspaceActions';

function folder(id: string, sortOrder: number, parentId: string | null = null): Folder {
  return { id, workspaceId: 'personal', parentId, name: id, color: null, sortOrder, createdAt: 0, updatedAt: 0, version: 1 } as Folder;
}

let records: WorkspaceRecords = { folders: [], connections: [], queries: [] };
let contents: FolderContents = { folders: 0, connections: 0, queries: 0 };
const deleteFolder = vi.fn(async (id: string, _mode: string) => {
  records = { ...records, folders: records.folders.filter((f) => f.id !== id) };
});
const createFolder = vi.fn(async (parentId: string | null, name: string, color: string | null) => {
  const created = { ...folder(`new-${name}`, 99, parentId), name, color };
  records = { ...records, folders: [...records.folders, created] };
  return created;
});
const reorder = vi.fn(async (_kind: string, _ids: string[]): Promise<void> => undefined);

vi.mock('../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: vi.fn(async () => records),
  folderContents: vi.fn(async () => contents),
  deleteFolder: (id: string, mode: string) => deleteFolder(id, mode),
  createFolder: (parentId: string | null, name: string, color: string | null) => createFolder(parentId, name, color),
  reorder: (kind: string, ids: string[]) => reorder(kind, ids),
}));

const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

describe('restoredFolderOrder', () => {
  it('puts the folder back after the nearest folder that stood before it', () => {
    expect(restoredFolderOrder(['a', 'b', 'c'], 'b', ['a', 'c'], 'n')).toEqual(['a', 'n', 'c']);
    // The folder before it went meanwhile: after the one before that.
    expect(restoredFolderOrder(['a', 'x', 'b', 'c'], 'b', ['a', 'c'], 'n')).toEqual(['a', 'n', 'c']);
  });

  it('puts it first when it was first or nothing before it is left, and keeps folders added since', () => {
    expect(restoredFolderOrder(['b', 'c'], 'b', ['c', 'd'], 'n')).toEqual(['n', 'c', 'd']);
    expect(restoredFolderOrder(['a', 'b'], 'b', ['z'], 'n')).toEqual(['n', 'z']);
    expect(restoredFolderOrder(['a', 'b'], 'b', ['a', 'd', 'n'], 'n')).toEqual(['a', 'n', 'd']);
  });
});

describe('deleting a folder', () => {
  beforeEach(async () => {
    records = { folders: [folder('Alpha', 0), folder('Scratch', 1), folder('Zulu', 2)], connections: [], queries: [] };
    contents = { folders: 0, connections: 0, queries: 0 };
    deleteFolder.mockClear();
    createFolder.mockClear();
    reorder.mockClear();
    for (const t of [...useToasts().toasts]) useToasts().dismiss(t.id);
    await useWorkspace().refresh();
  });

  it('deletes an empty folder at once, and Undo makes it again in its place', async () => {
    useWorkspace().setExpanded('folder:Scratch', true);
    await useWorkspaceActions().removeFolder(folder('Scratch', 1));
    expect(useDialogs().dialogs).toHaveLength(0);
    // Moving, not deleting, the contents is what makes a delete without a question safe: anything added to the
    // folder since it was read as empty goes to the parent.
    expect(deleteFolder).toHaveBeenCalledWith('Scratch', 'move_contents_to_parent');
    expect(useWorkspace().expanded.has('folder:Scratch')).toBe(false);
    const toast = useToasts().toasts.at(-1);
    expect(toast?.title).toBe('Folder Scratch deleted');
    expect(toast?.action?.label).toBe('Undo');

    useToasts().act(toast?.id ?? -1);
    for (let i = 0; i < 4; i += 1) await tick();
    expect(useToasts().toasts.some((t) => t.id === toast?.id)).toBe(false);
    expect(createFolder).toHaveBeenCalledWith(null, 'Scratch', null);
    expect(reorder).toHaveBeenCalledWith('folders', ['Alpha', 'new-Scratch', 'Zulu']);
    expect(useWorkspace().records.value.folders.map((f) => f.name)).toContain('Scratch');
  });

  it('says the folder is back at the end of the list when only putting it in its place fails', async () => {
    await useWorkspaceActions().removeFolder(folder('Scratch', 1));
    const toast = useToasts().toasts.at(-1);
    reorder.mockRejectedValueOnce(new Error('a folder in the list is gone'));
    useToasts().act(toast?.id ?? -1);
    for (let i = 0; i < 4; i += 1) await tick();
    expect(createFolder).toHaveBeenCalledWith(null, 'Scratch', null);
    const titles = useToasts().toasts.map((t) => t.title);
    expect(titles).toContain('Folder Scratch restored at the end of the list');
    expect(titles).not.toContain('Could not restore the folder');
    expect(useWorkspace().records.value.folders.map((f) => f.name)).toContain('Scratch');
  });

  it('reports a folder that could not be made again', async () => {
    await useWorkspaceActions().removeFolder(folder('Scratch', 1));
    const toast = useToasts().toasts.at(-1);
    createFolder.mockRejectedValueOnce(new Error('the parent folder is gone'));
    useToasts().act(toast?.id ?? -1);
    for (let i = 0; i < 4; i += 1) await tick();
    expect(useToasts().toasts.map((t) => t.title)).toContain('Could not restore the folder');
    expect(reorder).not.toHaveBeenCalled();
  });

  it('opens the Delete folder dialog for a folder that holds anything', async () => {
    contents = { folders: 0, connections: 0, queries: 1 };
    const dialogs = useDialogs();
    const done = useWorkspaceActions().removeFolder(folder('Scratch', 1));
    await tick();
    const ask = dialogs.dialogs.find((d) => d.spec.type === 'deleteFolder');
    expect(ask).toBeDefined();
    if (ask) dialogs.close(ask.id, null);
    await done;
    expect(deleteFolder).not.toHaveBeenCalled();
  });
});
