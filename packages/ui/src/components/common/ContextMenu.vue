<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue';
import type { MenuItem, MenuLeaf } from '../../lib/menu';

/**
 * A context menu at a point, with arrow-key navigation. Closing returns
 * focus to the opener. An entry with `children` opens a submenu (hover,
 * click, Enter or ArrowRight); ArrowLeft or Escape closes it again.
 * The menu grows in from the point it was opened at; submenus and closing
 * are not animated.
 */
const props = defineProps<{ items: MenuItem[]; x: number; y: number; label: string }>();
const emit = defineEmits<{ select: [id: string]; close: [] }>();

const menu = ref<HTMLElement | null>(null);
const position = ref({ left: props.x, top: props.y });
/** Id of the entry whose submenu is open. */
const openSub = ref<string | null>(null);
/** Where the open submenu sits: flipped to the left and moved up when it would leave the window. */
const subPlacement = ref({ flip: false, shift: 0 });
let opener: Element | null = null;

/** The opening point inside the menu, so it grows from where it was asked for even when moved to fit the window. */
const origin = computed(() => `${props.x - position.value.left}px ${props.y - position.value.top}px`);

/** The enabled entries of the (sub)menu that holds `from`, in order. */
function buttons(from: Element | null = document.activeElement): HTMLButtonElement[] {
  const scope = from?.closest('[role="menu"]') ?? menu.value;
  if (!scope) return [];
  return [...scope.querySelectorAll<HTMLButtonElement>(':scope > button.item:not([disabled]), :scope > .sub > button.item:not([disabled])')];
}

function move(delta: number): void {
  const list = buttons();
  if (list.length === 0) return;
  const at = list.indexOf(document.activeElement as HTMLButtonElement);
  const next = list[(at + delta + list.length) % list.length];
  next?.focus();
}

function subButton(id: string): HTMLButtonElement | null {
  return menu.value?.querySelector<HTMLButtonElement>(`button[data-sub="${CSS.escape(id)}"]`) ?? null;
}

/**
 * Right and bottom edge of `el` in the window from layout offsets, not the
 * bounding box: while the menu still grows in, its scale would make the box
 * look smaller than it ends up.
 */
function layoutEdges(el: HTMLElement): { right: number; bottom: number } {
  let left = 0;
  let top = 0;
  let node: HTMLElement | null = el;
  while (node && node !== menu.value) {
    left += node.offsetLeft;
    top += node.offsetTop;
    const parent: HTMLElement | null = node.offsetParent instanceof HTMLElement ? node.offsetParent : null;
    if (parent) {
      left += parent.clientLeft;
      top += parent.clientTop;
    }
    node = parent;
  }
  return { right: position.value.left + left + el.offsetWidth, bottom: position.value.top + top + el.offsetHeight };
}

async function showSub(item: MenuItem, focusFirst: boolean): Promise<void> {
  if (item.disabled || !item.children) return;
  if (openSub.value !== item.id) {
    openSub.value = item.id;
    subPlacement.value = { flip: false, shift: 0 };
    await nextTick();
    const sub = menu.value?.querySelector<HTMLElement>('.submenu');
    if (sub) {
      const edges = layoutEdges(sub);
      subPlacement.value = {
        flip: edges.right > window.innerWidth - 4,
        shift: Math.min(0, window.innerHeight - 4 - edges.bottom),
      };
    }
  }
  if (focusFirst) {
    await nextTick();
    const sub = menu.value?.querySelector<HTMLElement>('.submenu');
    sub?.querySelector<HTMLButtonElement>('button.item:not([disabled])')?.focus();
  }
}

function hideSub(focusParent: boolean): void {
  const id = openSub.value;
  openSub.value = null;
  if (focusParent && id !== null) subButton(id)?.focus();
}

function inSub(): boolean {
  return document.activeElement?.closest('.submenu') != null;
}

function onKeydown(event: KeyboardEvent): void {
  const active = document.activeElement as HTMLElement | null;
  const subId = active?.dataset['sub'];
  if (event.key === 'ArrowDown') move(1);
  else if (event.key === 'ArrowUp') move(-1);
  else if (event.key === 'Home') buttons()[0]?.focus();
  else if (event.key === 'End') buttons().at(-1)?.focus();
  else if (event.key === 'ArrowRight' && subId !== undefined) {
    const item = props.items.find((i) => i.id === subId);
    if (item) void showSub(item, true);
  } else if (event.key === 'ArrowLeft' && inSub()) hideSub(true);
  else if (event.key === 'Escape' && inSub()) hideSub(true);
  else if (event.key === 'Escape' || event.key === 'Tab') emit('close');
  else return;
  event.preventDefault();
}

function onPointerDown(event: PointerEvent): void {
  if (menu.value && !menu.value.contains(event.target as Node)) emit('close');
}

