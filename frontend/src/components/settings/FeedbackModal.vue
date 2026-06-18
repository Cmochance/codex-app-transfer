<script setup lang="ts">
// 用户反馈 — 恢复旧版反馈功能,接后端 /api/feedback(Cloudflare worker + 节流 + 自动附诊断/日志)。
import { ref } from 'vue'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { submitFeedback } from '@/api/system'
import AppModal from '@/components/ui/AppModal.vue'
import AppInput from '@/components/ui/AppInput.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'

const emit = defineEmits<{ close: [] }>()
const { show: toast } = useToast()

const title = ref('')
const contactEmail = ref('')
const body = ref('')
const includeDiagnostics = ref(true)
const submitting = ref(false)

async function onSubmit() {
  if (!body.value.trim()) {
    toast(t('feedback.bodyRequired'), 'error')
    return
  }
  submitting.value = true
  try {
    const res = await submitFeedback({
      title: title.value.trim(),
      contact_email: contactEmail.value.trim(),
      body: body.value.trim(),
      include_diagnostics: includeDiagnostics.value,
    })
    toast(tFmt('feedback.successToast', { id: res.id || '' }))
    emit('close')
  } catch (e) {
    toast(tFmt('feedback.failToast', { message: (e as Error).message || '' }), 'error')
  } finally {
    submitting.value = false
  }
}
</script>

<template>
  <AppModal :title="t('feedback.title')" wide @close="emit('close')">
    <div class="fb">
      <p class="fb__intro">{{ t('feedback.intro') }}</p>

      <label class="fb__field">
        <span class="fb__label">{{ t('feedback.titleLabel') }}</span>
        <AppInput v-model="title" :placeholder="t('feedback.titlePlaceholder')" />
      </label>

      <label class="fb__field">
        <span class="fb__label">{{ t('feedback.bodyLabel') }}</span>
        <textarea
          v-model="body"
          class="fb__textarea"
          :placeholder="t('feedback.bodyPlaceholder')"
          rows="6"
          spellcheck="false"
        ></textarea>
      </label>

      <label class="fb__field">
        <span class="fb__label">{{ t('feedback.contactEmailLabel') }}</span>
        <AppInput v-model="contactEmail" :placeholder="t('feedback.contactEmailPlaceholder')" />
        <span class="fb__hint">{{ t('feedback.contactEmailHint') }}</span>
      </label>

      <div class="fb__diag">
        <div class="fb__diag-text">
          <span class="fb__label">{{ t('feedback.includeDiagnostics') }}</span>
          <span class="fb__hint">{{ t('feedback.includeDiagnosticsHint') }}</span>
        </div>
        <AppSwitch v-model="includeDiagnostics" />
      </div>

      <div class="fb__actions">
        <AppButton variant="ghost" :label="t('common.cancel')" @click="emit('close')" />
        <AppButton
          variant="primary"
          :label="submitting ? t('feedback.submitting') : t('feedback.submit')"
          :disabled="submitting"
          @click="onSubmit"
        />
      </div>
    </div>
  </AppModal>
</template>

<style scoped>
.fb {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  min-width: 420px;
  max-height: 70vh;
  overflow-y: auto;
}
.fb__intro {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  line-height: 1.5;
  margin: 0;
}
.fb__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.fb__label {
  font-size: var(--fs-sm);
  font-weight: 550;
  color: var(--text);
}
.fb__hint {
  font-size: var(--fs-xs);
  color: var(--text-muted);
  line-height: 1.4;
}
.fb__textarea {
  width: 100%;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-family: inherit;
  font-size: var(--fs-base);
  line-height: 1.5;
  resize: vertical;
}
.fb__textarea:focus {
  outline: none;
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}
.fb__diag {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
  padding: var(--space-2) 0;
}
.fb__diag-text {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.fb__actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-3);
  margin-top: var(--space-2);
}
</style>
