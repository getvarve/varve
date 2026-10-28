<script setup lang="ts">
import { computed } from 'vue';

/** Stroke icons on a 24 grid, decorative unless a label is given. */
const PATHS = {
  chevronDown: 'M6 9l6 6 6-6',
  chevronRight: 'M9 6l6 6-6 6',
  folder: 'M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z',
  folderPlus: 'M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2zM12 11v5M9.5 13.5h5',
  file: 'M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9zM14 3v6h6M8 13h8M8 17h5',
  close: 'M6 6l12 12M18 6L6 18',
  search: 'M11 4a7 7 0 1 0 0 14a7 7 0 1 0 0-14M20 20l-4-4',
  play: 'M7 4l13 8-13 8z',
  stop: 'M6 6h12v12H6z',
  plus: 'M12 5v14M5 12h14',
  refresh: 'M20 11a8 8 0 0 0-14.9-3M4 5v4h4M4 13a8 8 0 0 0 14.9 3M20 19v-4h-4',
  history: 'M3 12a9 9 0 1 0 3-6.7M3 4v5h5M12 7v5l3 2',
  save: 'M5 3h11l3 3v13a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2zM7 3v5h8M7 21v-7h10v7',
  download: 'M12 3v12M7 10l5 5 5-5M4 21h16',
  lock: 'M5 11h14v10H5zM8 11V7a4 4 0 0 1 8 0v4',
  key: 'M3 15a4 4 0 1 0 8 0a4 4 0 1 0-8 0M10 12l10-10M16 6l3 3M13 9l2 2',
  shield: 'M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z',
  warning: 'M12 3l10 18H2zM12 10v5M12 18v.5',
  info: 'M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18M12 11v6M12 7.5v.5',
  check: 'M5 12l5 5 9-11',
  eye: 'M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12zM12 9a3 3 0 1 0 0 6a3 3 0 1 0 0-6',
  eyeOff: 'M3 3l18 18M10.6 6.1A10 10 0 0 1 12 6c6 0 10 6 10 6a17 17 0 0 1-3 3.6M6.6 6.6C3.8 8.4 2 12 2 12s4 7 10 7a9 9 0 0 0 4.3-1.1',
  copy: 'M9 9h11v11H9zM5 15H4V4h11v1',
  activity: 'M3 12h4l3-8 4 16 3-8h4',
  settings: 'M12 9a3 3 0 1 0 0 6a3 3 0 1 0 0-6M19 12a7 7 0 0 0-.1-1.2l2-1.6-2-3.4-2.4 1a7 7 0 0 0-2-1.2L14 3h-4l-.5 2.6a7 7 0 0 0-2 1.2l-2.4-1-2 3.4 2 1.6A7 7 0 0 0 5 12a7 7 0 0 0 .1 1.2l-2 1.6 2 3.4 2.4-1a7 7 0 0 0 2 1.2L10 21h4l.5-2.6a7 7 0 0 0 2-1.2l2.4 1 2-3.4-2-1.6A7 7 0 0 0 19 12z',
  home: 'M3 11l9-8 9 8M5 9v12h14V9',
  terminal: 'M4 17l6-5-6-5M12 19h8',
  database: 'M4 6c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3zM4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3',
  more: 'M5 12h.01M12 12h.01M19 12h.01',
} as const;

export type IconName = keyof typeof PATHS;

const props = withDefaults(defineProps<{ name: IconName; size?: number; label?: string }>(), { size: 14, label: '' });
const d = computed(() => PATHS[props.name]);
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
    :role="label ? 'img' : undefined"
    :aria-label="label || undefined"
    :aria-hidden="label ? undefined : 'true'"
    focusable="false"
    class="icon"
  >
    <path :d="d" />
  </svg>
</template>

<style scoped>
.icon {
  flex-shrink: 0;
}
</style>
