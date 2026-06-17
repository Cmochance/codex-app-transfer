<script setup lang="ts">
// FineTune 风设置行: 左(标题 + 灰色描述) + 右(控件 slot)
defineProps<{ title?: string; description?: string }>()
</script>

<template>
  <div class="settings-row">
    <div class="settings-row__text">
      <div class="settings-row__title"><slot name="title">{{ title }}</slot></div>
      <div v-if="description || $slots.description" class="settings-row__desc">
        <slot name="description">{{ description }}</slot>
      </div>
    </div>
    <div class="settings-row__control"><slot /></div>
  </div>
</template>

<style scoped>
.settings-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
  padding: var(--space-4);
}
.settings-row__title {
  font-size: var(--fs-md);
  font-weight: 550;
  color: var(--text);
}
.settings-row__desc {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  margin-top: 2px;
  line-height: 1.4;
}
.settings-row__control {
  flex-shrink: 0;
}
</style>
