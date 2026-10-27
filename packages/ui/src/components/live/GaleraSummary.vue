<script setup lang="ts">
import { computed, ref } from 'vue';
import type { GaleraStatus } from '../../api/live';

/** Galera cluster status of a MariaDB node (design: ServerVariables side panel). */
const props = defineProps<{ galera: GaleraStatus }>();

const showAll = ref(false);
const tone = computed(() => {
  const s = props.galera.clusterStatus;
  if (s === 'Primary') return 'tone-success';
  if (s === null) return 'tone-neutral';
  return 'tone-danger';
});

function yesNo(value: boolean | null): string {
  if (value === null) return 'n/a';
  return value ? 'Yes' : 'No';
}
</script>

<template>
  <div class="galera">
    <p
      v-if="!galera.enabled"
      class="note"
    >
      Not a cluster member: wsrep is off or no provider is loaded.
    </p>
    <template v-else>
      <span
        class="pill"
        :class="tone"
      ><span class="dot" />{{ galera.clusterStatus ?? 'Unknown' }}</span>
      <dl class="facts">
        <dt>Cluster size</dt><dd>{{ galera.clusterSize ?? 'n/a' }}</dd>
        <dt>Local state</dt><dd>{{ galera.localStateComment ?? 'n/a' }}</dd>
        <dt>Ready</dt><dd>{{ yesNo(galera.ready) }}</dd>
        <dt>Connected</dt><dd>{{ yesNo(galera.connected) }}</dd>
        <dt>Flow control paused</dt><dd>{{ galera.flowControlPaused === null ? 'n/a' : `${(galera.flowControlPaused * 100).toFixed(1)}%` }}</dd>
        <dt>Provider</dt><dd>{{ [galera.providerName, galera.providerVersion].filter(Boolean).join(' ') || 'n/a' }}</dd>
        <dt>State UUID</dt><dd>{{ galera.clusterStateUuid ?? 'n/a' }}</dd>
      </dl>
    </template>
    <button
      v-if="galera.values.length > 0"
      type="button"
      class="btn ghost compact"
      :aria-expanded="showAll"
      @click="showAll = !showAll"
    >
      {{ showAll ? 'Hide' : 'Show' }} all {{ galera.values.length }} wsrep values
    </button>
    <dl
      v-if="showAll"
      class="facts all"
    >
      <template
        v-for="v in galera.values"
        :key="v.name"
      >
        <dt>{{ v.name }}</dt><dd>{{ v.value }}</dd>
      </template>
    </dl>
    <p class="note">
      From <span class="mono">SHOW GLOBAL STATUS LIKE 'wsrep_%'</span>.
    </p>
  </div>
</template>

<style scoped src="./panel.css"></style>

<style scoped>
.galera {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 8px;
}

.all {
  max-height: 260px;
  overflow: auto;
}

.note {
  margin: 0;
}
</style>
