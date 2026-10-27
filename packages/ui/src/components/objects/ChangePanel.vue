<script setup lang="ts">
import { computed, ref } from 'vue';
import type { Engine } from '../../api/records';
import { useCrossFade } from '../../composables/useCrossFade';
import { stepsSql } from '../../lib/ddlApply';
import PlanPreview from './PlanPreview.vue';
import type { ChangePlan } from './useChangePlan';

/**
 * The plan preview of an editor in its two modes: saving the form, or
 * dropping the object (with CASCADE where the engine and kind have it).
 * The `drop` slot shows what the drop affects (`DropImpactPanel.vue`) and
 * drop options of the kind. The preview settles in when it appears and
 * cross-fades (120ms) when the planned SQL changes.
 */
const props = withDefaults(
  defineProps<{
    plan: ChangePlan;
    engine: Engine;
    readOnly: boolean;
    mode: 'edit' | 'drop';
    creating: boolean;
    objectName: string;
    /** Form problems or a reason the edit cannot be planned. */
    blocked?: string | null;
    saveLabel?: string;
    /** Whether the drop offers CASCADE; `null`: PostgreSQL does. */
    cascadeAvailable?: boolean | null;
    /** A reason the drop cannot run from here (shown instead of its SQL). */
    dropBlocked?: string | null;
  }>(),
  { blocked: null, saveLabel: '', cascadeAvailable: null, dropBlocked: null },
);
const cascade = defineModel<boolean>('cascade', { required: true });
const emit = defineEmits<{ applied: []; cancelDrop: []; reconnect: []; refresh: [] }>();

const offersCascade = computed(() => props.cascadeAvailable ?? props.engine === 'postgres');
const label = computed(() => {
  if (props.mode === 'drop') return `Drop ${props.objectName}`;
  if (props.saveLabel) return props.saveLabel;
  return props.creating ? 'Create' : 'Save';
});

const preview = ref<InstanceType<typeof PlanPreview> | null>(null);
const previewEl = computed(() => (preview.value?.$el instanceof HTMLElement ? preview.value.$el : null));
useCrossFade(previewEl, () => stepsSql(props.plan.state.steps), 120);
</script>

<template>
  <div class="panel">
    <div
      v-if="mode === 'drop'"
      class="notice tone-danger drop"
      role="note"
    >
      <span class="grow">Dropping <strong class="mono">{{ objectName }}</strong> removes it for every user of this database.</span>
      <label
        v-if="offersCascade"
        class="check"
      >
        <input
          v-model="cascade"
          type="checkbox"
        >CASCADE (also drop objects that depend on it)
      </label>
      <button
        type="button"
        class="btn compact"
        @click="emit('cancelDrop')"
      >
        Cancel
      </button>
    </div>
    <slot
      v-if="mode === 'drop'"
      name="drop"
    />
    <PlanPreview
      ref="preview"
      class="plan-in"
      :plan="plan"
      :engine="engine"
      :read-only="readOnly"
      :apply-label="label"
      :blocked="mode === 'drop' ? dropBlocked : blocked"
      :empty-text="creating ? 'Fill in the form to see the SQL.' : 'No changes yet. Edit the form to see the SQL that will run.'"
      @applied="emit('applied')"
      @reconnect="emit('reconnect')"
      @refresh="emit('refresh')"
    />
  </div>
</template>

<style scoped>
.panel {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.plan-in {
  animation: plan-in 180ms var(--ease-out);
}

@keyframes plan-in {
  from {
    opacity: 0;
    transform: translateY(4px);
  }
}

@keyframes plan-fade-in {
  from {
    opacity: 0;
  }
}

/* Reduced motion: a short fade only, no movement. The cross-fade is opacity only. */
@media (prefers-reduced-motion: reduce) {
  .plan-in {
    animation-name: plan-fade-in;
  }
}

.drop {
  flex-wrap: wrap;
  align-items: center;
}

.grow {
  flex-grow: 1;
}

.check {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.check input {
  margin: 0;
  accent-color: var(--danger);
}
</style>
