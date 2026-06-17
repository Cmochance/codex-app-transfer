<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useProvidersStore } from '@/stores/providers'
import * as providersApi from '@/api/providers'
import type { ProviderPayload } from '@/api/types'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppInput from '@/components/ui/AppInput.vue'
import AppButton from '@/components/ui/AppButton.vue'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'

const route = useRoute()
const router = useRouter()
const store = useProvidersStore()
const editId = route.query.id as string | undefined

const form = reactive({
  name: '',
  baseUrl: '',
  apiKey: '',
  apiFormat: 'openai_chat',
  defaultModel: '',
})
const saving = ref(false)
const error = ref('')

const formatOptions: { value: string; label: string }[] = [
  { value: 'openai_chat', label: 'OpenAI' },
  { value: 'responses', label: 'Responses' },
  { value: 'anthropic_messages', label: 'Claude' },
  { value: 'gemini_native', label: 'Gemini' },
]

onMounted(async () => {
  if (!editId) return
  if (!store.list.length) await store.load().catch(() => {})
  const p = store.list.find((x) => x.id === editId)
  if (!p) return
  form.name = p.name
  form.baseUrl = p.baseUrl
  form.apiFormat = p.apiFormat
  form.defaultModel = p.mappings.default
  const secret = await providersApi.getProviderSecret(editId).catch(() => ({ apiKey: '' }))
  form.apiKey = secret.apiKey || ''
})

async function save() {
  if (!form.name || !form.baseUrl) {
    error.value = '名称和 Base URL 必填'
    return
  }
  saving.value = true
  error.value = ''
  const payload: ProviderPayload = {
    name: form.name,
    baseUrl: form.baseUrl,
    apiKey: form.apiKey || undefined,
    apiFormat: form.apiFormat,
    models: form.defaultModel ? { default: form.defaultModel } : {},
  }
  try {
    if (editId) await providersApi.updateProvider(editId, payload)
    else await providersApi.addProvider(payload)
    await store.load().catch(() => {})
    router.push('/providers')
  } catch (e) {
    error.value = (e as Error).message || '保存失败'
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <div>
    <h1 class="page-title">{{ editId ? '编辑提供商' : '添加提供商' }}</h1>

    <SettingsGroup title="基本信息">
      <SettingsRow title="名称">
        <AppInput v-model="form.name" placeholder="My Provider" />
      </SettingsRow>
      <SettingsRow title="Base URL">
        <AppInput v-model="form.baseUrl" placeholder="https://api.example.com/v1" />
      </SettingsRow>
      <SettingsRow title="API Key" :description="editId ? '留空保持原 key 不变' : ''">
        <AppInput v-model="form.apiKey" type="password" placeholder="sk-..." />
      </SettingsRow>
      <SettingsRow title="协议格式">
        <SegmentedControl v-model="form.apiFormat" :options="formatOptions" />
      </SettingsRow>
      <SettingsRow title="默认模型" description="对应 Codex default 槽位">
        <AppInput v-model="form.defaultModel" placeholder="gpt-4o" />
      </SettingsRow>
    </SettingsGroup>

    <div v-if="error" class="form-error">{{ error }}</div>

    <div class="page-actions">
      <AppButton variant="ghost" label="取消" @click="router.push('/providers')" />
      <AppButton variant="primary" :label="saving ? '保存中…' : '保存'" :disabled="saving" @click="save" />
    </div>
  </div>
</template>

<style scoped>
.page-title {
  font-size: var(--fs-xl);
  font-weight: 600;
  margin: 0 0 var(--space-4);
}
.form-error {
  color: var(--danger);
  font-size: var(--fs-sm);
  margin: var(--space-2) 0;
}
.page-actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-3);
  margin-top: var(--space-4);
}
</style>
