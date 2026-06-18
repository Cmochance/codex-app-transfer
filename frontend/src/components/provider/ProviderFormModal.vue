<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { t, tFmt } from '@/i18n'
import { useProvidersStore } from '@/stores/providers'
import * as providersApi from '@/api/providers'
import type { ProviderPayload } from '@/api/types'
import AppModal from '@/components/ui/AppModal.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppInput from '@/components/ui/AppInput.vue'
import AppButton from '@/components/ui/AppButton.vue'
import SegmentedControl from '@/components/ui/SegmentedControl.vue'

// editId 为空 = 添加;非空 = 编辑(从 store 取数据 + 拉 secret 回填)
const props = defineProps<{ editId?: string | null }>()
const emit = defineEmits<{ close: []; saved: [] }>()
const store = useProvidersStore()

// Codex 槽位 → 上游模型 id 映射(对齐后端 models 字段 + 旧 providerFormDefaultRows)
const MODEL_SLOTS = [
  { key: 'default', label: t('models.defaultModel') },
  { key: 'gpt_5_5', label: 'gpt-5.5' },
  { key: 'gpt_5_4', label: 'gpt-5.4' },
  { key: 'gpt_5_4_mini', label: 'gpt-5.4-mini' },
  { key: 'gpt_5_3_codex', label: 'gpt-5.3-codex' },
  { key: 'gpt_5_2', label: 'gpt-5.2' },
] as const

const form = reactive({
  name: '',
  baseUrl: '',
  apiKey: '',
  apiFormat: 'openai_chat',
  authScheme: 'bearer',
  reviewModelSlot: '',
  models: {
    default: '',
    gpt_5_5: '',
    gpt_5_4: '',
    gpt_5_4_mini: '',
    gpt_5_3_codex: '',
    gpt_5_2: '',
  } as Record<string, string>,
  extraHeaders: '',
  modelCapabilities: '',
  requestOptions: '',
})
const saving = ref(false)
const error = ref('')
const showAdvanced = ref(false)

const formatOptions = [
  { value: 'openai_chat', label: 'OpenAI' },
  { value: 'responses', label: 'Responses' },
  { value: 'anthropic_messages', label: 'Claude' },
  { value: 'gemini_native', label: 'Gemini' },
]
const authOptions = [
  { value: 'bearer', label: 'Bearer' },
  { value: 'x-api-key', label: 'x-api-key' },
  { value: 'none', label: t('codex.statusNone') },
]
const isEdit = computed(() => !!props.editId)
const title = computed(() => (isEdit.value ? t('providerForm.titleEdit') : t('providerForm.titleAdd')))

onMounted(async () => {
  if (!props.editId) return
  if (!store.list.length) await store.load().catch(() => {})
  const p = store.list.find((x) => x.id === props.editId)
  if (!p) {
    error.value = t('providerForm.errNotFound')
    return
  }
  form.name = p.name
  form.baseUrl = p.baseUrl
  form.apiFormat = p.apiFormat
  form.authScheme = p.authScheme || 'bearer'
  form.reviewModelSlot = p.reviewModelSlot || ''
  for (const s of MODEL_SLOTS) {
    form.models[s.key] = (p.mappings as Record<string, string>)[s.key] || ''
  }
  const stringifyIfAny = (o: Record<string, unknown> | undefined) =>
    o && Object.keys(o).length ? JSON.stringify(o, null, 2) : ''
  form.extraHeaders = stringifyIfAny(p.extraHeaders)
  form.modelCapabilities = stringifyIfAny(p.modelCapabilities)
  form.requestOptions = stringifyIfAny(p.requestOptions)
  const secret = await providersApi.getProviderSecret(props.editId).catch(() => ({ apiKey: '' }))
  form.apiKey = secret.apiKey || ''
})

function parseJsonObj(label: string, raw: string): Record<string, unknown> | undefined {
  const trimmed = raw.trim()
  if (!trimmed) return undefined
  let v: unknown
  try {
    v = JSON.parse(trimmed)
  } catch {
    throw new Error(tFmt('providerForm.errJsonInvalid', { label }))
  }
  if (!v || typeof v !== 'object' || Array.isArray(v))
    throw new Error(tFmt('providerForm.errJsonNotObject', { label }))
  return v as Record<string, unknown>
}

