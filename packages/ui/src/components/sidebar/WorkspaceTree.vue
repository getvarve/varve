<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import { sameObject } from '../../api/catalog';
import type { ObjectRef } from '../../api/catalog';
import { useDialogs } from '../../composables/useDialogs';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import { useWorkspaceActions } from '../../composables/useWorkspaceActions';
import { useToasts } from '../../composables/useToasts';
import { colorVar, ENVIRONMENT_INFO } from '../../lib/display';
import { singleFlight } from '../../lib/async';
import { domId } from '../../lib/domId';
import { renameFormSchema } from '../../lib/forms';
import type { MenuItem } from '../../lib/menu';
import { requiredFields } from '../../lib/schemaRequired';
import { findNode, neighbourDrop, objectExpandable, planDrop, sidebarRows } from '../../lib/tree';
import type { DropPosition, ObjNode, SidebarRow, TreeNode } from '../../lib/tree';
import ContextMenu from '../common/ContextMenu.vue';
import EngineTile from '../common/EngineTile.vue';
import Icon from '../common/Icon.vue';
import { defaultAction, isObjectAction, objectMenuItems, runObjectAction } from '../tree/objectActions';
import { useObjectBrowser } from '../tree/objectBrowser';
import ObjectIcon from '../tree/ObjectIcon.vue';
import { openConnectionOverview } from '../tree/openConnection';

const workspace = useWorkspace();
const actions = useWorkspaceActions();
const tabs = useTabs();
const toasts = useToasts();
const router = useRouter();
const dialogs = useDialogs();
const objects = useObjectBrowser();

// Inline rename is a one-field form with its own schema.
const renameForm = useForm({ validationSchema: toTypedSchema(renameFormSchema), initialValues: { name: '' } });
const [renameText, renameAttrs] = renameForm.defineField('name');
const renameRequired = computed(() => requiredFields(renameFormSchema, renameForm.values, ['name']).has('name'));
const renameError = computed(() => renameForm.errors.value.name ?? '');

const filter = ref('');
const focusedKey = ref<string | null>(null);
const renamingKey = ref<string | null>(null);
const treeEl = ref<HTMLElement | null>(null);

const filtering = computed(() => filter.value.trim() !== '');
const rows = computed<SidebarRow[]>(() =>
  sidebarRows(
    workspace.tree.value,
    { expanded: workspace.expanded, objectExpanded: objects.objectExpanded, filter: filter.value },
    objects.source,
  ),
);
const indexByKey = computed(() => new Map(rows.value.map((r, i) => [r.key, i])));
const tabStop = computed(() => {
  const key = focusedKey.value;
  return key !== null && indexByKey.value.has(key) ? key : (rows.value[0]?.key ?? null);
});
const activeConnectionId = computed(() => tabs.active.value?.connectionId ?? null);

/** The object the active tab shows, to mark its row. */
const activeObject = computed<{ connectionId: string; object: ObjectRef } | null>(() => {
  const tab = tabs.active.value;
  if (!tab) return null;
  if ((tab.kind === 'object' || tab.kind === 'tableDesigner') && tab.object) return { connectionId: tab.connectionId, object: tab.object };
  if (tab.kind === 'ddl' || tab.kind === 'tableData') return { connectionId: tab.connectionId, object: tab.object };
  return null;
});

function isActiveObject(node: ObjNode): boolean {
  const active = activeObject.value;
  return !!active && node.type === 'object' && node.connectionId === active.connectionId && sameObject(node.summary.object, active.object);
}

// Expanded object nodes load their children; also those restored from the last run.
watch(
  rows,
  (list) => {
    for (const row of list) {
      if (row.item.type !== 'object' || !row.expanded) continue;
      const node = row.item.node;
      if (node.type !== 'section' && objects.source.children(node.key) === undefined) void objects.loadChildren(node);
    }
  },
  { immediate: true },
);

// Virtual scrolling: only the rows in view (plus a margin, the focused row and the tab stop) are rendered.

const ROW_HEIGHT = 28;
const OVERSCAN = 12;
const viewport = reactive({ scrollTop: 0, height: 0, listTop: 0 });
let scroller: HTMLElement | null = null;
let resizeObserver: ResizeObserver | null = null;

function scrollParent(el: HTMLElement | null): HTMLElement | null {
  let cursor = el?.parentElement ?? null;
  while (cursor) {
    const overflow = getComputedStyle(cursor).overflowY;
    if (overflow === 'auto' || overflow === 'scroll') return cursor;
    cursor = cursor.parentElement;
  }
  return document.scrollingElement instanceof HTMLElement ? document.scrollingElement : null;
}

function measure(): void {
  const list = treeEl.value;
  if (!scroller || !list) return;
  viewport.scrollTop = scroller.scrollTop;
  viewport.height = scroller.clientHeight;
  viewport.listTop = list.getBoundingClientRect().top - scroller.getBoundingClientRect().top + scroller.scrollTop;
}

