<script lang="ts">
/** An open modal, as the key handling shared by all modals sees it. */
interface OpenModal {
  /** Whether it is on screen: a dialog of a hidden tab is open but not shown. */
  shown: () => boolean;
  /** Handles a key whose target is not inside this modal, while it is the topmost one on screen. */
  keyOutside: (event: KeyboardEvent) => void;
}

/** The open modals in the order they opened, which is also their order in the body: the last one shown is on top. */
const openModals: OpenModal[] = [];

function topModal(): OpenModal | null {
  for (let i = openModals.length - 1; i >= 0; i -= 1) {
    const modal = openModals[i];
    if (modal?.shown()) return modal;
  }
  return null;
}

/** One listener for all modals (capture, so it runs before whatever has the focus): only the topmost one gets the key. */
function onDocumentKeydown(event: KeyboardEvent): void {
  topModal()?.keyOutside(event);
}

function register(modal: OpenModal): void {
  if (openModals.length === 0) document.addEventListener('keydown', onDocumentKeydown, true);
  openModals.push(modal);
}

function unregister(modal: OpenModal): void {
  const at = openModals.indexOf(modal);
  if (at >= 0) openModals.splice(at, 1);
  if (openModals.length === 0) document.removeEventListener('keydown', onDocumentKeydown, true);
}
</script>

<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, useId, watch } from 'vue';
import { useTabVisible } from '../../composables/useTabs';
import { leaveCopy } from '../../lib/motion';
import { isCut } from '../../lib/tips';
import Icon from './Icon.vue';

/**
 * A modal dialog: focus moves into it, Tab and Shift+Tab stay inside it,
 * Escape closes it, and focus returns to where it was when it closes.
 * It fades and scales in; on close an inert copy fades out (120ms, opacity
 * only) while focus already moves back, so closing never waits.
 *
 * Keys work for the topmost dialog wherever the focus is: Escape and Tab are
 * also taken at the document when the focus is outside the panel (on the
 * body after a click on the backdrop, or after the focused control was
 * removed), other keys aimed at something behind the dialog are stopped, and
 * the focus is pulled back into the panel. With several dialogs open only
 * the top one reacts.
 *
 * A dialog inside a tab's view is hidden while that tab is hidden
 * (`useTabVisible`): it keeps its state, but is not on screen over another
 * tab or page, holds no focus and takes no keys until its tab is shown again.
 */
const props = withDefaults(
  defineProps<{ title: string; width?: number; role?: 'dialog' | 'alertdialog'; describedBy?: string }>(),
  { width: 560, role: 'dialog', describedBy: '' },
);
const emit = defineEmits<{ close: [] }>();

const titleId = useId();
const backdrop = ref<HTMLElement | null>(null);
const panel = ref<HTMLElement | null>(null);
/** False while the tab that owns this dialog is hidden; always true outside a tab's view. */
const tabVisible = useTabVisible();

const titleEl = ref<HTMLElement | null>(null);
/** Whether the title is cut with an ellipsis: only then does it get its full text as a tooltip. */
const titleCut = ref(false);
let titleSize: ResizeObserver | null = null;

function measureTitle(): void {
  titleCut.value = titleEl.value !== null && isCut(titleEl.value);
}

/** Length of the closing fade, in ms. */
const CLOSE_MS = 120;
let previous: Element | null = null;
/** The control of this dialog that had the focus last, to go back to when the focus escaped or the dialog is shown again. */
let lastFocused: HTMLElement | null = null;

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function focusables(): HTMLElement[] {
  if (!panel.value) return [];
  return [...panel.value.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.offsetParent !== null || el === document.activeElement);
}

const entry: OpenModal = { shown: () => tabVisible.value, keyOutside };

function isTop(): boolean {
  return topModal() === entry;
}

/**
 * Whether a key target or focused element is this dialog's: inside the
 * panel, or in something that opened above the dialog (a context menu or a
 * later dialog is teleported to the body after this one) and handles its own
 * keys and focus.
 */
function belongs(node: EventTarget | null): boolean {
  const el = panel.value;
  const back = backdrop.value;
  if (!el || !back || !(node instanceof Node)) return false;
  if (el.contains(node)) return true;
  return !back.contains(node) && (back.compareDocumentPosition(node) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
}

function stop(event: KeyboardEvent): void {
  event.preventDefault();
  event.stopPropagation();
}

/** A key pressed while this is the top dialog; keys from inside the panel are left to `onKeydown`. */
function keyOutside(event: KeyboardEvent): void {
  if (belongs(event.target)) return;
  if (event.key === 'Escape') {
    stop(event);
    emit('close');
    return;
  }
  if (event.key === 'Tab') {
    stop(event);
    const items = focusables();
    ((event.shiftKey ? items[items.length - 1] : items[0]) ?? panel.value)?.focus();
    return;
  }
  // With the focus on nothing (the body), shortcuts and bare modifier keys pass as they do from inside the dialog.
  const onNothing = event.target === document.body || event.target === document.documentElement;
  if (onNothing && (event.metaKey || event.ctrlKey || event.altKey || ['Meta', 'Control', 'Alt', 'Shift'].includes(event.key))) return;
  // Anything else was aimed at something behind the dialog, or would be typed into the control that gets the focus now.
  stop(event);
  focusBack();
}

function onKeydown(event: KeyboardEvent): void {
  if (!isTop()) return;
  if (event.key === 'Escape') {
    event.stopPropagation();
    emit('close');
    return;
  }
  if (event.key !== 'Tab') return;
  const items = focusables();
  if (items.length === 0) {
    event.preventDefault();
    panel.value?.focus();
    return;
  }
  const first = items[0] as HTMLElement;
  const last = items[items.length - 1] as HTMLElement;
  const current = document.activeElement;
  if (event.shiftKey && (current === first || !panel.value?.contains(current))) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && current === last) {
    event.preventDefault();
    first.focus();
  }
}

