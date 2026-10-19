<script setup lang="ts">
import { computed } from 'vue';

/** The Year rings mark. Uses the 2-ring variant up to 32px, as the logo guide says. */
const props = withDefaults(defineProps<{ size?: number; label?: string }>(), { size: 20, label: 'Varve' });
const small = computed(() => props.size <= 32);
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="56 56 400 400"
    role="img"
    :aria-label="label"
  >
    <g
      v-if="small"
      fill="none"
      stroke-linecap="round"
      transform="rotate(-62 256 256)"
    >
      <circle
        cx="256"
        cy="256"
        r="168"
        stroke="var(--ring-1)"
        stroke-width="60"
        stroke-dasharray="880 1056"
      />
      <circle
        cx="256"
        cy="256"
        r="96"
        stroke="var(--ring-2)"
        stroke-width="44"
        stroke-dasharray="503 604"
      />
    </g>
    <circle
      v-if="small"
      cx="256"
      cy="256"
      r="30"
      fill="var(--ring-3)"
    />
    <g
      v-if="!small"
      fill="none"
      stroke-linecap="round"
      transform="rotate(-62 256 256)"
    >
      <circle
        cx="256"
        cy="256"
        r="172"
        stroke="var(--ring-1)"
        stroke-width="44"
        stroke-dasharray="900 1081"
      />
      <circle
        cx="256"
        cy="256"
        r="118"
        stroke="var(--ring-2)"
        stroke-width="30"
        stroke-dasharray="618 741"
      />
      <circle
        cx="256"
        cy="256"
        r="76"
        stroke="var(--ring-3)"
        stroke-width="20"
        stroke-dasharray="398 478"
      />
    </g>
    <circle
      v-if="!small"
      cx="256"
      cy="256"
      r="28"
      fill="var(--text-subtle)"
    />
  </svg>
</template>
