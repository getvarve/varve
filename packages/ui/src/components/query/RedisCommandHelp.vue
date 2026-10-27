<script setup lang="ts">
import { ref, shallowRef, watch } from 'vue';
import { toCmdError } from '../../api/errors';
import type { CommandHelp } from '../../api/redis';
import { latestOnly } from '../../lib/async';
import { redisErrorView } from '../../lib/redisErrors';
import { arityText, commandHelp } from '../../lib/redisHelp';
import Icon from '../common/Icon.vue';

/**
 * Help for the Redis command on the console's cursor line (F1 or the Help
 * button): `COMMAND DOCS` where the server has it (Redis 7.0 and newer,
 * Valkey), otherwise what `COMMAND INFO` knows and a note why.
 */
const props = defineProps<{ sessionId: string | null; command: string | null }>();
const emit = defineEmits<{ close: [] }>();

const help = shallowRef<CommandHelp | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);
const requests = latestOnly();

async function load(): Promise<void> {
  const token = requests.next();
  help.value = null;
  error.value = null;
  if (!props.sessionId || !props.command) return;
  loading.value = true;
  try {
    const out = await commandHelp(props.sessionId, props.command);
    if (requests.isCurrent(token)) help.value = out;
  } catch (e: unknown) {
    if (!requests.isCurrent(token)) return;
    const view = redisErrorView(toCmdError(e));
    error.value = view.hint ? `${view.message} ${view.hint}` : view.message;
  } finally {
    if (requests.isCurrent(token)) loading.value = false;
  }
}

watch(() => [props.sessionId, props.command], () => void load(), { immediate: true });
</script>

<template>
  <aside
    class="help"
    aria-label="Command help"
  >
    <header>
      <Icon name="info" />
      <strong>{{ help?.name ?? command ?? 'Help' }}</strong>
      <span class="spacer" />
      <button
        type="button"
        class="icon-btn"
        aria-label="Close help"
        @click="emit('close')"
      >
        <Icon name="close" />
      </button>
    </header>
    <div
      class="body"
      aria-live="polite"
    >
      <p
        v-if="!command"
        class="subtle"
      >
        Put the cursor on a line with a command and press F1.
      </p>
      <p
        v-else-if="!sessionId"
        class="subtle"
      >
        Connect to see the server's help.
      </p>
      <p
        v-else-if="loading"
        class="subtle"
        role="status"
      >
        Looking it up…
      </p>
      <p
        v-else-if="error"
        class="notice tone-danger"
        role="alert"
      >
        {{ error }}
      </p>
      <template v-else-if="help">
        <p v-if="help.summary">
          {{ help.summary }}
        </p>
        <pre
          v-if="help.syntax"
          class="mono syntax"
        >{{ help.syntax }}</pre>
        <p
          v-if="help.deprecatedSince"
          class="notice tone-warning"
        >
          {{ help.replacedBy ? `Deprecated since ${help.deprecatedSince}; use ${help.replacedBy} instead.` : `Deprecated since ${help.deprecatedSince}.` }}
        </p>
        <dl>
          <template v-if="help.group">
            <dt>Group</dt>
            <dd>{{ help.group }}</dd>
          </template>
          <template v-if="help.since">
            <dt>Since</dt>
            <dd>{{ help.since }}</dd>
          </template>
          <template v-if="help.complexity">
            <dt>Complexity</dt>
            <dd>{{ help.complexity }}</dd>
          </template>
          <template v-if="arityText(help.arity)">
            <dt>Arity</dt>
            <dd>{{ arityText(help.arity) }}</dd>
          </template>
          <template v-if="help.flags.length > 0">
            <dt>Flags</dt>
            <dd class="mono">
              {{ help.flags.join(', ') }}
            </dd>
          </template>
        </dl>
        <template v-if="help.subcommands.length > 0">
          <strong class="sub">Subcommands</strong>
          <ul class="mono subs">
            <li
              v-for="s in help.subcommands"
              :key="s"
            >
              {{ s }}
            </li>
          </ul>
        </template>
        <p
          v-if="help.note"
          class="subtle"
        >
          {{ help.note }}
        </p>
      </template>
    </div>
  </aside>
</template>

<style scoped>
.help {
  display: flex;
  flex-direction: column;
  width: 320px;
  min-height: 0;
  border-left: 1px solid var(--border);
  background: var(--surface);
}

header {
  display: flex;
  flex-shrink: 0;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 4px 0 12px;
  border-bottom: 1px solid var(--border-inner);
}

.spacer {
  flex: 1;
}

.body {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px;
  overflow: auto;
  font-size: 12px;
}

.body p {
  margin: 0;
}

.syntax {
  margin: 0;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  padding: 8px;
  border-radius: 4px;
  background: var(--zebra);
}

dl {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: 4px 12px;
  margin: 0;
}

dt {
  color: var(--text-muted);
}

dd {
  margin: 0;
}

.subs {
  margin: 0;
  padding-left: 16px;
}
</style>
