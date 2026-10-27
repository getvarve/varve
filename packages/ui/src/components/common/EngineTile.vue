<script setup lang="ts">
import { computed } from 'vue';
import type { Engine } from '../../api/records';
import { ENGINE_INFO } from '../../lib/display';

const props = defineProps<{ engine: Engine }>();
const info = computed(() => ENGINE_INFO[props.engine]);
</script>

<template>
  <span
    class="tile"
    :style="{ background: info.color }"
    :title="info.name"
  >{{ info.code }}</span>
</template>

<style scoped>
.tile {
  display: inline-flex;
  flex-shrink: 0;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 18px;
  border-radius: 3px;
  font-family: var(--font-mono);
  font-size: 10px;
  font-weight: 500;
  color: var(--on-engine);
}
</style>
