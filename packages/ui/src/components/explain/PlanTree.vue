<script setup lang="ts">
import { computed, ref } from 'vue';
import type { Plan, PlanNode } from '../../api/explain';
import {
  CARD_HEIGHT,
  CARD_WIDTH,
  HIGHLIGHT_LABELS,
  flattenPlan,
  formatRows,
  heatOf,
  highlightValues,
  layoutPlan,
  misestimateFactor,
  nodeSelfText,
  nodeSubject,
  nodeTitle,
  percentText,
} from '../../lib/explainPlan';
import type { HighlightMode } from '../../lib/explainPlan';

/**
 * The visual plan tree (design: ExplainPlan): one card per node with its
 * self time or cost, a hot-spot bar and estimated against actual rows.
 * Emits `select` with the node id; `selected` marks the chosen card.
 */
const props = defineProps<{ plan: Plan; selected: number | null; highlight: HighlightMode }>();
const emit = defineEmits<{ select: [id: number] }>();

const layout = computed(() => layoutPlan(props.plan.root));
const nodes = computed(() => new Map(flattenPlan(props.plan.root).map((f) => [f.node.id, f.node])));
const values = computed(() => highlightValues(props.plan.root, props.highlight));
const hotEdges = computed(() => new Set(layout.value.edges.filter((e) => heatOf(values.value.get(e.to) ?? null) === 'hot').map((e) => e.to)));

const zoom = ref(1);
const viewport = ref<HTMLElement | null>(null);

function zoomBy(step: number): void {
  zoom.value = Math.min(1.5, Math.max(0.3, Math.round((zoom.value + step) * 10) / 10));
}

function fit(): void {
  const el = viewport.value;
  if (!el) return;
  const scale = Math.min(1, el.clientWidth / layout.value.width, el.clientHeight / layout.value.height);
  zoom.value = Math.max(0.3, Math.floor(scale * 100) / 100);
}

function cardOf(id: number): PlanNode {
  return nodes.value.get(id) as PlanNode;
}

function rowsLine(node: PlanNode): string {
  const est = node.estimatedRows === null ? '?' : formatRows(node.estimatedRows);
  if (!props.plan.analyzed) return `rows ${est} est`;
  const act = node.neverExecuted ? 'never' : node.actualRows === null ? '?' : formatRows(node.actualRows);
  const loops = node.loops !== null && node.loops > 1 ? ` × ${node.loops} loops` : '';
  return `rows ${est} est / ${act} act${loops}`;
}

function extra(node: PlanNode): { text: string; bad: boolean } {
  const w = node.warnings[0];
  if (w?.kind === 'externalSort') return { text: `${w.method}${w.spaceKb === null ? '' : ` ${w.spaceKb} kB`}`, bad: true };
  if (w?.kind === 'diskSpill') return { text: `spilled${w.batches === null ? '' : `, ${w.batches} batches`}`, bad: true };
  if (w?.kind === 'rowMisestimate') return { text: `${Math.round(w.factor)}x off`, bad: true };
  if (w?.kind === 'seqScanLargeTable') return { text: 'large table scan', bad: true };
  if (props.highlight === 'misestimate') {
    const f = misestimateFactor(node);
    return { text: f === null ? '' : `${f.toFixed(1)}x`, bad: false };
  }
  if (node.buffers) {
    const read = node.buffers.sharedRead + node.buffers.localRead + node.buffers.tempRead;
    return { text: `hit ${formatRows(node.buffers.sharedHit)} · read ${formatRows(read)}`, bad: false };
  }
  if (node.detail.joinType) return { text: node.detail.joinType.toLowerCase(), bad: false };
  return { text: '', bad: false };
}

function aria(node: PlanNode): string {
  const self = nodeSelfText(node, props.plan.shareBasis);
  const v = values.value.get(node.id) ?? null;
  return `${nodeTitle(node)} ${nodeSubject(node)}${self ? `, ${self}` : ''}${v === null ? '' : `, ${percentText(v)} ${HIGHLIGHT_LABELS[props.highlight].toLowerCase()}`}`;
}

function onKey(event: KeyboardEvent, id: number): void {
  if (event.key === 'Enter' || event.key === ' ') {
    event.preventDefault();
    emit('select', id);
  }
}

defineExpose({ fit });
</script>