async function save() {
  if (!form.name.trim() || !form.baseUrl.trim()) {
    error.value = t('providerForm.errRequired')
    return
  }
  let extraHeaders: Record<string, unknown> | undefined
  let modelCapabilities: Record<string, unknown> | undefined
  let requestOptions: Record<string, unknown> | undefined
  try {
    extraHeaders = parseJsonObj(t('providerForm.extraHeaders'), form.extraHeaders)
    modelCapabilities = parseJsonObj(t('providerForm.modelCapabilities'), form.modelCapabilities)
    requestOptions = parseJsonObj(t('providerForm.requestOptions'), form.requestOptions)
  } catch (e) {
    error.value = (e as Error).message
    return
  }
  // 全部 6 槽都发(空发 ""),对齐旧 normalized 行为,支持清空某槽回落默认
  const models: Record<string, string> = {}
  for (const s of MODEL_SLOTS) models[s.key] = form.models[s.key].trim()
  const payload: ProviderPayload = {
    name: form.name.trim(),
    baseUrl: form.baseUrl.trim(),
    apiKey: form.apiKey || undefined,
    apiFormat: form.apiFormat,
    authScheme: form.authScheme,
    models,
    reviewModelSlot: form.reviewModelSlot.trim() || null,
    extraHeaders: extraHeaders as Record<string, string> | undefined,
    modelCapabilities,
    requestOptions,
  }
  saving.value = true
  error.value = ''
  try {
    if (props.editId) await providersApi.updateProvider(props.editId, payload)
    else await providersApi.addProvider(payload)
    await store.load().catch(() => {})
    emit('saved')
    emit('close')
  } catch (e) {
    error.value = (e as Error).message || t('providerForm.errSaveFailed')
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <AppModal :title="title" @close="emit('close')">
    <div class="pf">
      <SettingsRow :title="t('providerForm.name')">
        <AppInput v-model="form.name" placeholder="My Provider" />
      </SettingsRow>
      <SettingsRow title="Base URL">
        <AppInput v-model="form.baseUrl" placeholder="https://api.example.com/v1" />
      </SettingsRow>
      <SettingsRow title="API Key" :description="isEdit ? t('providerForm.apiKeyEditHint') : ''">
        <AppInput v-model="form.apiKey" type="password" placeholder="sk-..." />
      </SettingsRow>
      <SettingsRow :title="t('providerForm.apiFormat')">
        <SegmentedControl v-model="form.apiFormat" :options="formatOptions" />
      </SettingsRow>
      <SettingsRow :title="t('providerForm.authScheme')">
        <SegmentedControl v-model="form.authScheme" :options="authOptions" />
      </SettingsRow>

      <div class="pf__section">{{ t('providerForm.modelMapSection') }}</div>
      <SettingsRow v-for="s in MODEL_SLOTS" :key="s.key" :title="s.label">
        <AppInput
          v-model="form.models[s.key]"
          :placeholder="s.key === 'default' ? 'gpt-4o' : t('providerForm.slotFallbackPlaceholder')"
        />
      </SettingsRow>
      <SettingsRow :title="t('providerForm.reviewModelSlot')" :description="t('providerForm.reviewModelSlotDesc')">
        <AppInput v-model="form.reviewModelSlot" placeholder="default" />
      </SettingsRow>

      <button type="button" class="pf__adv" @click="showAdvanced = !showAdvanced">
        {{ showAdvanced ? '▾' : '▸' }} {{ t('providerForm.advancedToggle') }}
      </button>
      <template v-if="showAdvanced">
        <div class="pf__field">
          <label>{{ t('providerForm.extraHeaders') }} extraHeaders</label>
          <textarea
            v-model="form.extraHeaders"
            class="pf__json"
            spellcheck="false"
            placeholder='{"X-Title": "..."}'
          ></textarea>
        </div>
        <div class="pf__field">
          <label>{{ t('providerForm.modelCapabilities') }} modelCapabilities</label>
          <textarea
            v-model="form.modelCapabilities"
            class="pf__json"
            spellcheck="false"
            placeholder='{"gpt-4o": {"context_window": 1000000}}'
          ></textarea>
        </div>
        <div class="pf__field">
          <label>{{ t('providerForm.requestOptions') }} requestOptions</label>
          <textarea v-model="form.requestOptions" class="pf__json" spellcheck="false"></textarea>
        </div>
      </template>

      <div v-if="error" class="pf__error">{{ error }}</div>
      <div class="pf__actions">
        <AppButton variant="ghost" :label="t('common.cancel')" @click="emit('close')" />
        <AppButton
          variant="primary"
          :label="saving ? t('providerForm.saving') : t('common.save')"
          :disabled="saving"
          @click="save"
        />
      </div>
    </div>
  </AppModal>
</template>

<style scoped>
.pf {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  max-height: 68vh;
  overflow-y: auto;
  min-width: 460px;
}
.pf__section {
  font-size: var(--fs-sm);
  font-weight: 600;
  color: var(--text-secondary);
  margin: var(--space-3) 0 var(--space-1);
  padding-left: var(--space-1);
}
.pf__adv {
  align-self: flex-start;
  margin-top: var(--space-3);
  padding: var(--space-1) 0;
  background: none;
  border: none;
  color: var(--accent);
  font-size: var(--fs-sm);
  cursor: pointer;
}
.pf__field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  padding: var(--space-2) 0;
}
.pf__field label {
  font-size: var(--fs-sm);
  color: var(--text-secondary);
}
.pf__json {
  width: 100%;
  min-height: 70px;
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius);
  background: var(--surface);
  color: var(--text);
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  line-height: 1.5;
  resize: vertical;
}
.pf__json:focus {
  outline: none;
  border-color: var(--accent);
}
.pf__error {
  color: var(--danger);
  font-size: var(--fs-sm);
  margin: var(--space-2) 0;
}
.pf__actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-3);
  margin-top: var(--space-4);
}
</style>
