<script setup lang="ts">
import { computed } from 'vue';
import type { LiveTab } from '../../composables/useTabs';
import { useTabs } from '../../composables/useTabs';
import { useWorkspace } from '../../composables/useWorkspace';
import GaleraPanel from './GaleraPanel.vue';
import ProcessListPanel from './ProcessListPanel.vue';
import RedisConfigPanel from './RedisConfigPanel.vue';
import RedisDiagnostics from './RedisDiagnostics.vue';
import RedisFunctionsPanel from './RedisFunctionsPanel.vue';
import RedisInfoPanel from './RedisInfoPanel.vue';
import RedisMemory from './RedisMemory.vue';
import RedisModulesPanel from './RedisModulesPanel.vue';
import RedisTopologyPanel from './RedisTopologyPanel.vue';
import ServerStatusPanel from './ServerStatusPanel.vue';
import ServerVariablesPanel from './ServerVariablesPanel.vue';

/**
 * The live panel of a workspace tab (`kind: 'live'`), chosen by
 * `tab.panel`. WorkspaceView renders it once the tab is connected; every
 * panel reports its errors to the tab and offers Reconnect through it.
 */
const props = defineProps<{ tab: LiveTab }>();

const tabs = useTabs();
const workspace = useWorkspace();
const connectionName = computed(() => workspace.connection(props.tab.connectionId)?.name ?? props.tab.title);
const session = computed(() => props.tab.session);

function reconnect(): void {
  void tabs.reconnect(props.tab.id);
}
</script>

<template>
  <div
    v-if="session"
    class="live-tab"
  >
    <ProcessListPanel
      v-if="tab.panel === 'processes'"
      :session="session"
      :connection-name="connectionName"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <ServerVariablesPanel
      v-else-if="tab.panel === 'variables'"
      :session="session"
      :connection-name="connectionName"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <ServerStatusPanel
      v-else-if="tab.panel === 'serverStatus'"
      :session="session"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <GaleraPanel
      v-else-if="tab.panel === 'galera'"
      :session="session"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <RedisInfoPanel
      v-else-if="tab.panel === 'redisInfo'"
      :session="session"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <RedisDiagnostics
      v-else-if="tab.panel === 'redisClients' || tab.panel === 'redisSlowlog' || tab.panel === 'redisLatency'"
      :session="session"
      :connection-name="connectionName"
      :tab-id="tab.id"
      :focus="tab.panel === 'redisClients' ? 'clients' : tab.panel === 'redisSlowlog' ? 'slowlog' : 'latency'"
      @reconnect="reconnect"
    />
    <RedisConfigPanel
      v-else-if="tab.panel === 'redisConfig'"
      :session="session"
      :connection-name="connectionName"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <RedisMemory
      v-else-if="tab.panel === 'redisMemory'"
      :session="session"
      :connection-name="connectionName"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <RedisFunctionsPanel
      v-else-if="tab.panel === 'redisFunctions'"
      :session="session"
      :connection-name="connectionName"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <RedisTopologyPanel
      v-else-if="tab.panel === 'redisTopology'"
      :session="session"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
    <RedisModulesPanel
      v-else-if="tab.panel === 'redisModules'"
      :session="session"
      :tab-id="tab.id"
      @reconnect="reconnect"
    />
  </div>
</template>

<style scoped>
.live-tab {
  height: 100%;
  min-height: 0;
}
</style>