<template>
  <div class="tree">
    <div
      ref="viewport"
      class="viewport"
    >
      <div
        class="canvas"
        :style="{ width: `${layout.width * zoom}px`, height: `${layout.height * zoom}px` }"
      >
        <div
          class="scaled"
          :style="{ width: `${layout.width}px`, height: `${layout.height}px`, transform: `scale(${zoom})` }"
        >
          <svg
            :width="layout.width"
            :height="layout.height"
            :viewBox="`0 0 ${layout.width} ${layout.height}`"
            aria-hidden="true"
            class="edges"
          >
            <path
              v-for="e in layout.edges"
              :key="`${e.from}-${e.to}`"
              :d="e.d"
              :class="{ hot: hotEdges.has(e.to) }"
            />
          </svg>
          <div
            v-for="p in layout.nodes"
            :key="p.id"
            role="button"
            tabindex="0"
            class="card"
            :class="[`heat-${heatOf(values.get(p.id) ?? null)}`, { selected: selected === p.id, never: cardOf(p.id).neverExecuted }]"
            :aria-pressed="selected === p.id"
            :aria-label="aria(cardOf(p.id))"
            :style="{ left: `${p.x}px`, top: `${p.y}px`, width: `${CARD_WIDTH}px`, height: `${CARD_HEIGHT}px` }"
            @click="emit('select', p.id)"
            @keydown="onKey($event, p.id)"
          >
            <div class="line">
              <span class="op">{{ nodeTitle(cardOf(p.id)) }}</span>
              <span class="subject mono">{{ nodeSubject(cardOf(p.id)) }}</span>
              <span class="self mono">{{ nodeSelfText(cardOf(p.id), plan.shareBasis) }}</span>
            </div>
            <div class="line">
              <span class="track"><span
                class="fill"
                :style="{ width: `${Math.round((values.get(p.id) ?? 0) * 100)}%` }"
              /></span>
              <span class="pct mono">{{ percentText(values.get(p.id) ?? null) }}</span>
            </div>
            <div class="line meta">
              <span>{{ rowsLine(cardOf(p.id)) }}</span>
              <span
                class="mono"
                :class="{ bad: extra(cardOf(p.id)).bad }"
              >{{ extra(cardOf(p.id)).text }}</span>
            </div>
          </div>
        </div>
      </div>
    </div>
    <div class="legend">
      <span><span class="swatch hot" />over 50%</span>
      <span><span class="swatch warm" />10 to 50%</span>
      <span><span class="swatch cool" />under 10%</span>
      <span>{{ highlight === 'share' ? (plan.shareBasis === 'time' ? 'Times are self time, children excluded' : 'Costs are self cost, children excluded') : HIGHLIGHT_LABELS[highlight] }}</span>
    </div>
    <div class="zoom">
      <button
        type="button"
        class="zbtn"
        aria-label="Zoom out"
        @click="zoomBy(-0.1)"
      >
        −
      </button>
      <span class="mono zval">{{ Math.round(zoom * 100) }}%</span>
      <button
        type="button"
        class="zbtn"
        aria-label="Zoom in"
        @click="zoomBy(0.1)"
      >
        +
      </button>
      <button
        type="button"
        class="zbtn wide"
        @click="fit"
      >
        Fit
      </button>
    </div>
  </div>
</template>

<style scoped>
.tree {
  position: relative;
  flex: 1;
  min-width: 0;
  min-height: 0;
  display: flex;
}

.viewport {
  flex: 1;
  overflow: auto;
  background-color: var(--ground);
  background-image: radial-gradient(var(--border) 1px, transparent 1px);
  background-size: 16px 16px;
}

.canvas {
  position: relative;
  padding-bottom: 40px;
}

.scaled {
  position: absolute;
  top: 0;
  left: 0;
  transform-origin: 0 0;
}

.edges {
  position: absolute;
  top: 0;
  left: 0;
}

.edges path {
  fill: none;
  stroke: var(--text-disabled);
  stroke-width: 1.5;
}

.edges path.hot {
  stroke: var(--danger);
  stroke-width: 2;
}

.card {
  position: absolute;
  display: flex;
  flex-direction: column;
  gap: 5px;
  padding: 7px 10px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  /* Edges run under the cards: solid even over glass. */
  background: var(--surface-solid);
  cursor: pointer;
}

.card:hover {
  border-color: var(--text-disabled);
}

.card.selected {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--focus-ring);
}

.card.never {
  opacity: 0.6;
}

.line {
  display: flex;
  align-items: baseline;
  gap: 6px;
  min-width: 0;
}

.op {
  font-weight: 600;
  white-space: nowrap;
}

.subject {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  color: var(--text-muted);
  font-size: 12px;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.self {
  font-size: 12px;
  font-weight: 500;
  white-space: nowrap;
}

.track {
  flex: 1;
  height: 6px;
  border-radius: 3px;
  background: var(--surface-raised);
  align-self: center;
}

.fill {
  display: block;
  height: 6px;
  border-radius: 3px;
  background: var(--text-disabled);
}

.pct {
  width: 32px;
  font-size: 11px;
  text-align: right;
}

.heat-hot .fill {
  background: var(--danger);
}

.heat-hot .self,
.heat-hot .pct {
  color: var(--danger-text);
}

.heat-warm .fill {
  background: var(--warning);
}

.heat-warm .self,
.heat-warm .pct {
  color: var(--warning-text);
}

.meta {
  gap: 10px;
  overflow: hidden;
  color: var(--text-muted);
  font-size: 11px;
  white-space: nowrap;
}

.bad {
  color: var(--danger-text);
}

.legend {
  position: absolute;
  bottom: 12px;
  left: 24px;
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 2px 6px;
  border-radius: var(--radius-control);
  /* Floats over the plan: frosted over glass, the opaque ground without it. */
  background: var(--frost-ground);
  -webkit-backdrop-filter: var(--overlay-blur);
  backdrop-filter: var(--overlay-blur);
  color: var(--text-muted);
  font-size: 11px;
}

.legend > span {
  display: flex;
  align-items: center;
  gap: 6px;
}

.swatch {
  width: 16px;
  height: 6px;
  border-radius: 3px;
}

.swatch.hot {
  background: var(--danger);
}

.swatch.warm {
  background: var(--warning);
}

.swatch.cool {
  background: var(--text-disabled);
}

.zoom {
  position: absolute;
  right: 12px;
  bottom: 10px;
  display: flex;
  align-items: center;
  gap: 4px;
}

.zbtn {
  width: 26px;
  height: 26px;
  padding: 0;
  border: 1px solid var(--border);
  border-radius: var(--radius-control);
  background: var(--frost);
  -webkit-backdrop-filter: var(--overlay-blur);
  backdrop-filter: var(--overlay-blur);
  color: var(--text);
  cursor: pointer;
}

.zbtn.wide {
  width: auto;
  padding: 0 8px;
  font-size: 12px;
}

.zval {
  min-width: 40px;
  color: var(--text-muted);
  font-size: 11px;
  text-align: center;
}
</style>
