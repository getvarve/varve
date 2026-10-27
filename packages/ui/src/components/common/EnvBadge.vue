<script setup lang="ts">
import { computed } from 'vue';
import type { Environment } from '../../api/records';
import { ENVIRONMENT_INFO } from '../../lib/display';

const props = defineProps<{ environment: Environment }>();
const info = computed(() => ENVIRONMENT_INFO[props.environment]);
</script>

<template>
  <span
    class="pill"
    :class="`tone-${info.tone}`"
  ><span class="dot" />{{ info.label }}</span>
</template>
