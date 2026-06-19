<script setup lang="ts">
const model = defineModel<string>({ default: '' })
defineProps<{ placeholder?: string; type?: string; disabled?: boolean }>()
</script>

<template>
  <input
    :type="type || 'text'"
    :placeholder="placeholder"
    v-model="model"
    :disabled="disabled"
    class="app-input"
    autocomplete="off"
    spellcheck="false"
  />
</template>

<style scoped>
.app-input {
  width: 260px;
  max-width: 100%;
  height: 30px;
  padding: 0 var(--space-3);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-size: var(--fs-base);
  font-family: inherit;
  transition: border-color var(--transition), box-shadow var(--transition);
}
.app-input::placeholder {
  color: var(--text-muted);
}
.app-input:disabled {
  color: var(--text-muted);
  background: var(--surface-2);
  cursor: not-allowed;
}
.app-input:focus {
  outline: none;
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}
</style>
