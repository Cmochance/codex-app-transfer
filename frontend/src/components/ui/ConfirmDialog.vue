<script setup lang="ts">
import { t } from '@/i18n'
import { useConfirm } from '@/composables/useConfirm'
import AppModal from '@/components/ui/AppModal.vue'
import AppButton from '@/components/ui/AppButton.vue'

// 全局唯一确认弹窗,在 App.vue 挂一次,由 useConfirm() 的 state 驱动。
const { state, respond } = useConfirm()
</script>

<template>
  <AppModal
    v-if="state.open"
    :title="state.title || t('common.confirmTitle')"
    @close="respond(false)"
  >
    <div class="confirm">
      <p class="confirm__msg">{{ state.message }}</p>
      <div class="confirm__actions">
        <AppButton
          variant="ghost"
          :label="state.cancelLabel || t('common.cancel')"
          @click="respond(false)"
        />
        <AppButton
          :variant="state.danger ? 'danger' : 'primary'"
          :label="state.confirmLabel || t('common.confirm')"
          @click="respond(true)"
        />
      </div>
    </div>
  </AppModal>
</template>

<style scoped>
.confirm {
  display: flex;
  flex-direction: column;
  gap: 16px;
  min-width: 320px;
  max-width: 440px;
}
.confirm__msg {
  margin: 0;
  font-size: 14px;
  line-height: 1.6;
  white-space: pre-wrap;
  word-break: break-word;
}
.confirm__actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
</style>
