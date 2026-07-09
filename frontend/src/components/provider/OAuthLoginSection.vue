<script setup lang="ts">
// OAuth 账号登录区(替代 API Key):未登录显示「登录」(POST 开浏览器授权,长阻塞),
// 登录中显示「登录中…+取消」,已登录显示邮箱 + 「登出」。状态变化 emit 给父表单。
import { onMounted, ref } from 'vue'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import {
  oauthStatus,
  oauthLogin,
  oauthCancelLogin,
  oauthLogout,
  oauthSubmitCode,
  type OAuthKind,
  type OAuthStatus,
} from '@/api/oauth'
import AppButton from '@/components/ui/AppButton.vue'

// providerId:仅 trae 这类「按 provider 条目隔离多账号」的 kind 需要(= 当前编辑的
// provider id)。trae 登录 login-first:未保存(无 id)时登录写 pending,保存 provider 时
// 由父表单 claim 绑定到新 id —— 无需先保存。其余 kind(zai/gemini/...)忽略 providerId。
const props = defineProps<{ kind: OAuthKind; providerId?: string }>()
const emit = defineEmits<{ change: [loggedIn: boolean] }>()
const { show: toast } = useToast()

const status = ref<OAuthStatus | null>(null)
const logging = ref(false)
let cancelled = false
// [MOC-300] 仅 grokBuild:xAI 授权页显示 code 让用户粘回(不跳 loopback),登录中态提供输入框。
const manualCode = ref('')
const submittingCode = ref(false)

function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}
async function refresh() {
  try {
    status.value = await oauthStatus(props.kind, props.providerId)
    emit('change', !!status.value?.loggedIn)
  } catch {
    status.value = { loggedIn: false }
    emit('change', false)
  }
}
onMounted(refresh)

async function onLogin() {
  logging.value = true
  cancelled = false
  try {
    // 长阻塞:浏览器授权完成/取消才返回。部分上游(zai/bigmodel/google)登录失败时
    // 返回 HTTP 200 {loggedIn:false, error},api() 不抛 → 需显式读出 error 提示,
    // 否则失败看起来像无操作。
    const res = (await oauthLogin(props.kind, props.providerId)) as {
      loggedIn?: boolean
      error?: string
    } | null
    if (res && res.loggedIn === false && res.error && !cancelled) {
      toast(res.error, 'error')
    }
    await refresh()
  } catch (e) {
    if (!cancelled) toast(errMsg(e) || t('oauth.loginFailed'), 'error')
  } finally {
    logging.value = false
  }
}
async function onCancel() {
  cancelled = true
  try {
    await oauthCancelLogin(props.kind)
  } catch {
    /* 取消本身失败不影响:login 那边会自行结束 */
  }
}
// grokBuild 手动粘 code:送到后端等待中的 login,由它换 token(login 随即返回并 refresh)。
async function onSubmitCode() {
  const code = manualCode.value.trim()
  if (!code || submittingCode.value) return
  submittingCode.value = true
  try {
    const res = await oauthSubmitCode(props.kind, code)
    if (!res?.accepted) {
      toast(res?.error || t('oauth.submitCodeFailed'), 'error')
    } else {
      manualCode.value = ''
    }
  } catch (e) {
    toast(errMsg(e), 'error')
  } finally {
    submittingCode.value = false
  }
}
async function onLogout() {
  try {
    await oauthLogout(props.kind, props.providerId)
    await refresh()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}
</script>

<template>
  <div class="oauth">
    <template v-if="logging">
      <span class="oauth__msg">{{ t('oauth.loggingIn') }}</span>
      <AppButton size="sm" variant="secondary" :label="t('common.cancel')" @click="onCancel" />
      <template v-if="kind === 'grokBuild'">
        <input
          v-model="manualCode"
          class="oauth__code-input"
          :placeholder="t('oauth.pasteCodePlaceholder')"
          spellcheck="false"
          autocomplete="off"
          @keyup.enter="onSubmitCode"
        />
        <AppButton
          size="sm"
          variant="primary"
          :label="t('oauth.submitCode')"
          :disabled="!manualCode.trim() || submittingCode"
          @click="onSubmitCode"
        />
        <span class="oauth__hint">{{ t('oauth.pasteCodeHint') }}</span>
      </template>
    </template>
    <template v-else-if="status?.loggedIn">
      <span class="oauth__msg oauth__msg--ok">{{
        status.email ? tFmt('oauth.loggedInAs', { email: status.email }) : t('oauth.loggedIn')
      }}</span>
      <AppButton size="sm" variant="secondary" :label="t('oauth.logout')" @click="onLogout" />
    </template>
    <template v-else>
      <span class="oauth__msg">{{ t('oauth.notLoggedIn') }}</span>
      <AppButton size="sm" variant="secondary" :label="t('oauth.login')" @click="onLogin" />
    </template>
  </div>
</template>

<style scoped>
.oauth {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.oauth__msg {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  white-space: nowrap;
}
.oauth__msg--ok {
  color: var(--success);
}
.oauth__code-input {
  flex: 1 1 180px;
  min-width: 120px;
  font-size: var(--fs-sm);
  font-family: var(--font-mono, monospace);
  padding: var(--space-1) var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--surface);
  color: var(--text);
}
.oauth__hint {
  flex-basis: 100%;
  font-size: var(--fs-xs, 11px);
  color: var(--text-muted);
}
</style>
