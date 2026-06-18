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
  type OAuthKind,
  type OAuthStatus,
} from '@/api/oauth'
import AppButton from '@/components/ui/AppButton.vue'

const props = defineProps<{ kind: OAuthKind }>()
const emit = defineEmits<{ change: [loggedIn: boolean] }>()
const { show: toast } = useToast()

const status = ref<OAuthStatus | null>(null)
const logging = ref(false)
let cancelled = false

function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}
async function refresh() {
  try {
    status.value = await oauthStatus(props.kind)
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
    await oauthLogin(props.kind) // 长阻塞:浏览器授权完成/取消才返回
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
async function onLogout() {
  try {
    await oauthLogout(props.kind)
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
</style>
