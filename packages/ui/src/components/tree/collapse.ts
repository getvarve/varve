import { useWorkspace } from '../../composables/useWorkspace';
import { findNode, objectRootKey } from '../../lib/tree';
import type { SidebarRow, TreeNode } from '../../lib/tree';
import { useObjectBrowser } from './objectBrowser';

/** Open keys to close: folders and connections (workspace) and object nodes. */
export interface CollapsePlan {
  workspace: string[];
  objects: string[];
}

/** Object keys of a connection's tree, and below an object node, share this prefix. */
function below(key: string): string {
  return `${key}/`;
}

/**
 * What collapsing the row `key` closes: the row itself and every open row
 * under it, also those not on screen (under a closed folder), so opening it
 * again shows only its direct children. A folder or connection takes the
 * folders, connections and object trees inside it along; an object node the
 * object nodes below it.
 */
export function collapsePlan(
  roots: readonly TreeNode[],
  key: string,
  expanded: ReadonlySet<string>,
  objectExpanded: ReadonlySet<string>,
): CollapsePlan {
  const node = findNode(roots, key);
  if (!node) {
    // An object node (or one the tree no longer has): itself and its object subtree.
    const prefix = below(key);
    return { workspace: [], objects: [...objectExpanded].filter((k) => k === key || k.startsWith(prefix)) };
  }
  const workspace: string[] = [];
  const prefixes: string[] = [];
  const walk = (n: TreeNode): void => {
    if (n.kind === 'query') return;
    if (expanded.has(n.key)) workspace.push(n.key);
    if (n.kind === 'connection') prefixes.push(below(objectRootKey(n.id)));
    n.children.forEach(walk);
  };
  walk(node);
  return { workspace, objects: [...objectExpanded].filter((k) => prefixes.some((p) => k.startsWith(p))) };
}

/** Every open folder, connection and object node of the tree (Collapse all). */
export function collapseAllPlan(roots: readonly TreeNode[], expanded: ReadonlySet<string>, objectExpanded: ReadonlySet<string>): CollapsePlan {
  const plans = roots.map((root) => collapsePlan(roots, root.key, expanded, objectExpanded));
  return { workspace: plans.flatMap((p) => p.workspace), objects: plans.flatMap((p) => p.objects) };
}

/**
 * Whether Collapse all has anything to close that the user can see: a row
 * of the sidebar that is shown open. What is stored as open but not on
 * screen (object nodes under a closed connection, a folder inside a closed
 * folder, keys of rows that are gone) does not turn the button on; Collapse
 * all still closes it along with the rest.
 *
 * While filtering, folders and connections are shown open whatever is
 * stored and Collapse all cannot close them, so only object nodes the user
 * opened count.
 */
export function visiblyOpen(rows: readonly SidebarRow[], filtering: boolean, objectExpanded: ReadonlySet<string>): boolean {
  return rows.some((row) => {
    if (!row.expanded || row.item.type === 'status') return false;
    if (!filtering) return true;
    return row.item.type === 'object' && objectExpanded.has(row.key);
  });
}

/** Collapsing rows of the sidebar tree, shared by its rows, keys and Collapse all button. */
export function useCollapse() {
  const workspace = useWorkspace();
  const objects = useObjectBrowser();

  function apply(plan: CollapsePlan): void {
    workspace.collapseKeys(plan.workspace);
    objects.collapseKeys(plan.objects);
  }

  return {
    /** Collapses a row and everything under it. */
    collapse: (key: string): void => apply(collapsePlan(workspace.tree.value, key, workspace.expanded, objects.objectExpanded)),
    collapseAll: (): void => apply(collapseAllPlan(workspace.tree.value, workspace.expanded, objects.objectExpanded)),
  };
}
