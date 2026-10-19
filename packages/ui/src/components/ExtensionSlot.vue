<script setup lang="ts">
import { computed, inject } from 'vue';
import type { SlotName } from '../extensions';
import { REGISTRY_KEY } from '../keys';

const props = defineProps<{ name: SlotName }>();
const registry = inject(REGISTRY_KEY);
const items = computed(() => registry?.items(props.name) ?? []);
</script>

<template>
  <component
    :is="item.component"
    v-for="item in items"
    :key="item.id"
    :data-slot-item="item.id"
  />
</template>
