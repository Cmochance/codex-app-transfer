<script setup lang="ts" generic="T extends string | number">
defineProps<{
  options: { value: T; label: string }[]
}>()
const model = defineModel<T>()
</script>

<template>
  <div class="segmented" role="tablist">
    <button
      v-for="opt in options"
      :key="String(opt.value)"
      type="button"
      role="tab"
      class="segmented__item"
      :class="{ 'segmented__item--active': model === opt.value }"
      :aria-selected="model === opt.value"
      @click="model = opt.value"
    >
      {{ opt.label }}
    </button>
  </div>
</template>

<style scoped>
.segmented {
  display: inline-flex;
  padding: 2px;
  gap: 2px;
  background: var(--surface-2);
  border: 1px solid var(--border);
  border-radius: var(--radius);
}
.segmented__item {
  height: 26px;
  padding: 0 var(--space-3);
  border: none;
  background: transparent;
  color: var(--text-secondary);
  font-size: var(--fs-sm);
  font-weight: 500;
  border-radius: var(--radius-sm);
  transition: background var(--transition), color var(--transition);
}
.segmented__item:hover {
  color: var(--text);
}
.segmented__item--active {
  background: var(--surface);
  color: var(--accent);
  font-weight: 600;
  box-shadow: var(--shadow-sm);
}
</style>
