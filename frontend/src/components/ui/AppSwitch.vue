<script setup lang="ts">
const model = defineModel<boolean>({ default: false })
defineProps<{ disabled?: boolean }>()
</script>

<template>
  <button
    type="button"
    class="app-switch"
    :class="{ 'app-switch--on': model }"
    role="switch"
    :aria-checked="model"
    :disabled="disabled"
    @click="model = !model"
  >
    <span class="app-switch__knob" />
  </button>
</template>

<style scoped>
.app-switch {
  position: relative;
  width: 38px;
  height: 22px;
  padding: 0;
  border: none;
  border-radius: var(--radius-full);
  background: var(--border-strong);
  transition: background var(--transition);
}
.app-switch:disabled {
  opacity: 0.5;
  cursor: default;
}
.app-switch--on {
  background: var(--accent);
}
.app-switch__knob {
  position: absolute;
  top: 2px;
  left: 2px;
  width: 18px;
  height: 18px;
  border-radius: var(--radius-full);
  background: #fff;
  box-shadow: var(--shadow-sm);
  transition: transform var(--transition);
}
.app-switch--on .app-switch__knob {
  transform: translateX(16px);
}
</style>
