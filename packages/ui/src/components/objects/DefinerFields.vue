<script setup lang="ts">
import { useId } from 'vue';
import type { DefinerChoice } from '../../lib/ddlPlan';
import FormField from '../common/FormField.vue';

/**
 * The DEFINER of a MySQL/MariaDB view, routine, trigger or event. Editing
 * an existing object keeps its definer unless the user picks another one
 * (recreating it would otherwise make the current user its definer).
 */
withDefaults(defineProps<{ errors: Record<string, string>; existing: boolean }>(), {});
const definer = defineModel<DefinerChoice>('definer', { required: true });
const user = defineModel<string>('user', { required: true });
const host = defineModel<string>('host', { required: true });
const ids = { choice: useId(), user: useId(), host: useId() };
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
          v-if="existing"
          value="keep"
        >
          Keep {{ user }}@{{ host }}
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