/**
 * Focuses the `[data-autofocus]` element, otherwise the first control of the
 * body or footer (the header's Close button only when there is nothing
 * else), otherwise the panel. Runs on open; a dialog whose content changes
 * (a new primary action) calls it again through its ref. While the dialog is
 * hidden with its tab it does nothing: the focus moves in when it is shown.
 */
function focusInitial(): void {
  if (!tabVisible.value) return;
  const auto = panel.value?.querySelector<HTMLElement>('[autofocus], [data-autofocus]');
  const items = focusables();
  const content = items.find((el) => !el.closest('.head'));
  (auto ?? content ?? items[0] ?? panel.value)?.focus();
}

/** Puts the focus back into the dialog: on the control that had it last when that is still there, else as on open. */
function focusBack(): void {
  if (lastFocused && focusables().includes(lastFocused)) lastFocused.focus();
  else focusInitial();
}

function onFocusin(event: FocusEvent): void {
  if (event.target instanceof HTMLElement && event.target !== panel.value) lastFocused = event.target;
}

/** The focus left the panel (a click on the backdrop, a control that was disabled): the top dialog takes it back. */
function onFocusout(event: FocusEvent): void {
  if (belongs(event.relatedTarget)) return;
  // The browser finishes moving the focus after this event: look where it went once it has.
  setTimeout(() => {
    if (isTop() && !belongs(document.activeElement)) focusBack();
  }, 0);
}

defineExpose({ focusInitial });

onMounted(async () => {
  previous = document.activeElement;
  register(entry);
  measureTitle();
  if (typeof ResizeObserver === 'function' && titleEl.value) {
    titleSize = new ResizeObserver(measureTitle);
    titleSize.observe(titleEl.value);
  }
  await nextTick();
  focusInitial();
});

// After the new title is in the page.
watch(() => props.title, measureTitle, { flush: 'post' });

watch(
  tabVisible,
  (shown) => {
    if (shown) {
      // Back on screen: the focus returns to where it was in the dialog, unless a dialog above it has it.
      // After this flush, in which `v-show` puts the dialog back: a hidden control cannot take the focus.
      void nextTick(() => {
        if (tabVisible.value && isTop()) focusBack();
      });
      return;
    }
    // Hidden with its tab: the focus must not stay on a control nobody can see.
    const active = document.activeElement;
    if (active instanceof HTMLElement && panel.value?.contains(active)) active.blur();
  },
  { flush: 'post' },
);

onBeforeUnmount(() => {
  titleSize?.disconnect();
  titleSize = null;
  const wasTop = isTop();
  unregister(entry);
  // A dialog that closes while it is hidden, or under another dialog, leaves the screen and the focus alone.
  if (!wasTop) return;
  if (backdrop.value) leaveCopy(backdrop.value, CLOSE_MS, (copy) => document.body.append(copy));
  if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
});

</script>

<template>
  <Teleport to="body">
    <div
      v-show="tabVisible"
      ref="backdrop"
      class="backdrop"
    >
      <div
        ref="panel"
        class="panel"
        :role="role"
        aria-modal="true"
        :aria-labelledby="titleId"
        :aria-describedby="describedBy || undefined"
        tabindex="-1"
        :style="{ width: `${width}px` }"
        @keydown="onKeydown"
        @focusin="onFocusin"
        @focusout="onFocusout"
      >
        <header class="head">
          <slot name="lead" />
          <h2
            :id="titleId"
            ref="titleEl"
            class="title"
            :title="titleCut ? title : undefined"
          >
            {{ title }}
          </h2>
          <slot name="badges" />
          <span class="spacer" />
          <button
            type="button"
            class="icon-btn"
            title="Close (Esc)"
            aria-label="Close"
            @click="emit('close')"
          >
            <Icon name="close" />
          </button>
        </header>
        <slot name="tabs" />
        <div class="body">
          <slot />
        </div>
        <footer
          v-if="$slots.footer"
          class="foot"
        >
          <slot name="footer" />
        </footer>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.backdrop {
  position: fixed;
  inset: 0;
  z-index: 100;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
  background: var(--backdrop);
  animation: backdrop-in var(--dur-fast) var(--ease-out);
}

.panel {
  display: flex;
  flex-direction: column;
  max-width: 100%;
  max-height: 100%;
  border-radius: var(--radius-dialog);
  background: var(--overlay);
  box-shadow: var(--shadow-dialog), var(--glass-edge);
  /* Frosted over glass (glass.css); --overlay-blur is none without it. */
  -webkit-backdrop-filter: var(--overlay-blur);
  backdrop-filter: var(--overlay-blur);
  outline: none;
  animation: panel-in 200ms var(--ease-out);
}

@keyframes backdrop-in {
  from {
    opacity: 0;
  }
}

@keyframes panel-in {
  from {
    opacity: 0;
    transform: scale(0.97);
  }
}

@keyframes panel-fade-in {
  from {
    opacity: 0;
  }
}

/* Reduced motion: the panel fades in without scaling. */
@media (prefers-reduced-motion: reduce) {
  .panel {
    animation-name: panel-fade-in;
  }
}

.head {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 10px;
  height: 56px;
  padding: 0 12px 0 20px;
  border-bottom: 1px solid var(--border-inner);
}

.title {
  margin: 0;
  overflow: hidden;
  font-size: 15px;
  font-weight: 600;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.spacer {
  flex: 1;
}

.body {
  flex: 1;
  min-height: 0;
  padding: 20px;
  overflow: auto;
}

.foot {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 20px;
  border-top: 1px solid var(--border-inner);
}
</style>
