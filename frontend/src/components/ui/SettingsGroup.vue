<script setup lang="ts">
// FineTune 风分组: 粗体 section 标题 + 圆角白卡片(内含多行 SettingsRow, 行间细分隔线)
defineProps<{ title?: string }>()
</script>

<template>
  <section class="settings-group">
    <h2 v-if="title" class="settings-group__title">{{ title }}</h2>
    <div class="settings-group__card">
      <slot />
    </div>
  </section>
</template>

<style scoped>
.settings-group {
  margin-bottom: var(--space-6);
}
.settings-group__title {
  font-size: var(--fs-lg);
  font-weight: 600;
  margin: 0 0 var(--space-3);
  padding-left: var(--space-1);
}
.settings-group__card {
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  overflow: hidden;
}
/* 卡片内相邻设置行之间的细分隔线 */
.settings-group__card :deep(.settings-row + .settings-row) {
  border-top: 1px solid var(--border);
}
</style>
