<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, ref, useId } from 'vue';
import type { ConnectionProfile } from '../../api/records';
import { useWorkspace } from '../../composables/useWorkspace';
import { pickerFormSchema } from '../../lib/forms';
import EngineTile from '../common/EngineTile.vue';
import EnvBadge from '../common/EnvBadge.vue';
import Modal from '../common/Modal.vue';

defineProps<{ title: string }>();
const emit = defineEmits<{ done: [answer: ConnectionProfile | null] }>();

const workspace = useWorkspace();
const { defineField, errors } = useForm({ validationSchema: toTypedSchema(pickerFormSchema), initialValues: { filter: '' } });
const [filter] = defineField('filter');
const filterId = useId();
const connections = computed(() => {
  const needle = (filter.value ?? '').trim().toLocaleLowerCase();
  return workspace.records.value.connections.filter((c) => c.name.toLocaleLowerCase().includes(needle));
});

const list = ref<HTMLElement | null>(null);
const filterInput = ref<HTMLInputElement | null>(null);

function items(): HTMLButtonElement[] {
  return [...(list.value?.querySelectorAll<HTMLButtonElement>('button.item') ?? [])];
}

/** Enter in the filter picks the only match; with more matches it moves to the first one, like ArrowDown. */
function onFilterEnter(): void {
  const only = connections.value.length === 1 ? connections.value[0] : undefined;
  if (only) emit('done', only);
  else items()[0]?.focus();
}

/** Arrow keys move between the connections; ArrowUp on the first goes back to the filter. */
function onItemKeydown(event: KeyboardEvent): void {
  const all = items();
  const at = all.indexOf(event.currentTarget as HTMLButtonElement);
  if (event.key === 'ArrowDown') all[Math.min(at + 1, all.length - 1)]?.focus();
  else if (event.key === 'ArrowUp') {
    if (at <= 0) filterInput.value?.focus();
    else all[at - 1]?.focus();
  } else if (event.key === 'Home') all[0]?.focus();
  else if (event.key === 'End') all.at(-1)?.focus();
  else return;
  event.preventDefault();
}
</script>

<template>
  <Modal
    :title="title"
    :width="480"
    @close="emit('done', null)"
  >
    <label
      :for="filterId"
      class="visually-hidden"
    >Filter connections</label>
    <input
      :id="filterId"
      ref="filterInput"
      v-model="filter"
      class="input"
      placeholder="Filter connections"
      data-autofocus
      :aria-invalid="!!errors.filter"
      @keydown.enter.prevent="onFilterEnter"
      @keydown.down.prevent="items()[0]?.focus()"
    >
    <p
      v-if="errors.filter"
      class="error"
      role="alert"
    >
      {{ errors.filter }}
    </p>
    <ul
      ref="list"
      class="list"
      aria-label="Connections"
    >
      <li
        v-for="c in connections"
        :key="c.id"
      >
        <button
          type="button"
          class="item"
          @click="emit('done', c)"
          @keydown="onItemKeydown"
        >
          <EngineTile :engine="c.engine" />
          <span class="name">{{ c.name }}</span>
          <EnvBadge :environment="c.environment" />
        </button>
      </li>
      <li
        v-if="connections.length === 0"
        class="subtle empty"
      >
        No connections match.
      </li>
    </ul>
  </Modal>
</template>

<style scoped>
.error {
  margin: 4px 0 0;
  font-size: 12px;
  color: var(--danger-text);
}

.list {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin: 12px 0 0;
  padding: 0;
  list-style: none;
}

.item {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  height: 36px;
  padding: 0 8px;
  border: 0;
  border-radius: var(--radius-control);
  background: transparent;
  text-align: left;
  cursor: pointer;
}

.item:hover,
.item:focus-visible {
  background: var(--accent-tint);
}

.name {
  flex: 1;
}

.empty {
  padding: 8px;
}
</style>
