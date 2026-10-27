<script setup lang="ts">
import { shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { galeraStatus } from '../../api/live';
import { stopsPolling } from '../../lib/live';
import type { GaleraStatus } from '../../api/live';
import type { SessionInfo } from '../../api/sessions';
import EnvBadge from '../common/EnvBadge.vue';
import GaleraSummary from './GaleraSummary.vue';
import LiveError from './LiveError.vue';
import RefreshControls from './RefreshControls.vue';
import { useAutoRefresh } from './useAutoRefresh';
import { useReporter } from './report';

/** Galera cluster status of a MariaDB node (`wsrep_%` status), refreshed every 5 s. */
const props = defineProps<{
  /** The tab's MariaDB session. */
  session: SessionInfo;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const galera = shallowRef<GaleraStatus | null>(null);
const error = shallowRef<CmdError | null>(null);

const refresh = useAutoRefresh(
  async () => {
    const result = await galeraStatus(props.session.id);
    if (!refresh.alive()) return;
    galera.value = result;
    error.value = null;
  },
  5000,
  (e) => {
    error.value = report(e);
    // A node without Galera stays without it: polling again only repeats the error.
    if (stopsPolling(error.value)) refresh.pause();
  },
);
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Galera cluster</h1>
      <EnvBadge :environment="session.environment" />
      <span class="grow" />
      <RefreshControls
        :state="refresh.state.value"
        @interval="refresh.setInterval"
        @pause="refresh.pause"
        @resume="refresh.resume"
        @refresh="refresh.refreshNow"
      />
    </div>
    <LiveError
      v-if="error"
      :error="error"
      class="panel-error"
      @reconnect="emit('reconnect')"
    />
    <div class="scroll body">
      <GaleraSummary
        v-if="galera"
        :galera="galera"
      />
      <div
        v-else-if="!error"
        class="empty"
      >
        Loading…
      </div>
    </div>
  </div>
</template>

<style scoped src="./panel.css"></style>

<style scoped>
.panel-error {
  margin: 8px 16px 0;
}

.body {
  padding: 16px;
}
</style>
