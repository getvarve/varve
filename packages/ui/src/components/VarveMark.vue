<script setup lang="ts">
import { computed } from 'vue';

/**
 * The Year rings mark. Uses the 2-ring variant up to 32px, as the logo guide says.
 * With `draw`, the rings draw themselves in (600ms) when the mark mounts.
 */
const props = withDefaults(defineProps<{ size?: number; label?: string; draw?: boolean }>(), {
  size: 20,
  label: 'Varve',
  draw: false,
});
const small = computed(() => props.size <= 32);
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="56 56 400 400"
    role="img"
    :aria-label="label"
    :class="{ draw }"
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
        class="ring ring-880"
      />
      <circle
        cx="256"
        cy="256"
        r="96"
        stroke="var(--ring-2)"
        stroke-width="44"
        stroke-dasharray="503 604"
        class="ring ring-503"
      />
    </g>
    <circle
      v-if="small"
      cx="256"
      cy="256"
      r="30"
      fill="var(--ring-3)"
      class="core"
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
        class="ring ring-900"
      />
      <circle
        cx="256"
        cy="256"
        r="118"
        stroke="var(--ring-2)"
        stroke-width="30"
        stroke-dasharray="618 741"
        class="ring ring-618"
      />
      <circle
        cx="256"
        cy="256"
        r="76"
        stroke="var(--ring-3)"
        stroke-width="20"
        stroke-dasharray="398 478"
        class="ring ring-398"
      />
    </g>
    <circle
      v-if="!small"
      cx="256"
      cy="256"
      r="28"
      fill="var(--text-subtle)"
      class="core"
    />
  </svg>
</template>

<style scoped>
/*
 * Drawing in: each ring's dash grows from its start (stroke-dashoffset from
 * the dash length to 0) while it fades in, so the round cap of the empty
 * dash never shows as a dot.
 */
.draw .ring,
.draw .core {
  animation-duration: 600ms;
  animation-timing-function: var(--ease-out);
}

.draw .core {
  animation-name: mark-fade-in;
}

.draw .ring-880 {
  animation-name: ring-880;
}

.draw .ring-503 {
  animation-name: ring-503;
}

.draw .ring-900 {
  animation-name: ring-900;
}

.draw .ring-618 {
  animation-name: ring-618;
}

.draw .ring-398 {
  animation-name: ring-398;
}

@keyframes ring-880 {
  from {
    stroke-dashoffset: 880;
    opacity: 0;
  }

  20% {
    opacity: 1;
  }
}

@keyframes ring-503 {
  from {
    stroke-dashoffset: 503;
    opacity: 0;
  }

  20% {
    opacity: 1;
  }
}

@keyframes ring-900 {
  from {
    stroke-dashoffset: 900;
    opacity: 0;
  }

  20% {
    opacity: 1;
  }
}

@keyframes ring-618 {
  from {
    stroke-dashoffset: 618;
    opacity: 0;
  }

  20% {
    opacity: 1;
  }
}

@keyframes ring-398 {
  from {
    stroke-dashoffset: 398;
    opacity: 0;
  }

  20% {
    opacity: 1;
  }
}

@keyframes mark-fade-in {
  from {
    opacity: 0;
  }
}

/* Reduced motion: the whole mark fades in briefly, the rings do not draw. */
@media (prefers-reduced-motion: reduce) {
  .draw .ring,
  .draw .core {
    animation-name: mark-fade-in;
    animation-duration: var(--dur-fast);
  }
}
</style>
