import { markRaw, reactive } from 'vue';

/**
 * The actions a tab's view offers to the menu bar. The view that holds them (a query tab's run, save and history)
 * registers them while it is mounted, so the menu runs exactly what its buttons run.
 */
export interface TabCommands {
  run(): void;
  cancel(): void;
  save(): void;
  saveAs(): void;
  toggleHistory(): void;
}

// Reactive, so the menu bar notices a view that mounts after its tab came in front.
const commands = reactive(new Map<string, TabCommands>());

/** Makes a tab's actions reachable from the menu bar; returns the function that takes them away again. */
export function registerTabCommands(tabId: string, actions: TabCommands): () => void {
  const held = markRaw(actions);
  commands.set(tabId, held);
  return () => {
    if (commands.get(tabId) === held) commands.delete(tabId);
  };
}

/** The actions of a tab, when its view offers any. */
export function tabCommands(tabId: string): TabCommands | null {
  return commands.get(tabId) ?? null;
}
