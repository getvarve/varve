import type { TreeNode } from '../../lib/tree';
import { anythingOpen, collapseAllPlan, collapsePlan } from './collapse';

function folder(id: string, children: TreeNode[]): TreeNode {
  return { kind: 'folder', key: `folder:${id}`, id, name: id, parentId: null, children } as unknown as TreeNode;
}
function connection(id: string): TreeNode {
  return { kind: 'connection', key: `connection:${id}`, id, name: id, parentId: null, children: [] } as unknown as TreeNode;
}

// webshop { orders, inner { staging } }, legacy { old }
const roots: TreeNode[] = [folder('webshop', [connection('orders'), folder('inner', [connection('staging')])]), folder('legacy', [connection('old')])];

const expanded = new Set(['folder:webshop', 'folder:inner', 'connection:orders', 'connection:staging', 'folder:legacy', 'folder:gone']);
const objectExpanded = new Set([
  'objects:orders/db%3Ashop',
  'objects:orders/db%3Ashop/s%3Apublic',
  'objects:orders/db%3Ashop/s%3Aaudit',
  'objects:staging/db%3Ashop',
  'objects:old/db%3Amain',
  // Another connection whose id starts like "orders".
  'objects:orders2/db%3Ax',
]);

describe('collapsePlan', () => {
  it('closes a folder with every folder, connection and object tree inside it', () => {
    const plan = collapsePlan(roots, 'folder:webshop', expanded, objectExpanded);
    expect(plan.workspace).toEqual(['folder:webshop', 'connection:orders', 'folder:inner', 'connection:staging']);
    expect(plan.objects).toEqual([
      'objects:orders/db%3Ashop',
      'objects:orders/db%3Ashop/s%3Apublic',
      'objects:orders/db%3Ashop/s%3Aaudit',
      'objects:staging/db%3Ashop',
    ]);
  });

  it('closes a connection with its object tree only', () => {
    const plan = collapsePlan(roots, 'connection:orders', expanded, objectExpanded);
    expect(plan.workspace).toEqual(['connection:orders']);
    expect(plan.objects).toEqual(['objects:orders/db%3Ashop', 'objects:orders/db%3Ashop/s%3Apublic', 'objects:orders/db%3Ashop/s%3Aaudit']);
  });

  it('closes an object node with the object nodes below it, not its siblings', () => {
    const plan = collapsePlan(roots, 'objects:orders/db%3Ashop/s%3Apublic', expanded, objectExpanded);
    expect(plan).toEqual({ workspace: [], objects: ['objects:orders/db%3Ashop/s%3Apublic'] });
    const database = collapsePlan(roots, 'objects:orders/db%3Ashop', expanded, objectExpanded);
    expect(database.objects).toEqual(['objects:orders/db%3Ashop', 'objects:orders/db%3Ashop/s%3Apublic', 'objects:orders/db%3Ashop/s%3Aaudit']);
  });

  it('also closes what is open under a closed row', () => {
    const plan = collapsePlan(roots, 'folder:legacy', expanded, objectExpanded);
    expect(plan).toEqual({ workspace: ['folder:legacy'], objects: ['objects:old/db%3Amain'] });
  });
});

describe('Collapse all', () => {
  it('closes every folder, connection and object node of the tree', () => {
    const plan = collapseAllPlan(roots, expanded, objectExpanded);
    expect(new Set(plan.workspace)).toEqual(new Set(['folder:webshop', 'folder:inner', 'connection:orders', 'connection:staging', 'folder:legacy']));
    expect(plan.objects).not.toContain('objects:orders2/db%3Ax');
    expect(plan.objects).toHaveLength(5);
  });

  it('has nothing to do when only keys of rows that are gone are open', () => {
    expect(anythingOpen(roots, expanded, objectExpanded)).toBe(true);
    expect(anythingOpen(roots, new Set(['folder:gone']), new Set(['objects:gone/db%3Ax']))).toBe(false);
    expect(anythingOpen(roots, new Set(), new Set(['objects:old/db%3Amain']))).toBe(true);
    expect(anythingOpen([], new Set(), new Set())).toBe(false);
  });
});