function attachScroller(): void {
  const next = scrollParent(treeEl.value);
  if (next === scroller) return;
  scroller?.removeEventListener('scroll', measure);
  resizeObserver?.disconnect();
  scroller = next;
  if (!scroller) return;
  scroller.addEventListener('scroll', measure, { passive: true });
  if (typeof ResizeObserver !== 'undefined') {
    resizeObserver = new ResizeObserver(measure);
    resizeObserver.observe(scroller);
  }
  measure();
}

onMounted(() => {
  attachScroller();
  window.addEventListener('resize', measure);
});

onBeforeUnmount(() => {
  scroller?.removeEventListener('scroll', measure);
  resizeObserver?.disconnect();
  window.removeEventListener('resize', measure);
});

// The list appears and moves (empty states, filter) as rows change: measure again after each render.
watch(
  () => rows.value.length,
  async () => {
    await nextTick();
    attachScroller();
    measure();
  },
);

const windowRange = computed(() => {
  // Without layout (hidden, or a test DOM) the window is the screen height.
  const height = viewport.height > 0 ? viewport.height : window.innerHeight;
  const offset = viewport.scrollTop - viewport.listTop;
  const start = Math.max(0, Math.floor(offset / ROW_HEIGHT) - OVERSCAN);
  const end = Math.min(rows.value.length, Math.ceil((offset + height) / ROW_HEIGHT) + OVERSCAN);
  return { start, end };
});

const rendered = computed<{ row: SidebarRow; index: number }[]>(() => {
  const { start, end } = windowRange.value;
  const out = rows.value.slice(start, end).map((row, i) => ({ row, index: start + i }));
  // Keep the focused row and the tab stop in the DOM, so focus and Tab survive scrolling.
  for (const key of new Set([focusedKey.value, tabStop.value])) {
    if (key === null) continue;
    const index = indexByKey.value.get(key);
    const row = index === undefined ? undefined : rows.value[index];
    if (row && index !== undefined && (index < start || index >= end)) out.push({ row, index });
  }
  return out;
});

/** Scrolls the sidebar so the row at `index` is in view. */
function reveal(index: number): void {
  if (!scroller) return;
  const top = viewport.listTop + index * ROW_HEIGHT;
  if (top < scroller.scrollTop) scroller.scrollTop = top;
  else if (top + ROW_HEIGHT > scroller.scrollTop + scroller.clientHeight) scroller.scrollTop = top + ROW_HEIGHT - scroller.clientHeight;
  measure();
}

function rowId(key: string): string {
  return `tree-${domId(key)}`;
}

async function focusRow(key: string | null): Promise<void> {
  if (!key) return;
  focusedKey.value = key;
  const index = indexByKey.value.get(key);
  if (index !== undefined) reveal(index);
  await nextTick();
  document.getElementById(rowId(key))?.focus();
}

function indexOf(key: string | null): number {
  return key === null ? -1 : (indexByKey.value.get(key) ?? -1);
}

function toggle(row: SidebarRow, open?: boolean): void {
  if (!row.expandable) return;
  const next = open ?? !row.expanded;
  if (row.item.type === 'node') {
    // While filtering, folders and connections are shown expanded.
    if (filtering.value) return;
    workspace.setExpanded(row.key, next);
    if (next && row.item.node.kind === 'connection' && !objects.browsing(row.item.node.id)) void objects.browse(row.item.node.id);
  } else if (row.item.type === 'object') {
    objects.setExpanded(row.key, next);
  }
}

function activateObject(row: SidebarRow, node: ObjNode): void {
  const profile = workspace.connection(node.connectionId);
  const action = defaultAction(node);
  if (!profile || !action) {
    toggle(row);
    return;
  }
  runObjectAction(action, node, profile);
}


function activateStatus(row: SidebarRow): void {
  if (row.item.type !== 'status') return;
  const { state, connectionId, ownerKey } = row.item;
  if (state.status !== 'idle' && state.status !== 'error') return;
  if (state.status === 'idle') {
    void objects.browse(connectionId);
    return;
  }
  const owner = row.parentKey !== null ? rows.value[indexOf(row.parentKey)] : undefined;
  const node = owner?.item.type === 'object' && owner.key === ownerKey ? owner.item.node : null;
  void objects.retry(connectionId, node);
}

function activate(row: SidebarRow): void {
  const item = row.item;
  if (item.type === 'status') {
    activateStatus(row);
    return;
  }
  if (item.type === 'object') {
    activateObject(row, item.node);
    return;
  }
  const node = item.node;
  if (node.kind === 'folder') {
    workspace.setExpanded(node.key, !workspace.expanded.has(node.key));
  } else if (node.kind === 'connection') {
    openConnectionOverview(node.connection);
  } else {
    void actions.openQuery(node.query);
  }
}

