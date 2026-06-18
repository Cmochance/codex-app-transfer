<script setup lang="ts" generic="T extends string | number">
// 纯受控:高亮始终跟随 modelValue prop(不留本地副本)。点击只 emit,由父级决定是否更新值
// —— 父级若拒绝/异步门控(如 webFetch),高亮不会错位停在被拒绝的档。v-model 用法不受影响。
const props = defineProps<{
  modelValue?: T
  options: { value: T; label: string }[]
}>()
const emit = defineEmits<{ 'update:modelValue': [T] }>()
</script>

<template>
  <div class="segmented" role="tablist">
    <button
      v-for="opt in options"
      :key="String(opt.value)"
      type="button"
      role="tab"
      class="segmented__item"
      :class="{ 'segmented__item--active': props.modelValue === opt.value }"
      :aria-selected="props.modelValue === opt.value"
      @click="emit('update:modelValue', opt.value)"
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
