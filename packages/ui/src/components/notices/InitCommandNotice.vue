<script setup lang="ts">
import { computed, ref } from 'vue';
import type { CmdError } from '../../api/errors';
import { useWorkspace } from '../../composables/useWorkspace';
import { useWorkspaceActions } from '../../composables/useWorkspaceActions';
import { initCommandProblem } from '../../lib/redisInit';
import Icon from '../common/Icon.vue';

/**
 * Explains a Redis init command that was refused (when connecting or saving)
 * and opens the connection editor to fix it. Renders nothing for other
 * errors, so a caller can place it next to any connect error.
 * Emits `edited` after the editor saved the profile (for example to connect again).
 */
const props = defineProps<{ error: CmdError; connectionId: string }>();
const emit = defineEmits<{ edited: [] }>();

const problem = computed(() => initCommandProblem(props.error));
const profile = computed(() => useWorkspace().connection(props.connectionId));
const editing = ref(false);

async function edit(): Promise<void> {
  const current = profile.value;
  if (!current || editing.value) return;
  editing.value = true;
  try {
    const before = current.version;
    await useWorkspaceActions().editConnection(current);
    const after = useWorkspace().connection(props.connectionId);
    if (after && after.version !== before) emit('edited');
  } finally {
    editing.value = false;
  }
}
</script>

<template>
  <div
    v-if="problem"
    class="notice tone-danger init"
    role="alert"
  >
    <Icon
      name="warning"
      class="icon"
    />
    <div class="body">
      <p class="head">
        Init command on line {{ problem.line }} was refused
      </p>
      <p>{{ problem.reason }}</p>
      <p v-if="problem.credentials">
        Remove this line and enter the password (and the ACL user name) in the connection's credentials fields
        instead. Varve keeps the password in your system keychain and sends it with AUTH itself.
      </p>
      <p v-else>
        Init commands run on every connect and reconnect, so only connection setup commands are allowed:
        SELECT, HELLO 2 (with SETNAME), CLIENT SETNAME, CLIENT SETINFO, CLIENT NO-EVICT, CLIENT NO-TOUCH, READONLY
        and READWRITE. Run anything else in the console.
      </p>
    </div>
    <button
      v-if="profile"
      type="button"
      class="btn compact"
      :disabled="editing"
      @click="edit"
    >
      Edit connection
    </button>
  </div>
</template>

<style scoped>
.init {
  align-items: flex-start;
}

.icon {
  flex-shrink: 0;
  margin-top: 2px;
}

.body {
  flex: 1;
  min-width: 0;
}

.body p {
  margin: 0 0 4px;
}

.head {
  font-weight: 500;
}

.compact {
  height: 28px;
  font-size: 12px;
}
</style>
