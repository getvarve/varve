<script setup lang="ts">
import { useId } from 'vue';
import type { Environment } from '../../api/records';
import EnvBadge from '../common/EnvBadge.vue';
import Icon from '../common/Icon.vue';

/**
 * Header, section tabs and body of an object editor (design canvases
 * ViewEditor, FunctionEditor, ...): object name, kind, environment,
 * unsaved-change state, the editor's actions and its sections.
 */
const props = withDefaults(
  defineProps<{
    title: string;
    kindLabel: string;
    environment: Environment;
    /** Secondary text after the badges (owner, table, ...). */
    subtitle?: string;
    /** Section tabs; the body shows the slot named after the active id. */
    tabs: { id: string; label: string; count?: number | null }[];
    readOnly?: boolean;
    /** Text of the unsaved-changes pill, or blank when there are none. */
    dirtyText?: string;
    loading?: boolean;
    /** Load failure; the body shows it with a Retry button. */
    loadError?: string | null;
  }>(),
  { subtitle: '', readOnly: false, dirtyText: '', loading: false, loadError: null },
);
const active = defineModel<string>('active', { required: true });
const emit = defineEmits<{ reload: [] }>();
const tablistId = useId();

function tabId(id: string): string {
  return `${tablistId}-${id}`;
}

function onKey(event: KeyboardEvent, index: number): void {
  const step = event.key === 'ArrowRight' ? 1 : event.key === 'ArrowLeft' ? -1 : 0;
  if (step === 0) return;
  event.preventDefault();
  const next = props.tabs[(index + step + props.tabs.length) % props.tabs.length];
  if (next) {
    active.value = next.id;
    document.getElementById(tabId(next.id))?.focus();
  }
}
</script>

<template>
  <div class="frame">
    <header class="header">
      <h1 class="mono">
        {{ title }}
      </h1>
      <span class="kind">{{ kindLabel }}</span>
      <EnvBadge :environment="environment" />
      <span
        v-if="readOnly"
        class="pill tone-neutral"
      ><Icon name="lock" />Read only</span>
      <slot name="badges" />
      <span
        v-if="subtitle"
        class="subtitle"
      >{{ subtitle }}</span>
      <span class="grow" />
      <span
        v-if="dirtyText"
        class="pill tone-warning"
      ><span class="dot" />{{ dirtyText }}</span>
      <slot name="actions" />
    </header>
    <div
      role="tablist"
      :aria-label="`${kindLabel} sections`"
      class="tabs"
    >
      <button
        v-for="(tab, index) in tabs"
        :id="tabId(tab.id)"
        :key="tab.id"
        type="button"
        role="tab"
        :aria-selected="active === tab.id"
        :aria-controls="`${tabId(tab.id)}-panel`"
        :tabindex="active === tab.id ? 0 : -1"
        @click="active = tab.id"
        @keydown="onKey($event, index)"
      >
        {{ tab.label }}<span
          v-if="tab.count !== undefined && tab.count !== null"
          class="count"
        >{{ tab.count }}</span>
      </button>
    </div>
    <div
      :id="`${tabId(active)}-panel`"
      role="tabpanel"
      :aria-labelledby="tabId(active)"
      class="body"
    >
      <p
        v-if="loading"
        class="muted state"
        role="status"
      >
        Loading
      </p>
      <div
        v-else-if="loadError"
        class="notice tone-danger state"
        role="alert"
      >
        <Icon name="warning" /><span class="grow">{{ loadError }}</span>
        <button
          type="button"
          class="btn compact"
          @click="emit('reload')"
        >
          Retry
        </button>
      </div>
      <slot
        v-else
        :name="active"
      />
    </div>
  </div>
</template>

<style scoped>
.frame {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
  background: var(--ground);
}

.header {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px;
  min-height: 48px;
  padding: 8px 16px;
  border-bottom: 1px solid var(--border-inner);
  background: var(--surface);
}

h1 {
  margin: 0;
  font-size: 15px;
  font-weight: 500;
  overflow-wrap: anywhere;
}

.kind,
.subtitle {
  font-size: 12px;
  color: var(--text-muted);
}

.grow {
  flex-grow: 1;
}

.tabs {
  display: flex;
  gap: 2px;
  height: 36px;
  padding: 0 12px;
  border-bottom: 1px solid var(--border);
  background: var(--surface);
  overflow-x: auto;
}

.tabs button {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 35px;
  padding: 0 12px;
  border: 0;
  background: transparent;
  color: var(--text-muted);
  font-size: 13px;
  white-space: nowrap;
  cursor: pointer;
}

.tabs button[aria-selected='true'] {
  color: var(--text);
  font-weight: 500;
  box-shadow: inset 0 -2px 0 var(--accent);
}

.tabs button:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: -2px;
}

.count {
  padding: 0 6px;
  border-radius: 9px;
  background: var(--surface-raised);
  color: var(--text-muted);
  font-size: 11px;
}

.body {
  flex-grow: 1;
  min-height: 0;
  overflow: auto;
}

.state {
  margin: 16px;
}
</style>