// Rename

async function startRename(node: TreeNode): Promise<void> {
  renamingKey.value = node.key;
  renameForm.resetForm({ values: { name: node.name } });
  await nextTick();
  const input = document.getElementById(`${rowId(node.key)}-rename`);
  if (input instanceof HTMLInputElement) {
    input.focus();
    input.select();
  }
}

/** Enter saves when the name is valid and keeps editing otherwise; leaving the field with an invalid name cancels. */
async function finishRename(node: TreeNode, leaving: boolean): Promise<void> {
  if (renamingKey.value !== node.key) return;
  const { valid } = await renameForm.validate();
  if (!valid) {
    if (leaving) cancelRename(node);
    return;
  }
  renamingKey.value = null;
  const name = (renameForm.values.name ?? '').trim();
  if (name !== node.name) await actions.rename(node, name);
  await focusRow(node.key);
}

/** One rename at a time: the blur that follows Enter during validation is ignored. */
const renameFlight = singleFlight(finishRename);
function commitRename(node: TreeNode, leaving: boolean): Promise<void> {
  return renameFlight.run(node, leaving);
}

function cancelRename(node: TreeNode): void {
  renamingKey.value = null;
  void focusRow(node.key);
}

// Moving with the keyboard

function movable(node: TreeNode): boolean {
  return !(node.kind === 'query' && node.attached);
}

async function moveTo(node: TreeNode): Promise<void> {
  if (!movable(node)) return;
  const answer = await dialogs.moveTo({ node });
  if (!answer) return;
  const target = answer.folderId ? findNode(workspace.tree.value, `folder:${answer.folderId}`) : null;
  const plan = planDrop(workspace.tree.value, node, target, 'inside');
  if (plan === null) return;
  if ('error' in plan) {
    toasts.push('warning', 'Cannot move the folder there', plan.error);
    return;
  }
  if (target) workspace.setExpanded(target.key, true);
  await actions.applyDrop(plan.operations);
  await focusRow(node.key);
}

async function moveAmongSiblings(node: TreeNode, direction: -1 | 1): Promise<void> {
  if (!movable(node)) return;
  const drop = neighbourDrop(workspace.tree.value, node, direction);
  if (!drop) return;
  const plan = planDrop(workspace.tree.value, node, drop.target, drop.position);
  if (plan === null || 'error' in plan) return;
  await actions.applyDrop(plan.operations);
  await focusRow(node.key);
}

// Deleting

function remove(node: TreeNode): void {
  if (node.kind === 'folder') void actions.removeFolder(node.folder);
  else if (node.kind === 'connection') void actions.removeConnection(node.connection);
  else void actions.removeQuery(node.query);
}

async function addFolder(parentId: string | null): Promise<void> {
  const folder = await actions.newFolder(parentId);
  if (!folder) return;
  await nextTick();
  const node = rows.value.find((r) => r.key === `folder:${folder.id}`)?.item;
  if (node?.type === 'node') await startRename(node.node);
}

// Context menu

const menu = ref<{ x: number; y: number; row: SidebarRow | null } | null>(null);

function connectionMenu(node: Extract<TreeNode, { kind: 'connection' }>): MenuItem[] {
  const redis = node.connection.engine === 'redis';
  const browsing = objects.browsing(node.id);
  const items: MenuItem[] = [
    ...(redis ? [] : [{ id: 'overview', label: 'Open overview' }]),
    { id: 'new-query', label: redis ? 'Open console' : 'New query' },
    ...(redis ? [{ id: 'keys', label: 'Browse keys' }] : []),
    { id: browsing ? 'objects-refresh' : 'objects-browse', label: browsing ? 'Refresh objects' : 'Browse objects' },
  ];
  if (!redis) {
    items.push({ id: 'objects-system', label: objects.includeSystem(node.id) ? 'Hide system objects' : 'Show system objects', disabled: !browsing });
  }
  if (browsing) items.push({ id: 'objects-disconnect', label: 'Close object browser' });
  items.push(
    { id: 'activity', label: 'Activity' },
    { id: 'edit', label: 'Edit connection…', separatorBefore: true },
    { id: 'duplicate', label: 'Duplicate' },
    { id: 'rename', label: 'Rename' },
    { id: 'move', label: 'Move to…' },
    { id: 'delete', label: 'Delete connection…', danger: true, separatorBefore: true },
  );
  return items;
}

