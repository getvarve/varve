<script setup lang="ts">
import { computed, useId, watch } from 'vue';
import { parseDefiner } from '../../lib/ddlPlan';
import type { DefinerChoice } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';

/**
 * The DEFINER of a MySQL/MariaDB view, routine, trigger or event. Editing
 * an existing object keeps its definer unless the user picks another one
 * (recreating it would otherwise make the current user its definer).
 * `original` is the described definer (`user@host`): "Keep" names it, and
 * choosing "Keep" again puts it back into the account fields. When the
 * object is gone and the form became a create form, a kept definer stays
 * chosen and names the account the fields still hold.
 */
const props = withDefaults(defineProps<{ errors: Record<string, string>; existing: boolean; original?: string | null }>(), { original: null });
const definer = defineModel<DefinerChoice>('definer', { required: true });
const user = defineModel<string>('user', { required: true });
const host = defineModel<string>('host', { required: true });
const ids = { choice: useId(), user: useId(), host: useId() };
const kept = computed(() => parseDefiner(props.original));
/** The account "Keep" stands for: the described definer, or the account fields when nothing is described any more. */
const keptLabel = computed(() => {
  if (kept.value) return `${kept.value.user}@${kept.value.host}`;
  if (props.original) return props.original;
  return `${user.value.trim()}@${host.value.trim() === '' ? '%' : host.value.trim()}`;
});

watch(definer, (choice) => {
  if (choice !== 'keep' || !kept.value) return;
  user.value = kept.value.user;
  host.value = kept.value.host;
});
</script>

<template>
  <div class="definer">
    <FormField
      label="Definer"
      :input-id="ids.choice"
    >
      <select
        :id="ids.choice"
        v-model="definer"
        class="select"
      >
        <option
          v-if="existing || definer === 'keep'"
          value="keep"
        >
          Keep {{ keptLabel }}
        </option>
        <option value="currentUser">
          CURRENT_USER
        </option>
        <option value="account">
          Another account
        </option>
      </select>
    </FormField>
    <template v-if="definer === 'account'">
      <FormField
        label="User"
        :input-id="ids.user"
        required
        :error="errors.definerUser ?? ''"
      >
        <input
          :id="ids.user"
          v-model="user"
          class="input mono"
          autocomplete="off"
          spellcheck="false"
          :aria-invalid="!!errors.definerUser"
        >
      </FormField>
      <FormField
        label="Host"
        :input-id="ids.host"
      >
        <input
          :id="ids.host"
          v-model="host"
          class="input mono"
          autocomplete="off"
          spellcheck="false"
          placeholder="%"
        >
      </FormField>
    </template>
  </div>
</template>

<style scoped>
.definer {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
  gap: 12px;
}
</style>
