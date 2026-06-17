<script setup lang="ts">
import type { Provider } from '@/api/types'
import { t } from '@/i18n'
import AppButton from '@/components/ui/AppButton.vue'
import IconGrip from '~icons/lucide/grip-vertical'
import IconPlay from '~icons/lucide/play'
import IconPencil from '~icons/lucide/square-pen'
import IconTrash from '~icons/lucide/trash-2'
import IconRadio from '~icons/lucide/radio'
import IconPlug from '~icons/lucide/plug'

defineProps<{ provider: Provider }>()
defineEmits<{ enable: []; edit: []; remove: [] }>()
</script>

<template>
  <article class="pcard" :class="{ 'pcard--active': provider.default }">
    <!-- 诉求2: 左侧拖拽手柄 -->
    <span class="pcard__grip" aria-hidden="true"><IconGrip /></span>
    <img v-if="provider.logo" :src="`/${provider.logo}`" class="pcard__logo" alt="" />
    <span v-else class="pcard__logo pcard__logo--fallback"><IconPlug /></span>
    <div class="pcard__main">
      <strong class="pcard__name">{{ provider.name }}</strong>
      <span class="pcard__url">{{ provider.baseUrl }}</span>
    </div>
    <span v-if="provider.default" class="pcard__badge"><IconRadio />{{ t('status.active') }}</span>
    <!-- 诉求1: 只保留 启用/编辑/删除 三个, 统一图标+文字 -->
    <div class="pcard__actions">
      <AppButton
        :variant="provider.default ? 'secondary' : 'primary'"
        size="sm"
        :icon="IconPlay"
        :label="t('providers.enable')"
        @click="$emit('enable')"
      />
      <AppButton variant="ghost" size="sm" :icon="IconPencil" :label="t('common.edit')" @click="$emit('edit')" />
      <AppButton variant="danger" size="sm" :icon="IconTrash" :label="t('common.delete')" @click="$emit('remove')" />
    </div>
  </article>
</template>

<style scoped>
.pcard {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  transition: border-color var(--transition), box-shadow var(--transition);
}
.pcard--active {
  border-color: var(--accent);
  box-shadow: inset 0 0 0 1px var(--accent);
}
.pcard__grip {
  display: grid;
  place-items: center;
  color: var(--text-muted);
  cursor: grab;
  opacity: 0.5;
}
.pcard__grip:active {
  cursor: grabbing;
}
.pcard__grip :deep(svg) {
  width: 16px;
  height: 16px;
}
.pcard__logo {
  width: 32px;
  height: 32px;
  border-radius: var(--radius);
  object-fit: cover;
  flex-shrink: 0;
}
.pcard__logo--fallback {
  display: grid;
  place-items: center;
  background: var(--surface-2);
  color: var(--text-muted);
}
.pcard__main {
  display: flex;
  flex-direction: column;
  min-width: 0;
  flex: 1;
}
.pcard__name {
  font-size: var(--fs-md);
  font-weight: 600;
}
.pcard__url {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.pcard__badge {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: var(--fs-sm);
  color: var(--success);
  flex-shrink: 0;
}
.pcard__badge :deep(svg) {
  width: 13px;
  height: 13px;
}
.pcard__actions {
  display: flex;
  gap: var(--space-2);
  flex-shrink: 0;
}
</style>
