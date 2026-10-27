<script setup lang="ts">
/**
 * Label, control, help text and error of one form field. The required
 * marker comes from the caller, which reads it from the form's schema.
 */
withDefaults(
  defineProps<{ label: string; inputId: string; required?: boolean; error?: string; help?: string; labelId?: string }>(),
  { required: false, error: '', help: '', labelId: '' },
);
</script>

<template>
  <div
    class="field"
    :class="{ invalid: !!error }"
  >
    <label
      :id="labelId || undefined"
      class="field-label"
      :for="inputId"
    >
      {{ label }}<span
        v-if="required"
        class="required"
        aria-hidden="true"
      > *</span><span
        v-if="required"
        class="visually-hidden"
      > (required)</span>
      <slot name="aside" />
    </label>
    <slot />
    <span
      v-if="error"
      :id="`${inputId}-error`"
      class="error"
      role="alert"
    >{{ error }}</span>
    <span
      v-else-if="help || $slots.help"
      :id="`${inputId}-help`"
      class="help"
    ><slot name="help">{{ help }}</slot></span>
  </div>
</template>

<style scoped>
.field {
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}

.field-label {
  display: flex;
  align-items: center;
  gap: 2px;
  font-size: 12px;
  font-weight: 500;
  color: var(--text-muted);
}

.required {
  color: var(--danger);
}

.error {
  font-size: 12px;
  color: var(--danger-text);
}

.help {
  font-size: 12px;
  color: var(--text-subtle);
}
</style>
