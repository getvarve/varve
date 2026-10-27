<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, useId } from 'vue';
import Icon from './Icon.vue';

/**
 * A modal dialog: focus moves into it, Tab and Shift+Tab stay inside it,
 * Escape closes it, and focus returns to where it was when it closes.
 */
withDefaults(
  defineProps<{ title: string; width?: number; role?: 'dialog' | 'alertdialog'; describedBy?: string }>(),
  { width: 560, role: 'dialog', describedBy: '' },
);
const emit = defineEmits<{ close: [] }>();

const titleId = useId();
const panel = ref<HTMLElement | null>(null);
let previous: Element | null = null;

const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

function focusables(): HTMLElement[] {
  if (!panel.value) return [];
  return [...panel.value.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.offsetParent !== null || el === document.activeElement);
}

function onKeydown(event: KeyboardEvent): void {
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

onMounted(async () => {
  previous = document.activeElement;
  await nextTick();
  const auto = panel.value?.querySelector<HTMLElement>('[autofocus], [data-autofocus]');
  (auto ?? focusables()[0] ?? panel.value)?.focus();
});

onBeforeUnmount(() => {
  if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
});

</script>

<template>
  <Teleport to="body">
    <div class="backdrop">
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
      >
        <header class="head">
          <slot name="lead" />
          <h2
            :id="titleId"
            class="title"
          >
            {{ title }}
          </h2>
          <slot name="badges" />
          <span class="spacer" />
          <button
            type="button"
            class="icon-btn"
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
}

.panel {
  display: flex;
  flex-direction: column;
  max-width: 100%;
  max-height: 100%;
  border-radius: var(--radius-dialog);
  background: var(--surface);
  box-shadow: var(--shadow-dialog);
  outline: none;
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
