<script setup lang="ts">
import { toTypedSchema } from '@vee-validate/zod';
import { useForm } from 'vee-validate';
import { computed, useId } from 'vue';
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
      v-model="filter"
      class="input"
      placeholder="Filter connections"
      data-autofocus
      :aria-invalid="!!errors.filter"
    >
    <p
      v-if="errors.filter"
      class="error"
      role="alert"
    >
      {{ errors.filter }}
    </p>
    <ul
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