const menuItems = computed<MenuItem[]>(() => {
  const row = menu.value?.row ?? null;
  if (!row) {
    return [
      { id: 'new-connection', label: 'New connection' },
      { id: 'new-folder', label: 'New folder' },
    ];
  }
  if (row.item.type === 'status') {
    const state = row.item.state;
    if (state.status === 'idle') return [{ id: 'status-activate', label: 'Browse objects' }];
    if (state.status === 'error') return [{ id: 'status-activate', label: state.reconnect ? 'Reconnect' : 'Retry' }];
    return [];
  }
  if (row.item.type === 'object') return objectMenuItems(row.item.node, objects.capabilities(row.item.node.connectionId));
  const node = row.item.node;
  if (node.kind === 'folder') {
    return [
      { id: 'new-connection', label: 'New connection here' },
      { id: 'new-folder', label: 'New folder inside' },
      { id: 'rename', label: 'Rename', separatorBefore: true },
      { id: 'move', label: 'Move to…' },
      { id: 'delete', label: 'Delete folder…', danger: true },
    ];
  }
  if (node.kind === 'connection') return connectionMenu(node);
  return [
    { id: 'open', label: 'Open' },
    { id: 'rename', label: 'Rename', disabled: node.attached },
    { id: 'move', label: 'Move to…', disabled: node.attached },
    { id: 'delete', label: 'Delete saved query…', danger: true },
  ];
});

const menuLabel = computed(() => {
  const row = menu.value?.row ?? null;
  if (!row) return 'Workspace actions';
  return row.item.type === 'status' ? 'Object browser actions' : `Actions for ${row.item.node.name}`;
});

function openMenu(event: MouseEvent | KeyboardEvent, row: SidebarRow | null): void {
  if (row?.item.type === 'object' && objectMenuItems(row.item.node, objects.capabilities(row.item.node.connectionId)).length === 0) return;
  if (event instanceof MouseEvent && event.type === 'contextmenu') {
    menu.value = { x: event.clientX, y: event.clientY, row };
  } else {
    const el = row ? document.getElementById(rowId(row.key)) : treeEl.value;
    const rect = el?.getBoundingClientRect();
    menu.value = { x: (rect?.left ?? 0) + 24, y: (rect?.bottom ?? 0) - 4, row };
  }
}

function onMenuSelect(id: string): void {
  const row = menu.value?.row ?? null;
  if (row?.item.type === 'status') {
    if (id === 'status-activate') activateStatus(row);
    return;
  }
  if (row?.item.type === 'object') {
    const node = row.item.node;
    const profile = workspace.connection(node.connectionId);
    if (profile && isObjectAction(id)) runObjectAction(id, node, profile);
    return;
  }
  const node = row?.item.type === 'node' ? row.item.node : null;
  switch (id) {
    case 'new-connection':
      void actions.newConnection(node?.kind === 'folder' ? node.id : null);
      break;
    case 'new-folder':
      void addFolder(node?.kind === 'folder' ? node.id : null);
      break;
    case 'rename':
      if (node) void startRename(node);
      break;
    case 'delete':
      if (node) remove(node);
      break;
    case 'move':
      if (node) void moveTo(node);
      break;
    case 'overview':
      if (node?.kind === 'connection') openConnectionOverview(node.connection);
      break;
    case 'new-query':
      if (node?.kind === 'connection') actions.openConnection(node.connection);
      break;
    case 'keys':
      if (node?.kind === 'connection') tabs.openKeysTab(node.connection);
      break;
    case 'objects-browse':
      if (node?.kind === 'connection') {
        workspace.setExpanded(node.key, true);
        void objects.browse(node.id);
      }
      break;
    case 'objects-refresh':
      if (node?.kind === 'connection') void objects.refresh(node.id);
      break;
    case 'objects-system':
      if (node?.kind === 'connection') void objects.setIncludeSystem(node.id, !objects.includeSystem(node.id));
      break;
    case 'objects-disconnect':
      if (node?.kind === 'connection') objects.disconnect(node.id);
      break;
    case 'activity':
      if (node?.kind === 'connection') void router.push({ name: 'activity', query: { connection: node.id } });
      break;
    case 'edit':
      if (node?.kind === 'connection') void actions.editConnection(node.connection);
      break;
    case 'duplicate':
      if (node?.kind === 'connection') void actions.duplicate(node.connection);
      break;
    case 'open':
      if (node?.kind === 'query') void actions.openQuery(node.query);
      break;
  }
}

// Keyboard

function refreshRow(row: SidebarRow): void {
  if (row.item.type === 'object') {
    if (objectExpandable(row.item.node)) void objects.refresh(row.item.node.connectionId, row.item.node);
  } else if (row.item.type === 'node' && row.item.node.kind === 'connection' && objects.browsing(row.item.node.id)) {
    void objects.refresh(row.item.node.id);
  }
}