function choose(item: MenuLeaf): void {
  if (item.disabled) return;
  emit('select', item.id);
  emit('close');
}

function chooseTop(item: MenuItem): void {
  if (item.children) {
    if (openSub.value === item.id) hideSub(false);
    else void showSub(item, true);
    return;
  }
  choose(item);
}

/** Pointer moves onto a top-level entry: open its submenu, or close another one. */
function hover(item: MenuItem): void {
  if (item.children) void showSub(item, false);
  else if (openSub.value !== null) openSub.value = null;
}

onMounted(async () => {
  opener = document.activeElement;
  document.addEventListener('pointerdown', onPointerDown, true);
  await nextTick();
  // Layout size, not the bounding box: the opening scale must not shrink the measure.
  const el = menu.value;
  if (el) {
    position.value = {
      left: Math.max(4, Math.min(props.x, window.innerWidth - el.offsetWidth - 4)),
      top: Math.max(4, Math.min(props.y, window.innerHeight - el.offsetHeight - 4)),
    };
  }
  buttons(menu.value)[0]?.focus();
});

onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', onPointerDown, true);
  if (opener instanceof HTMLElement && opener.isConnected) opener.focus();
});
</script>

<template>
  <Teleport to="body">
    <div
      ref="menu"
      class="menu root"
      role="menu"
      :aria-label="label"
      :style="{ left: `${position.left}px`, top: `${position.top}px`, transformOrigin: origin }"
      @keydown="onKeydown"
    >
      <template
        v-for="item in items"
        :key="item.id"
      >
        <div
          v-if="item.separatorBefore"
          class="sep"
          role="separator"
        />
        <div
          v-if="item.children"
          class="sub"
          role="none"
          @mouseenter="hover(item)"
        >
          <button
            type="button"
            role="menuitem"
            class="item has-sub"
            aria-haspopup="menu"
            :aria-expanded="openSub === item.id"
            :data-sub="item.id"
            :disabled="item.disabled"
            @click="chooseTop(item)"
          >
            <span>{{ item.label }}</span>
            <span
              class="chevron"
              aria-hidden="true"
            >›</span>
          </button>
          <div
            v-if="openSub === item.id"
            class="menu submenu"
            :class="{ flip: subPlacement.flip }"
            :style="{ marginTop: `${subPlacement.shift}px` }"
            role="menu"
            :aria-label="item.label"
          >
            <template
              v-for="child in item.children"
              :key="child.id"
            >
              <div
                v-if="child.separatorBefore"
                class="sep"
                role="separator"
              />
              <button
                type="button"
                role="menuitem"
                class="item"
                :class="{ danger: child.danger }"
                :disabled="child.disabled"
                @click="choose(child)"
              >
                {{ child.label }}
              </button>
            </template>
          </div>
        </div>
        <button
          v-else
          type="button"
          role="menuitem"
          class="item"
          :class="{ danger: item.danger }"
          :disabled="item.disabled"
          @click="chooseTop(item)"
          @mouseenter="hover(item)"
        >
          {{ item.label }}
        </button>
      </template>
    </div>
  </Teleport>
</template>

<style scoped>
.menu {
  position: fixed;
  z-index: 200;
  display: flex;
  flex-direction: column;
  min-width: 200px;
  padding: 4px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
  box-shadow: var(--shadow-dialog);
}

.root {
  animation: menu-in 120ms var(--ease-out);
}

@keyframes menu-in {
  from {
    opacity: 0;
    transform: scale(0.98);
  }
}

@keyframes menu-fade-in {
  from {
    opacity: 0;
  }
}

/* Reduced motion: a short fade only, no scaling. */
@media (prefers-reduced-motion: reduce) {
  .root {
    animation-name: menu-fade-in;
  }
}

.sub {
  position: relative;
  display: flex;
  flex-direction: column;
}

.submenu {
  position: absolute;
  top: -5px;
  left: calc(100% + 4px);
}

.submenu.flip {
  right: calc(100% + 4px);
  left: auto;
}

.item {
  height: 28px;
  padding: 0 10px;
  border: 0;
  border-radius: var(--radius-control);
  background: transparent;
  color: var(--text);
  font-size: 13px;
  text-align: left;
  cursor: pointer;
}

.item.has-sub {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

.chevron {
  color: var(--text-muted);
}

.item:hover:not(:disabled),
.item:focus-visible,
.item.has-sub[aria-expanded='true'] {
  background: var(--accent-tint);
  outline: none;
}

.item:disabled {
  color: var(--text-disabled);
  cursor: default;
}

.item.danger {
  color: var(--danger-text);
}

.sep {
  height: 1px;
  margin: 4px 0;
  background: var(--border-inner);
}
</style>
