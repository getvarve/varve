<script setup lang="ts">
import { computed } from 'vue';
import type { Environment } from '../../api/records';
import { ENVIRONMENT_INFO, environmentTitle } from '../../lib/display';

/** `tag`: the short word in the environment's colours that ends a connection's row in the sidebar. */
const props = defineProps<{ environment: Environment; tag?: boolean }>();
const info = computed(() => ENVIRONMENT_INFO[props.environment]);
</script>

<template>
  <span
    v-if="tag"
    class="env-tag"
    :class="`tone-${info.tone}`"
    :title="environmentTitle(environment)"
  ><span aria-hidden="true">{{ info.tag }}</span><span class="visually-hidden">{{ info.label }} environment</span></span>
  <span
    v-else
    class="pill"
    :class="`tone-${info.tone}`"
  ><span class="dot" />{{ info.label }}</span>
</template>

<style scoped>
/* Never wraps; when even the shortest name leaves no room, the word ends in an ellipsis (the tooltip has it whole). */
.env-tag {
  flex-shrink: 1;
  min-width: 0;
  height: 18px;
  padding: 0 6px;
  overflow: hidden;
  border-radius: 4px;
  box-sizing: border-box;
  font-size: 10px;
  font-weight: 500;
  letter-spacing: 0.04em;
  line-height: 18px;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
