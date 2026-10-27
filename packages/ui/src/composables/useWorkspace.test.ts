import type * as RecordsModule from '../api/records';
import type { WorkspaceRecords } from '../api/records';
import { useWorkspace } from './useWorkspace';

const resolvers: ((value: WorkspaceRecords) => void)[] = [];
const fetchTree = vi.fn(
  () =>
    new Promise<WorkspaceRecords>((resolve) => {
      resolvers.push(resolve);
    }),
);
vi.mock('../api/records', async (original) => ({
  ...(await original<typeof RecordsModule>()),
  fetchTree: () => fetchTree(),
}));

const tree = (name: string): WorkspaceRecords => ({
  folders: [{ id: name, workspaceId: 'personal', parentId: null, name, color: null, sortOrder: 0, createdAt: 0, updatedAt: 0, version: 1 }],
  connections: [],
  queries: [],
});
const tick = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

describe('useWorkspace refresh', () => {
  it('runs a new fetch after the one in flight instead of joining it', async () => {
    const workspace = useWorkspace();
    const first = workspace.refresh();
    // A write happens now, then two more refreshes are asked for.
    const second = workspace.refresh();
    const third = workspace.refresh();
    expect(fetchTree).toHaveBeenCalledTimes(1);
    resolvers[0]?.(tree('before-write'));
    await first;
    await tick();
    expect(fetchTree).toHaveBeenCalledTimes(2);
    resolvers[1]?.(tree('after-write'));
    await Promise.all([second, third]);
    expect(fetchTree).toHaveBeenCalledTimes(2);
    expect(workspace.records.value.folders[0]?.name).toBe('after-write');
  });
});