function onKeydown(event: KeyboardEvent, row: SidebarRow): void {
  if (renamingKey.value) return;
  const at = indexOf(row.key);
  const node = row.item.type === 'node' ? row.item.node : null;
  switch (event.key) {
    case 'ArrowDown':
      if (event.altKey) {
        if (node) void moveAmongSiblings(node, 1);
      } else void focusRow(rows.value[Math.min(at + 1, rows.value.length - 1)]?.key ?? null);
      break;
    case 'ArrowUp':
      if (event.altKey) {
        if (node) void moveAmongSiblings(node, -1);
      } else void focusRow(rows.value[Math.max(at - 1, 0)]?.key ?? null);
      break;
    case 'm':
    case 'M':
      if (!((event.metaKey || event.ctrlKey) && event.shiftKey) || !node) return;
      void moveTo(node);
      break;
    case 'Home':
      void focusRow(rows.value[0]?.key ?? null);
      break;
    case 'End':
      void focusRow(rows.value.at(-1)?.key ?? null);
      break;
    case 'ArrowRight':
      if (!row.expandable) return;
      if (!row.expanded) toggle(row, true);
      else void focusRow(rows.value[at + 1]?.key ?? null);
      break;
    case 'ArrowLeft':
      if (row.expanded && !(filtering.value && row.item.type === 'node')) toggle(row, false);
      else void focusRow(row.parentKey);
      break;
    case 'Enter':
      activate(row);
      break;
    case 'F2':
      if (node && !(node.kind === 'query' && node.attached)) void startRename(node);
      break;
    case 'F5':
      refreshRow(row);
      break;
    case 'Delete':
    case 'Backspace':
      if (!node || (event.key === 'Backspace' && !event.metaKey)) return;
      remove(node);
      break;
    case 'ContextMenu':
      openMenu(event, row);
      break;
    case 'F10':
      if (!event.shiftKey) return;
      openMenu(event, row);
      break;
    default:
      return;
  }
  event.preventDefault();
}

// Drag and drop (folders, connections and saved queries only)

const DRAG_TYPE = 'application/x-varve-tree';
const dragged = ref<TreeNode | null>(null);
const dropTarget = ref<{ key: string | null; position: DropPosition } | null>(null);

function draggable(row: SidebarRow): boolean {
  if (row.item.type !== 'node') return false;
  const node = row.item.node;
  return renamingKey.value !== node.key && !(node.kind === 'query' && node.attached);
}

function onDragStart(event: DragEvent, row: SidebarRow): void {
  if (!draggable(row) || row.item.type !== 'node') {
    event.preventDefault();
    return;
  }
  const node = row.item.node;
  dragged.value = node;
  event.dataTransfer?.setData(DRAG_TYPE, node.key);
  if (event.dataTransfer) event.dataTransfer.effectAllowed = 'move';
}

function positionFor(event: DragEvent, node: TreeNode): DropPosition {
  const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
  const y = (event.clientY - rect.top) / rect.height;
  if (node.kind === 'folder') return y < 0.25 ? 'before' : y > 0.75 ? 'after' : 'inside';
  return y < 0.5 ? 'before' : 'after';
}

function dropNode(row: SidebarRow | null): TreeNode | null | undefined {
  if (row === null) return null;
  return row.item.type === 'node' ? row.item.node : undefined;
}

function onDragOver(event: DragEvent, row: SidebarRow | null): void {
  if (!dragged.value) return;
  const node = dropNode(row);
  if (node === undefined) {
    dropTarget.value = null;
    return;
  }
  const position = node ? positionFor(event, node) : 'inside';
  const plan = planDrop(workspace.tree.value, dragged.value, node, position);
  if (plan === null) {
    dropTarget.value = null;
    return;
  }
  event.preventDefault();
  if (event.dataTransfer) event.dataTransfer.dropEffect = 'error' in plan ? 'none' : 'move';
  dropTarget.value = { key: node?.key ?? null, position };
}

async function onDrop(event: DragEvent, row: SidebarRow | null): Promise<void> {
  event.preventDefault();
  const source = dragged.value;
  const target = dropTarget.value;
  dragged.value = null;
  dropTarget.value = null;
  const node = dropNode(row);
  if (!source || !target || node === undefined) return;
  const plan = planDrop(workspace.tree.value, source, node, target.position);
  if (plan === null) return;
  if ('error' in plan) {
    toasts.push('warning', 'Cannot move the folder there', plan.error);
    return;
  }
  if (node?.kind === 'folder' && target.position === 'inside') workspace.setExpanded(node.key, true);
  await actions.applyDrop(plan.operations);
}

function onDragEnd(): void {
  dragged.value = null;
  dropTarget.value = null;
}

function dropClass(key: string): string | null {
  if (!dropTarget.value || dropTarget.value.key !== key) return null;
  return `drop-${dropTarget.value.position}`;
}

