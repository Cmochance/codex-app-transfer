<script setup lang="ts">
// WorkBuddy 账号池(单 provider 多账号 + 额度守护自动切换):列出池内账号,「添加账号」开浏览器
// 登录加一个进池(同 uid = 重登更新),每账号可「设为当前」/「移除」。当前服务账号(sticky)标
// 「当前」;额度低于守护阈值被跳过的账号标「额度不足」。需 provider 已保存(有 id)才能加账号。
import { onMounted, ref } from 'vue'
import { t } from '@/i18n'
import { useToast } from '@/composables/useToast'
import {
  oauthLogin,
  oauthCancelLogin,
  workbuddyPoolStatus,
  workbuddyRemoveAccount,
  workbuddySwitchAccount,
  type WorkbuddyAccount,
} from '@/api/oauth'
import AppButton from '@/components/ui/AppButton.vue'

const props = defineProps<{ providerId?: string }>()
const emit = defineEmits<{ change: [loggedIn: boolean] }>()
const { show: toast } = useToast()

const accounts = ref<WorkbuddyAccount[]>([])
const logging = ref(false)
let cancelled = false

function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}
function accountLabel(a: WorkbuddyAccount): string {
  return a.nickname || a.uid
}
async function refresh() {
  if (!props.providerId) {
    accounts.value = []
    emit('change', false)
    return
  }
  try {
    const s = await workbuddyPoolStatus(props.providerId)
    accounts.value = s.accounts ?? []
    emit('change', accounts.value.length > 0)
  } catch {
    accounts.value = []
    emit('change', false)
  }
}
onMounted(refresh)

async function onAddAccount() {
  if (!props.providerId) return
  logging.value = true
  cancelled = false
  try {
    const res = (await oauthLogin('workbuddy', props.providerId)) as {
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
    await oauthCancelLogin('workbuddy')
  } catch {
    /* 取消本身失败不影响 */
  }
}
async function onSetActive(uid: string) {
  if (!props.providerId) return
  try {
    await workbuddySwitchAccount(props.providerId, uid)
    await refresh()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}
async function onRemove(uid: string) {
  if (!props.providerId) return
  try {
    await workbuddyRemoveAccount(props.providerId, uid)
    await refresh()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}
</script>

<template>
  <div class="wbpool">
    <!-- provider 未保存:账号池无处安放 -->
    <p v-if="!props.providerId" class="wbpool__hint">{{ t('workbuddyPool.saveFirst') }}</p>
    <template v-else>
      <ul v-if="accounts.length" class="wbpool__list">
        <li v-for="a in accounts" :key="a.uid" class="wbpool__item">
          <span class="wbpool__name">{{ accountLabel(a) }}</span>
          <span v-if="a.isActive" class="wbpool__badge wbpool__badge--active">{{
            t('workbuddyPool.current')
          }}</span>
          <span v-if="a.exhausted" class="wbpool__badge wbpool__badge--low">{{
            t('workbuddyPool.exhausted')
          }}</span>
          <span class="wbpool__spacer" />
          <AppButton
            v-if="!a.isActive"
            size="sm"
            variant="ghost"
            :label="t('workbuddyPool.setCurrent')"
            @click="onSetActive(a.uid)"
          />
          <AppButton
            size="sm"
            variant="ghost"
            :label="t('workbuddyPool.remove')"
            @click="onRemove(a.uid)"
          />
        </li>
      </ul>
      <p v-else class="wbpool__hint">{{ t('workbuddyPool.noAccounts') }}</p>

      <div class="wbpool__actions">
        <template v-if="logging">
          <span class="wbpool__hint">{{ t('oauth.loggingIn') }}</span>
          <AppButton size="sm" variant="secondary" :label="t('common.cancel')" @click="onCancel" />
        </template>
        <AppButton
          v-else
          size="sm"
          variant="secondary"
          :label="t('workbuddyPool.addAccount')"
          @click="onAddAccount"
        />
      </div>
    </template>
  </div>
</template>

<style scoped>
.wbpool {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  width: 100%;
}
.wbpool__list {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  margin: 0;
  padding: 0;
  list-style: none;
}
.wbpool__item {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.wbpool__name {
  font-size: var(--fs-sm);
  color: var(--text-primary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 180px;
}
.wbpool__spacer {
  flex: 1 1 auto;
}
.wbpool__badge {
  font-size: var(--fs-xs);
  padding: 0 6px;
  border-radius: 4px;
  white-space: nowrap;
}
.wbpool__badge--active {
  color: var(--success);
  background: color-mix(in srgb, var(--success) 16%, transparent);
}
.wbpool__badge--low {
  color: var(--warning, #d9822b);
  background: color-mix(in srgb, var(--warning, #d9822b) 16%, transparent);
}
.wbpool__hint {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  margin: 0;
}
.wbpool__actions {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
</style>
