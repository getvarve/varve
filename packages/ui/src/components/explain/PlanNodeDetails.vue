<script setup lang="ts">
import { computed } from 'vue';
import type { Buffers, Plan, PlanNode } from '../../api/explain';
import { PG_BLOCK_BYTES, formatMs, formatNumber, formatRows, heatOf, misestimateFactor, nodeSubject, nodeTitle, percentText, warningText } from '../../lib/explainPlan';
import { formatBytes } from '../../lib/values';
import Icon from '../common/Icon.vue';

/**
 * Details of one plan node (design: ExplainPlan "Node details"): times,
 * rows, cost, buffers, conditions, every other field the server reported,
 * and the problems found. Without a node it shows the plan as a whole
 * (triggers, planner settings, plan fields).
 */
const props = defineProps<{ plan: Plan; node: PlanNode | null }>();

const heat = computed(() => (props.node ? heatOf(props.node.share) : 'cool'));
const factor = computed(() => (props.node ? misestimateFactor(props.node) : null));
const pg = computed(() => props.plan.format === 'postgresJson');

function range(a: number | null | undefined, b: number | null | undefined, fmt: (n: number) => string): string {
  if (a !== null && a !== undefined && b !== null && b !== undefined) return `${fmt(a)} to ${fmt(b)}`;
  if (b !== null && b !== undefined) return fmt(b);
  return '';
}

function blocks(n: number): string {
  return pg.value ? `${formatNumber(n)} (${formatBytes(n * PG_BLOCK_BYTES)})` : formatNumber(n);
}

function bufferRows(b: Buffers): { label: string; value: number; bad: boolean }[] {
  const rows = [
    { label: 'Shared hit', value: b.sharedHit, bad: false },
    { label: 'Shared read', value: b.sharedRead, bad: b.sharedRead > b.sharedHit && b.sharedRead > 0 },
    { label: 'Shared dirtied', value: b.sharedDirtied, bad: false },
    { label: 'Shared written', value: b.sharedWritten, bad: false },
    { label: 'Local hit', value: b.localHit, bad: false },
    { label: 'Local read', value: b.localRead, bad: false },
    { label: 'Local dirtied', value: b.localDirtied, bad: false },
    { label: 'Local written', value: b.localWritten, bad: false },
    { label: 'Temp read', value: b.tempRead, bad: b.tempRead > 0 },
    { label: 'Temp written', value: b.tempWritten, bad: b.tempWritten > 0 },
  ];
  return rows.filter((r) => r.value > 0 || r.label === 'Shared hit' || r.label === 'Shared read');
}

const conditions = computed(() => {
  const d = props.node?.detail;
  if (!d) return [];
  const out: { label: string; text: string }[] = [];
  if (d.condition) out.push({ label: 'Condition', text: d.condition });
  if (d.filter) out.push({ label: 'Filter', text: d.filter });
  if (d.sortKey.length > 0) out.push({ label: 'Sort key', text: d.sortKey.join(', ') });
  if (d.groupKey.length > 0) out.push({ label: 'Group key', text: d.groupKey.join(', ') });
  if (d.output.length > 0) out.push({ label: 'Output', text: d.output.join(', ') });
  return out;
});
</script>