function nodeColor(row: SidebarRow): string | null {
  if (row.item.type !== 'node') return null;
  const node = row.item.node;
  if (node.kind === 'connection') return colorVar(node.connection.color);
  if (node.kind === 'folder') return colorVar(node.folder.color);
  return null;
}

function rowClasses(row: SidebarRow): (string | null | Record<string, boolean>)[] {
  const item = row.item;
  return [
    dropClass(row.key),
    {
      current:
        (item.type === 'node' && item.node.kind === 'connection' && item.node.id === activeConnectionId.value) ||
        (item.type === 'object' && isActiveObject(item.node)),
      attached: item.type === 'node' && item.node.kind === 'query' && item.node.attached,
      status: item.type === 'status',
      group: item.type === 'object' && (item.node.type === 'group' || item.node.type === 'section'),
      system: item.type === 'object' && (item.node.type === 'object' || item.node.type === 'schema' || item.node.type === 'database') && item.node.summary.system,
    },
  ];
}

function statusText(row: SidebarRow): string {
  if (row.item.type !== 'status') return '';
  const state = row.item.state;
  switch (state.status) {
    case 'idle':
      return 'Browse objects';
    case 'loading':
      return state.message;
    case 'empty':
      return 'Nothing here';
    case 'error':
      return `${state.title}: ${state.message}`;
  }
}

function objectTitle(node: ObjNode): string | undefined {
  if (node.type !== 'object' && node.type !== 'database' && node.type !== 'schema') return undefined;
  const parts = [node.summary.comment, node.summary.flags.join(', ')].filter((p): p is string => !!p);
  return parts.length > 0 ? parts.join(' · ') : undefined;
}
</script>

