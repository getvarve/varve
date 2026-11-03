import { RECORDS_CHANGED, SETTINGS_CHANGED } from '../api/mcp';
import { installExternalChanges, useExternalChanges } from './useExternalChanges';

const handlers = new Map<string, () => void>();
const unlisten = vi.fn();
const refresh = vi.fn(async () => undefined);
const refreshInfo = vi.fn(async () => undefined);

vi.mock('@tauri-apps/api/core', () => ({ isTauri: () => true }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: async (event: string, handler: () => void) => {
    handlers.set(event, handler);
    return unlisten;
  },
}));
vi.mock('./useWorkspace', () => ({ useWorkspace: () => ({ refresh }) }));
vi.mock('./useUpdates', () => ({ useUpdates: () => ({ refreshInfo }) }));

describe('installExternalChanges', () => {
  it('reloads the tree and the settings after an MCP change, until removed', async () => {
    const remove = await installExternalChanges();
    const { settingsRevision } = useExternalChanges();

    handlers.get(RECORDS_CHANGED)?.();
    expect(refresh).toHaveBeenCalledTimes(1);

    const before = settingsRevision.value;
    handlers.get(SETTINGS_CHANGED)?.();
    expect(settingsRevision.value).toBe(before + 1);
    expect(refreshInfo).toHaveBeenCalledTimes(1);

    remove();
    expect(unlisten).toHaveBeenCalledTimes(2);
  });
});
