<script setup lang="ts">
import { computed } from 'vue';
import { formatTime } from '../../lib/display';
import { REFRESH_CHOICES } from '../../lib/liveRefresh';
import type { RefreshState } from '../../lib/liveRefresh';
import Icon from '../common/Icon.vue';

/**
 * Auto refresh interval, pause and "refresh now" for a live panel (design:
 * PgActivity toolbar). `choices` limits the offered intervals.
 */
const props = withDefaults(defineProps<{ state: RefreshState; choices?: readonly { label: string; ms: number | null }[] }>(), {
  choices: () => REFRESH_CHOICES,
});
const emit = defineEmits<{ interval: [ms: number | null]; pause: []; resume: []; refresh: [] }>();

const updated = computed(() => (props.state.lastAt === null ? 'Not loaded yet' : `Updated ${formatTime(props.state.lastAt)}`));
const canPause = computed(() => props.state.intervalMs !== null);
</script>

<template>
  <div class="refresh">
    <span
      class="caption"
      aria-hidden="true"
    >Auto refresh</span>
    <div
      class="segmented"
      role="group"
      aria-label="Auto refresh interval"
    >
      <button
        v-for="c in choices"
        :key="c.label"
        type="button"
        :aria-pressed="state.intervalMs === c.ms"
        @click="emit('interval', c.ms)"
      >
        {{ c.label }}
      </button>
    </div>
    <button
      type="button"
      class="icon-btn"
      :disabled="!canPause"
      :aria-label="state.paused ? 'Resume auto refresh' : 'Pause auto refresh'"
      :aria-pressed="state.paused"
      :title="state.paused ? 'Resume auto refresh' : 'Pause auto refresh'"
      @click="state.paused ? emit('resume') : emit('pause')"
    >
      <Icon
        v-if="state.paused"
        name="play"
      />
      <svg
        v-else
        width="14"
        height="14"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        stroke-linecap="round"
        aria-hidden="true"
        focusable="false"
      ><path d="M9 5v14M15 5v14" /></svg>
    </button>
    <button
      type="button"
      class="icon-btn"
      aria-label="Refresh now"
      title="Refresh now"
      :disabled="state.loading"
      @click="emit('refresh')"
    >
      <Icon
        name="refresh"
        :class="{ spin: state.loading }"
      />
    </button>
    <span
      class="updated mono"
      :class="{ failed: state.failed }"
      aria-live="polite"
    >{{ state.paused ? 'Paused' : state.failed ? 'Last refresh failed' : updated }}</span>
  </div>
</template>

<style scoped>
.refresh {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12px;
}

.caption {
  color: var(--text-muted);
}

.segmented button {
  height: 26px;
  padding: 0 8px;
  font-size: 12px;
}

.updated {
  min-width: 64px;
  color: var(--text-subtle);
  font-size: 12px;
}

.updated.failed {
  color: var(--danger-text);
}

.spin {
  animation: spin 900ms linear infinite;
}

@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}

@media (prefers-reduced-motion: reduce) {
  .spin {
    animation: none;
  }
}
</style>