<template>
  <div class="workspace">
    <div class="filter">
      <label
        class="filter-box"
        for="tree-filter"
      >
        <Icon name="search" />
        <span class="visually-hidden">Filter connections, folders, queries and loaded objects</span>
        <input
          id="tree-filter"
          v-model="filter"
          type="search"
          placeholder="Filter"
          autocomplete="off"
          spellcheck="false"
        >
      </label>
    </div>

    <div
      class="section"
      :class="{ 'drop-inside': dragged && dropTarget?.key === null }"
      @dragover="onDragOver($event, null)"
      @drop="onDrop($event, null)"
      @contextmenu.prevent="openMenu($event, null)"
    >
      <span class="label">Personal</span>
      <button
        type="button"
        class="icon-btn small"
        aria-label="New folder"
        title="New folder"
        @click="addFolder(null)"
      >
        <Icon name="folderPlus" />
      </button>
    </div>

    <div
      v-if="workspace.isEmpty.value"
      class="empty"
    >
      <strong>No connections yet</strong>
      <span>Connections and saved queries you create appear here, in folders you choose.</span>
    </div>
    <p
      v-else-if="filtering && rows.length === 0"
      class="empty"
    >
      Nothing matches “{{ filter }}”. The filter searches connections, folders, queries and the objects already loaded.
    </p>

    <ul
      v-show="!workspace.isEmpty.value && rows.length > 0"
      ref="treeEl"
      class="tree"
      role="tree"
      aria-label="Connections, folders, saved queries and database objects"
      aria-describedby="tree-keys-help"
      :style="{ height: `${rows.length * ROW_HEIGHT}px` }"
    >
      <li
        v-for="{ row, index } in rendered"
        :id="rowId(row.key)"
        :key="row.key"
        role="treeitem"
        class="row"
        :class="rowClasses(row)"
        :aria-level="row.level"
        :aria-setsize="row.setSize"
        :aria-posinset="row.posInSet"
        :aria-expanded="row.expandable ? row.expanded : undefined"
        :aria-selected="focusedKey === row.key"
        :aria-busy="row.item.type === 'status' && row.item.state.status === 'loading' ? true : undefined"
        :tabindex="tabStop === row.key ? 0 : -1"
        :draggable="draggable(row)"
        :style="{ top: `${index * ROW_HEIGHT}px`, paddingLeft: `${8 + (row.level - 1) * 16}px` }"
        :title="row.item.type === 'object' ? objectTitle(row.item.node) : undefined"
        @focus="focusedKey = row.key"
        @click="focusRow(row.key)"
        @dblclick="activate(row)"
        @keydown="onKeydown($event, row)"
        @contextmenu.prevent.stop="openMenu($event, row)"
        @dragstart="onDragStart($event, row)"
        @dragover="onDragOver($event, row)"
        @drop="onDrop($event, row)"
        @dragend="onDragEnd"
      >
        <span
          v-if="nodeColor(row)"
          class="color"
          :style="{ background: nodeColor(row) ?? undefined }"
          aria-hidden="true"
        />
        <button
          v-if="row.expandable"
          type="button"
          class="chevron"
          tabindex="-1"
          :aria-label="row.expanded ? 'Collapse' : 'Expand'"
          @click.stop="toggle(row)"
        >
          <Icon
            :name="row.expanded ? 'chevronDown' : 'chevronRight'"
            :size="12"
          />
        </button>
        <span
          v-else
          class="chevron-space"
        />

        <!-- Folders, connections and saved queries -->
        <template v-if="row.item.type === 'node'">
          <Icon
            v-if="row.item.node.kind === 'folder'"
            name="folder"
            class="kind-icon"
          />
          <EngineTile
            v-else-if="row.item.node.kind === 'connection'"
            :engine="row.item.node.connection.engine"
          />
          <Icon
            v-else
            name="file"
            class="kind-icon"
          />

          <input
            v-if="renamingKey === row.key"
            :id="`${rowId(row.key)}-rename`"
            v-model="renameText"
            v-bind="renameAttrs"
            class="rename"
            :aria-label="`New name for ${row.item.node.name}`"
            :aria-required="renameRequired"
            :aria-invalid="!!renameError"
            :aria-describedby="renameError ? `${rowId(row.key)}-rename-error` : undefined"
            :title="renameError || undefined"
            @keydown.enter.prevent="commitRename(row.item.node, false)"
            @keydown.esc.prevent.stop="cancelRename(row.item.node)"
            @blur="commitRename(row.item.node, true)"
            @click.stop
          >
          <span
            v-if="renamingKey === row.key && renameError"
            :id="`${rowId(row.key)}-rename-error`"
            class="visually-hidden"
            role="alert"
          >{{ renameError }}</span>
          <span
            v-if="renamingKey !== row.key"
            class="name"
          >{{ row.item.node.name }}</span>

          <span
            v-if="row.item.node.kind === 'connection'"
            class="env-dot"
            :style="{ background: ENVIRONMENT_INFO[row.item.node.connection.environment].color }"
            :title="ENVIRONMENT_INFO[row.item.node.connection.environment].label"
          >
            <span class="visually-hidden">{{ ENVIRONMENT_INFO[row.item.node.connection.environment].label }}</span>
          </span>
          <span
            v-else-if="row.item.node.kind === 'query' && !row.item.node.attached"
            class="meta"
          >query</span>
        </template>

        <!-- Database objects -->
        <template v-else-if="row.item.type === 'object'">
          <ObjectIcon
            v-if="row.item.node.type === 'database'"
            kind="database"
            class="kind-icon"
          />
          <ObjectIcon
            v-else-if="row.item.node.type === 'schema'"
            kind="schema"
            class="kind-icon"
          />
          <ObjectIcon
            v-else-if="row.item.node.type === 'object'"
            :kind="row.item.node.summary.object.kind"
            class="kind-icon"
          />
          <ObjectIcon
            v-else-if="row.item.node.type === 'action'"
            :action="row.item.node.action"
            class="kind-icon"
          />
          <ObjectIcon
            v-else-if="row.item.node.type === 'section'"
            server
            class="kind-icon"
          />
          <ObjectIcon
            v-else-if="row.item.node.type === 'panel'"
            class="kind-icon"
          />
          <ObjectIcon
            v-else-if="row.item.node.type === 'redisDb'"
            kind="database"
            class="kind-icon"
          />

          <span class="name">
            {{ row.item.node.name }}<template v-if="row.item.node.type === 'group'">
              <span class="count"> ({{ row.item.node.count.toLocaleString() }})</span>
            </template>
          </span>

          <template v-if="row.item.node.type === 'database' && row.item.node.current">
            <span
              class="current-dot"
              title="The database this connection opens"
            >
              <span class="visually-hidden">, connected database</span>
            </span>
          </template>
          <span
            v-if="row.item.node.type === 'object' && row.item.node.summary.flags.length > 0"
            class="flag"
          >{{ row.item.node.summary.flags[0] }}</span>
          <span
            v-if="row.item.node.type === 'object' && row.item.node.summary.detail"
            class="detail"
          >{{ row.item.node.summary.detail }}</span>
          <span
            v-else-if="row.item.node.type === 'redisDb'"
            class="detail"
          >{{ Number(row.item.node.keys).toLocaleString() }} keys</span>
        </template>

        <!-- Loading, empty, failed or not yet browsed -->
        <template v-else>
          <span
            class="status-text"
            :class="{ danger: row.item.state.status === 'error' }"
            :role="row.item.state.status === 'error' ? 'alert' : undefined"
          >
            <Icon
              v-if="row.item.state.status === 'error'"
              name="warning"
              :size="12"
            />
            {{ statusText(row) }}
          </span>
          <button
            v-if="row.item.state.status === 'idle' || row.item.state.status === 'error'"
            type="button"
            class="status-action"
            tabindex="-1"
            @click.stop="activateStatus(row)"
          >
            {{ row.item.state.status === 'idle' ? 'Connect' : row.item.state.reconnect ? 'Reconnect' : 'Retry' }}
          </button>
        </template>
      </li>
    </ul>

    <p
      id="tree-keys-help"
      class="visually-hidden"
    >
      Enter opens (a connection, database or schema opens its overview), F2 renames, F5 refreshes objects, Delete removes, Alt with the up or down arrow reorders, Command or
      Control with Shift and M moves to another folder, Shift F10 opens the menu.
    </p>

    <ContextMenu
      v-if="menu"
      :items="menuItems"
      :x="menu.x"
      :y="menu.y"
      :label="menuLabel"
      @select="onMenuSelect"
      @close="menu = null"
    />
  </div>
