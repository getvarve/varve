import { sidebarRows } from '../../lib/tree';
import type { ObjectSource, SidebarRow, TreeNode } from '../../lib/tree';
import { collapseAllPlan, collapsePlan, visiblyOpen } from './collapse';

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

  it('also closes what is stored as open but not on screen', () => {
    const plan = collapseAllPlan(roots, new Set(['connection:old']), new Set(['objects:old/db%3Amain']));
    expect(plan).toEqual({ workspace: ['connection:old'], objects: ['objects:old/db%3Amain'] });
  });
});

describe('visiblyOpen (whether Collapse all is on)', () => {
  // No connection is browsed: an open connection shows its status row.
  const source: ObjectSource = { root: () => undefined, children: () => undefined } as unknown as ObjectSource;
  const rowsFor = (open: string[], objects: string[] = [], filter = ''): SidebarRow[] =>
    sidebarRows(roots, { expanded: new Set(open), objectExpanded: new Set(objects), filter }, source);

  it('is on when a row is shown open', () => {
    expect(visiblyOpen(rowsFor(['folder:legacy']), false, new Set())).toBe(true);
    expect(visiblyOpen(rowsFor(['folder:legacy', 'connection:old']), false, new Set())).toBe(true);
  });

  it('is off when nothing is shown open, whatever is stored as open out of sight', () => {
    expect(visiblyOpen(rowsFor([]), false, new Set())).toBe(false);
    // Keys of rows that are gone.
    expect(visiblyOpen(rowsFor(['folder:gone'], ['objects:gone/db%3Ax']), false, new Set(['objects:gone/db%3Ax']))).toBe(false);
    // Object nodes under a closed connection, and a connection and a folder inside closed folders. The old rule
    // turned the button on for these, and a click then seemed to do nothing.
    const hidden = ['objects:old/db%3Amain'];
    expect(visiblyOpen(rowsFor([], hidden), false, new Set(hidden))).toBe(false);
    expect(visiblyOpen(rowsFor(['connection:old', 'folder:inner']), false, new Set())).toBe(false);
    expect(visiblyOpen([], false, new Set())).toBe(false);
  });

  it('counts only opened object nodes while filtering, when folders and connections are shown open anyway', () => {
    const filtered = rowsFor([], [], 'old');
    expect(filtered.some((row) => row.expanded)).toBe(true);
    expect(visiblyOpen(filtered, true, new Set())).toBe(false);
    const object = { key: 'objects:old/db%3Amain', expanded: true, item: { type: 'object' } } as unknown as SidebarRow;
    expect(visiblyOpen([...filtered, object], true, new Set(['objects:old/db%3Amain']))).toBe(true);
    expect(visiblyOpen([...filtered, object], true, new Set())).toBe(false);
  });
});
