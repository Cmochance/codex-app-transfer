<script setup lang="ts" generic="T extends string | number">
// 自定义单选下拉(替代原生 <select>):trigger 显示选中项,面板上沿贴 trigger 下沿、
// 固定高度可滚。选项 hover 高亮、当前项打勾。
import { computed } from 'vue'
import AppDropdown from './AppDropdown.vue'
import IconChevronDown from '~icons/lucide/chevron-down'
import IconCheck from '~icons/lucide/check'

const props = defineProps<{ options: { value: T; label: string }[]; align?: 'left' | 'right' }>()
const model = defineModel<T>()
const selectedLabel = computed(
  () => props.options.find((o) => o.value === model.value)?.label ?? '',
)
</script>

<template>
  <AppDropdown class="app-select" :align="props.align">
    <template #trigger="{ open }">
      <button type="button" class="app-select__trigger" :class="{ open }">
        <span class="app-select__label">{{ selectedLabel }}</span>
        <IconChevronDown class="app-select__chevron" />
      </button>
    </template>
    <template #default="{ close }">
      <button
        v-for="opt in options"
        :key="String(opt.value)"
        type="button"
        class="app-select__option"
        :class="{ sel: opt.value === model }"
        @click="model = opt.value;close()"
      >
        <IconCheck
          class="app-select__check"
          :style="{ visibility: opt.value === model ? 'visible' : 'hidden' }"
        />
        <span class="app-select__opt-label">{{ opt.label }}</span>
      </button>
    </template>
  </AppDropdown>
</template>

<style scoped>
.app-select__trigger {
  display: inline-flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-2);
  width: 100%;
  height: 30px;
  padding: 0 var(--space-2);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-size: var(--fs-sm);
}
.app-select__trigger.open {
  border-color: var(--accent);
}
.app-select__label {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.app-select__chevron {
  width: 14px;
  height: 14px;
  flex-shrink: 0;
  color: var(--text-muted);
}
.app-select__option {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  width: 100%;
  padding: var(--space-2);
  border: none;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text);
  font-size: var(--fs-sm);
  text-align: left;
  white-space: nowrap;
}
.app-select__option:hover {
  background: var(--surface-hover);
}
.app-select__option.sel {
  color: var(--accent);
  font-weight: 600;
}
.app-select__check {
  width: 13px;
  height: 13px;
  flex-shrink: 0;
  color: var(--accent);
}
.app-select__opt-label {
  flex: 1;
}
</style>