<template>
  <aside
    class="details"
    aria-label="Node details"
  >
    <template v-if="node">
      <div class="title">
        <span class="op">{{ nodeTitle(node) }}</span>
        <span class="subject mono">{{ nodeSubject(node) }}</span>
        <span class="grow" />
        <span
          v-if="node.share !== null && heat !== 'cool'"
          class="pill"
          :class="heat === 'hot' ? 'tone-danger' : 'tone-warning'"
        ><span class="dot" />Hot spot, {{ percentText(node.share) }}</span>
      </div>
      <dl class="facts">
        <template v-if="node.neverExecuted">
          <dt>Ran</dt><dd>Never executed</dd>
        </template>
        <template v-if="node.actualTime">
          <dt>Actual time</dt><dd>{{ range(node.actualTime.firstRowMs, node.actualTime.perLoopMs, formatMs) }}<span class="subtle"> per loop</span></dd>
          <template v-if="node.actualTime.totalMs !== null">
            <dt>Total time</dt><dd>{{ formatMs(node.actualTime.totalMs) }}</dd>
          </template>
          <template v-if="node.actualTime.exclusiveMs !== null">
            <dt>Self time</dt><dd>{{ formatMs(node.actualTime.exclusiveMs) }}</dd>
          </template>
        </template>
        <template v-if="node.loops !== null">
          <dt>Loops</dt><dd>{{ formatNumber(node.loops) }}</dd>
        </template>
        <dt>Rows est / actual</dt>
        <dd>
          {{ node.estimatedRows === null ? '?' : formatRows(node.estimatedRows) }} /
          {{ node.actualRows === null ? (plan.analyzed ? '?' : 'not run') : formatRows(node.actualRows) }}
          <span
            v-if="factor !== null"
            class="subtle"
            :class="{ bad: factor >= 10 }"
          >({{ factor.toFixed(2) }}x)</span>
        </dd>
        <template v-if="node.estimatedCost">
          <dt>Cost</dt><dd>{{ range(node.estimatedCost.startup, node.estimatedCost.total, (n) => formatNumber(n, 2)) }}</dd>
          <template v-if="node.estimatedCost.exclusive !== null">
            <dt>Self cost</dt><dd>{{ formatNumber(node.estimatedCost.exclusive, 2) }}</dd>
          </template>
        </template>
        <template v-if="node.detail.index">
          <dt>Index</dt><dd>{{ node.detail.index }}</dd>
        </template>
        <template v-if="node.detail.joinType">
          <dt>Join type</dt><dd>{{ node.detail.joinType }}</dd>
        </template>
        <template v-if="node.detail.strategy">
          <dt>Strategy</dt><dd>{{ node.detail.strategy }}</dd>
        </template>
        <template v-if="node.detail.relationship">
          <dt>Parent relation</dt><dd>{{ node.detail.relationship }}</dd>
        </template>
        <template v-if="node.detail.subplan">
          <dt>Subplan</dt><dd>{{ node.detail.subplan }}</dd>
        </template>
        <template v-if="node.detail.parallelAware || node.detail.workersPlanned !== null">
          <dt>Workers</dt><dd>{{ node.detail.workersLaunched ?? '?' }} launched of {{ node.detail.workersPlanned ?? '?' }} planned</dd>
        </template>
        <template v-if="node.buffers">
          <template
            v-for="r in bufferRows(node.buffers)"
            :key="r.label"
          >
            <dt>{{ r.label }}</dt><dd :class="{ bad: r.bad }">
              {{ blocks(r.value) }}
            </dd>
          </template>
        </template>
      </dl>

      <div
        v-for="c in conditions"
        :key="c.label"
        class="block"
      >
        <span class="label">{{ c.label }}</span>
        <pre class="code">{{ c.text }}</pre>
      </div>

      <div
        v-for="(w, i) in node.warnings"
        :key="i"
        class="advice"
      >
        <div class="advice-title">
          <Icon name="info" /><span>{{ warningText(w).title }}</span>
        </div>
        <span class="advice-text">{{ warningText(w).detail }}</span>
      </div>

      <details
        v-if="node.properties.length > 0"
        class="more"
      >
        <summary>All fields ({{ node.properties.length }})</summary>
        <dl class="facts">
          <template
            v-for="(p, i) in node.properties"
            :key="i"
          >
            <dt>{{ p.name }}</dt><dd>{{ p.value }}</dd>
          </template>
        </dl>
      </details>
    </template>

    <template v-else>
      <div class="title">
        <span class="op">Plan</span>
        <span class="subtle">Select a node to see its details.</span>
      </div>
      <dl class="facts">
        <template v-if="plan.planningMs !== null">
          <dt>Planning</dt><dd>{{ formatMs(plan.planningMs) }}</dd>
        </template>
        <template v-if="plan.executionMs !== null">
          <dt>Execution</dt><dd>{{ formatMs(plan.executionMs) }}</dd>
        </template>
        <template v-if="plan.totalCost !== null">
          <dt>Total cost</dt><dd>{{ formatNumber(plan.totalCost, 2) }}</dd>
        </template>
        <template v-if="plan.planningBuffers">
          <dt>Planning hit / read</dt><dd>{{ blocks(plan.planningBuffers.sharedHit) }} / {{ blocks(plan.planningBuffers.sharedRead) }}</dd>
        </template>
      </dl>
      <div
        v-if="plan.triggers.length > 0"
        class="block"
      >
        <span class="label">Triggers</span>
        <dl class="facts">
          <template
            v-for="t in plan.triggers"
            :key="t.name"
          >
            <dt>{{ t.name }}{{ t.relation ? ` on ${t.relation}` : '' }}</dt>
            <dd>{{ t.timeMs === null ? '' : formatMs(t.timeMs) }}{{ t.calls === null ? '' : ` · ${formatNumber(t.calls)} calls` }}</dd>
          </template>
        </dl>
      </div>
      <div
        v-if="plan.settings.length > 0"
        class="block"
      >
        <span class="label">Planner settings</span>
        <dl class="facts">
          <template
            v-for="s in plan.settings"
            :key="s.name"
          >
            <dt>{{ s.name }}</dt><dd>{{ s.value }}</dd>
          </template>
        </dl>
      </div>
      <div
        v-if="plan.properties.length > 0"
        class="block"
      >
        <span class="label">Plan fields</span>
        <dl class="facts">
          <template
            v-for="(p, i) in plan.properties"
            :key="i"
          >
            <dt>{{ p.name }}</dt><dd>{{ p.value }}</dd>
          </template>
        </dl>
      </div>
    </template>
  </aside>
</template>

<style scoped>
.details {
  display: flex;
  flex-direction: column;
  gap: 10px;
  width: 360px;
  flex-shrink: 0;
  padding: 12px 16px;
  border-left: 1px solid var(--border);
  background: var(--surface);
  overflow: auto;
}

.title {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}

.op {
  font-size: 15px;
  font-weight: 500;
}

.subject {
  color: var(--text-muted);
  font-size: 12px;
  word-break: break-word;
}

.grow {
  flex: 1;
}

.facts {
  display: grid;
  grid-template-columns: 132px minmax(0, 1fr);
  gap: 5px 8px;
  margin: 0;
  font-size: 12px;
}

.facts dt {
  color: var(--text-muted);
  word-break: break-word;
}

.facts dd {
  margin: 0;
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
  word-break: break-word;
}

.subtle {
  color: var(--text-subtle);
}

.bad {
  color: var(--danger-text);
}

.block {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.code {
  margin: 0;
  padding: 8px 10px;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
  background: var(--ground);
  font-family: var(--font-mono);
  font-size: 12px;
  line-height: 17px;
  white-space: pre-wrap;
  word-break: break-word;
}

.advice {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 10px 12px;
  border-radius: var(--radius-panel);
  background: var(--info-tint);
  box-shadow: inset 2px 0 0 var(--info);
}

.advice-title {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  color: var(--info-text);
  font-weight: 500;
}

.advice-text {
  font-size: 12px;
  line-height: 1.45;
}

.more summary {
  color: var(--text-muted);
  font-size: 12px;
  cursor: pointer;
}

.more .facts {
  margin-top: 6px;
}
</style>