</template>

<style scoped>
.workspace {
  display: flex;
  flex-direction: column;
  min-height: 0;
}

.filter {
  padding: 4px 12px 8px;
}

.filter-box {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 28px;
  padding: 0 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
  color: var(--text-subtle);
}

.filter-box:focus-within {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--focus-ring);
}

.filter-box input {
  flex: 1;
  min-width: 0;
  border: 0;
  background: transparent;
  color: var(--text);
  outline: none;
}

.section {
  display: flex;
  align-items: center;
  justify-content: space-between;
  height: var(--row-tree);
  padding: 0 8px 0 12px;
}

.icon-btn.small {
  width: 24px;
  height: 24px;
}

.empty {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0;
  padding: 8px 12px;
  color: var(--text-subtle);
  font-size: 12px;
}

.empty strong {
  color: var(--text);
  font-size: 13px;
  font-weight: 500;
}

.tree {
  position: relative;
  margin: 0;
  padding: 0;
  list-style: none;
}

.row {
  position: absolute;
  right: 0;
  left: 0;
  box-sizing: border-box;
  display: flex;
  align-items: center;
  gap: 6px;
  height: var(--row-tree);
  padding-right: 12px;
  cursor: default;
  user-select: none;
}

.row:hover {
  background: var(--surface-raised);
}

.row:focus {
  outline: none;
}

.row:focus-visible,
.row[aria-selected='true'] {
  background: var(--accent-tint);
  box-shadow: inset 2px 0 0 var(--accent);
}

.row.current .name {
  font-weight: 500;
}

.row.attached {
  color: var(--text-muted);
}

.row.drop-inside,
.section.drop-inside {
  background: var(--accent-tint);
  box-shadow: inset 0 0 0 1px var(--accent);
}

.row.drop-before {
  box-shadow: inset 0 2px 0 var(--accent);
}

.row.drop-after {
  box-shadow: inset 0 -2px 0 var(--accent);
}

.color {
  position: absolute;
  top: 6px;
  bottom: 6px;
  left: 2px;
  width: 3px;
  border-radius: 2px;
}

.chevron {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 16px;
  height: 16px;
  padding: 0;
  border: 0;
  border-radius: 3px;
  background: transparent;
  color: var(--text-subtle);
  cursor: pointer;
}

.chevron-space {
  width: 16px;
  flex-shrink: 0;
}

.kind-icon {
  color: var(--text-subtle);
}

.name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.rename[aria-invalid='true'] {
  border-color: var(--danger);
}

.rename {
  flex: 1;
  min-width: 0;
  height: 22px;
  padding: 0 6px;
  border: 1px solid var(--accent);
  border-radius: var(--radius-control);
  background: var(--surface);
  color: var(--text);
  outline: none;
}

.env-dot {
  flex-shrink: 0;
  width: 8px;
  height: 8px;
  border-radius: 50%;
}

.meta {
  font-size: 11px;
  color: var(--text-subtle);
}

.row.group .name,
.row.status {
  color: var(--text-muted);
  font-size: 12px;
}

.row.system .name {
  color: var(--text-muted);
}

.count {
  color: var(--text-subtle);
}

.detail {
  flex-shrink: 0;
  max-width: 40%;
  overflow: hidden;
  font-family: var(--font-mono);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  color: var(--text-subtle);
  text-overflow: ellipsis;
  white-space: nowrap;
}

.flag {
  flex-shrink: 0;
  padding: 0 4px;
  border: 1px solid var(--border);
  border-radius: 3px;
  font-size: 10px;
  line-height: 14px;
  color: var(--text-subtle);
}

.current-dot {
  flex-shrink: 0;
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--accent);
}

.status-text {
  display: inline-flex;
  flex: 1;
  align-items: center;
  gap: 6px;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.status-text.danger {
  color: var(--danger-text);
}

.status-action {
  flex-shrink: 0;
  height: 20px;
  padding: 0 6px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
  color: var(--text);
  font-size: 11px;
  cursor: pointer;
}

.status-action:hover {
  background: var(--surface-raised);
}
</style>
