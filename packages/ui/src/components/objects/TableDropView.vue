<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { kindLabel } from '../../api/catalog';
import type { ObjectRef } from '../../api/ddl';
import type { SessionInfo } from '../../api/sessions';
import { buildDrop } from '../../lib/ddlPlan';
import ChangePanel from './ChangePanel.vue';
import DropImpactPanel from './DropImpactPanel.vue';
import EditorFrame from './EditorFrame.vue';
import { displayName } from './types';
import { useChangePlan } from './useChangePlan';

/**
 * Dropping a table (the tree's "Drop table"; tables are edited in the table
 * designer, so this is their drop flow): what depends on it and what goes
 * with it, CASCADE on PostgreSQL, the plan preview and the destructive
 * confirmation. Emits `dropped` after the drop ran, `cancel` to go back to
 * the table's details, `changed` after anything ran.
 */
const props = withDefaults(
  defineProps<{
    session: SessionInfo;
    connectionName: string;
    object: ObjectRef;
    reportError?: ((error: unknown) => void) | null;
  }>(),
  { reportError: null },
);
const emit = defineEmits<{ dropped: [object: ObjectRef]; cancel: []; changed: []; reconnect: [] }>();

const cascade = ref(false);
const tab = ref('drop');
const plan = useChangePlan({
  session: () => props.session,
  connectionName: () => props.connectionName,
  onChanged: () => emit('changed'),
  reportError: (error) => props.reportError?.(error),
});
const name = computed(() => displayName(props.object, props.session.engine));

async function planDrop(): Promise<void> {
  const change = buildDrop(props.session.engine, props.object, cascade.value);
  await plan.plan(change ? [{ kind: 'edit', change }] : []);
}

function applied(): void {
  emit('dropped', props.object);
}

watch(cascade, () => void planDrop());
watch(
  () => JSON.stringify([props.session.id, props.object]),
  () => void planDrop(),
);
onMounted(() => void planDrop());
</script>

<template>
  <EditorFrame
    v-model:active="tab"
    :title="name"
    :kind-label="kindLabel(object.kind)"
    :environment="session.environment"
    :tabs="[{ id: 'drop', label: 'Drop' }]"
    :read-only="session.readOnly"
  >
    <template #drop>
      <div class="layout single">
        <ChangePanel
          v-model:cascade="cascade"
          :plan="plan"
          :engine="session.engine"
          :read-only="session.readOnly"
          mode="drop"
          :creating="false"
          :object-name="name"
          @applied="applied"
          @cancel-drop="emit('cancel')"
          @reconnect="emit('reconnect')"
          @refresh="planDrop"
        >
          <template #drop>
            <DropImpactPanel
              :session="session"
              :object="object"
              :cascade="cascade"
              :report-error="reportError"
            />
          </template>
        </ChangePanel>
      </div>
    </template>
  </EditorFrame>
</template>

<style scoped src="./editor.css"></style>
