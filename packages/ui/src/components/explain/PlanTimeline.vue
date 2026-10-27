<script setup lang="ts">
import { computed } from 'vue';
import type { Plan, PlanNode } from '../../api/explain';
import { flattenPlan, formatMs, formatNumber, heatOf, nodeSubject, nodeTitle, planTimeline } from '../../lib/explainPlan';

/**
 * The plan as a timeline: each node is a bar as wide as its inclusive time
 * (or cost, when the statement did not run), nested under its parent, so
 * where the time goes reads left to right. Emits `select` with the node id.
 */
const props = defineProps<{ plan: Plan; selected: number | null }>();
const emit = defineEmits<{ select: [id: number] }>();

const ROW = 26;
const timeline = computed(() => planTimeline(props.plan));
const nodes = computed(() => new Map(flattenPlan(props.plan.root).map((f) => [f.node.id, f.node])));

function nodeOf(id: number): PlanNode {
  return nodes.value.get(id) as PlanNode;
}

function amount(value: number): string {
  return timeline.value?.basis === 'time' ? formatMs(value) : `cost ${formatNumber(value, 2)}`;
}

function label(id: number): string {
  const n = nodeOf(id);
  const subject = nodeSubject(n);
  return subject ? `${nodeTitle(n)} ${subject}` : nodeTitle(n);
}

const ticks = computed(() => {
  const t = timeline.value;
  if (!t) return [];
  return [0, 0.25, 0.5, 0.75, 1].map((f) => ({ at: f, text: amount(t.total * f) }));
});

function onKey(event: KeyboardEvent, id: number): void {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    emit('select', id);
  }
}
</script>

<template>
  <div class="timeline">
    <p
      v-if="!timeline"
      class="empty"
    >
      This plan has neither times nor costs, so there is no timeline. The server reports them with ANALYZE.
    </p>
    <template v-else>
      <div
        class="axis mono"
        aria-hidden="true"
      >
        <span
          v-for="t in ticks"
          :key="t.at"
          :style="{ left: `${t.at * 100}%` }"
        >{{ t.text }}</span>
      </div>
      <div
        class="rows"
        :style="{ height: `${timeline.depth * ROW}px` }"
        role="list"
        :aria-label="timeline.basis === 'time' ? 'Inclusive time per node' : 'Inclusive cost per node'"
      >
        <div
          v-for="b in timeline.bars"
          :key="b.id"
          role="listitem"
        >
          <div
            role="button"
            tabindex="0"
            class="bar"
            :class="[`heat-${heatOf(nodeOf(b.id).share)}`, { selected: selected === b.id }]"
            :style="{ left: `${b.start * 100}%`, width: `max(${b.width * 100}%, 3px)`, top: `${b.depth * ROW}px`, height: `${ROW - 4}px` }"
            :aria-pressed="selected === b.id"
            :aria-label="`${label(b.id)}: ${amount(b.value)} including children`"
            :title="`${label(b.id)}\n${amount(b.value)} including children`"
            @click="emit('select', b.id)"
            @keydown="onKey($event, b.id)"
          >
            <span class="text">{{ nodeOf(b.id).operation }} <span class="mono">{{ amount(b.value) }}</span></span>
          </div>
        </div>
      </div>
      <p class="note">
        Bar width is the node's {{ timeline.basis === 'time' ? 'time' : 'estimated cost' }} including its children; color is its own share of the plan.
      </p>
    </template>
  </div>
</template>

<style scoped>
.timeline {
  flex: 1;
  min-width: 0;
  padding: 12px 24px 24px;
  overflow: auto;
  background: var(--ground);
}

.axis {
  position: relative;
  height: 20px;
  margin: 0 40px 4px 0;
  color: var(--text-subtle);
  font-size: 10px;
}

.axis span {
  position: absolute;
  transform: translateX(-50%);
  white-space: nowrap;
}

.axis span:first-child {
  transform: none;
}

.rows {
  position: relative;
  margin-right: 40px;
}

.bar {
  position: absolute;
  display: flex;
  align-items: center;
  overflow: hidden;
  padding: 0 6px;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--surface);
  font-size: 11px;
  white-space: nowrap;
  cursor: pointer;
}

.bar .text {
  overflow: hidden;
  text-overflow: ellipsis;
}

.bar.heat-hot {
  border-color: var(--danger);
  background: var(--danger-tint);
  color: var(--danger-text);
}

.bar.heat-warm {
  border-color: var(--warning);
  background: var(--warning-tint);
  color: var(--warning-text);
}

.bar.selected {
  border-color: var(--accent);
  box-shadow: 0 0 0 2px var(--focus-ring);
}

.note,
.empty {
  margin: 12px 0 0;
  color: var(--text-subtle);
  font-size: 12px;
}
</style>
