<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useProvidersStore } from '@/stores/providers'
import { t } from '@/i18n'
import { restartCodexApp } from '@/api/desktop'
import {
  cancelRealAccountLogin,
  getRealAccountStatus,
  startRealAccountLogin,
} from '@/api/desktop'
import { useToast } from '@/composables/useToast'
import ProviderCard from '@/components/provider/ProviderCard.vue'
import OfficialCodexCard from '@/components/provider/OfficialCodexCard.vue'
import ProviderFormModal from '@/components/provider/ProviderFormModal.vue'
import AppButton from '@/components/ui/AppButton.vue'
import IconPlus from '~icons/lucide/plus'
import IconRefreshCw from '~icons/lucide/refresh-cw'

const store = useProvidersStore()
const { show: toast } = useToast()
const formOpen = ref(false)
const formEditId = ref<string | null>(null)
const officialLoggedIn = ref(false)
const officialLoginRunning = ref(false)
const officialBusy = ref(false)
let officialLoginPoll: number | undefined

async function refreshOfficialStatus() {
  const status = await getRealAccountStatus()
  officialLoggedIn.value = status.loggedIn
  officialLoginRunning.value = status.loginState === 'running'
  return status
}

function stopOfficialLoginPoll() {
  if (officialLoginPoll !== undefined) {
    window.clearInterval(officialLoginPoll)
    officialLoginPoll = undefined
  }
}

async function activateOfficial() {
  officialBusy.value = true
  try {
    await store.activateOfficial()
    await refreshOfficialStatus()
    toast(t('providers.officialSwitchSuccess'))
  } catch (e) {
    toast((e as Error).message || t('providers.officialSwitchFailed'), 'error')
  } finally {
    officialBusy.value = false
  }
}

async function pollOfficialLoginOnce() {
  try {
    const status = await refreshOfficialStatus()
    if (status.loginState === 'succeeded') {
      stopOfficialLoginPoll()
      await activateOfficial()
    } else if (status.loginState === 'failed' || status.loginState === 'cancelled') {
      stopOfficialLoginPoll()
      toast(status.loginMessage || t('providers.officialLoginFailed'), 'error')
    }
  } catch {
    // 登录窗口仍可能正常进行；短暂状态读取失败时继续轮询。
  }
}

function startOfficialLoginPoll() {
  stopOfficialLoginPoll()
  officialLoginPoll = window.setInterval(() => void pollOfficialLoginOnce(), 2000)
}

async function onOfficialLogin() {
  officialBusy.value = true
  try {
    await startRealAccountLogin()
    officialLoginRunning.value = true
    startOfficialLoginPoll()
  } catch (e) {
    toast((e as Error).message || t('providers.officialLoginFailed'), 'error')
  } finally {
    officialBusy.value = false
  }
}

async function onOfficialCancelLogin() {
  stopOfficialLoginPoll()
  officialLoginRunning.value = false
  try {
    await cancelRealAccountLogin()
  } catch (e) {
    toast((e as Error).message || t('providers.officialLoginFailed'), 'error')
  }
}

onMounted(async () => {
  await Promise.all([store.load(), refreshOfficialStatus().catch(() => undefined)])
  if (officialLoginRunning.value) startOfficialLoginPoll()
})
onUnmounted(stopOfficialLoginPoll)

function openAdd() {
  formEditId.value = null
  formOpen.value = true
}

// 已启用(default)的提供商置顶,其余保持后端顺序
const displayList = computed(() => {
  const def = store.list.filter((p) => p.default)
  const rest = store.list.filter((p) => !p.default)
  return [...def, ...rest]
})

// 诉求2: HTML5 拖拽排序(复刻旧 enableProviderReorder 语义), drop 后乐观更新 + 持久化
const draggingId = ref<string | null>(null)
function onDragStart(id: string, e: DragEvent) {
  draggingId.value = id
  // ⚠️ WKWebView(macOS)必须在 dragstart 写 dataTransfer, 否则 drop 事件根本不触发,
  // 表现为「拖得动但松手位置不变」(本次 bug 根因)。setData + effectAllowed 缺一不可。
  if (e.dataTransfer) {
    e.dataTransfer.effectAllowed = 'move'
    e.dataTransfer.setData('text/plain', id)
  }
}
function onDragEnd() {
  draggingId.value = null
}
function onDrop(targetId: string) {
  const from = draggingId.value
  draggingId.value = null
  if (!from || from === targetId) return
  const ids = displayList.value.map((p) => p.id)
  const fi = ids.indexOf(from)
  const ti = ids.indexOf(targetId)
  if (fi < 0 || ti < 0) return
  ids.splice(fi, 1) // 先移除被拖项
  const tAfter = ids.indexOf(targetId) // 移除后目标的新索引
  ids.splice(fi < ti ? tAfter + 1 : tAfter, 0, from) // 下拖落目标之后 / 上拖落之前
  store.reorder(ids)
}

async function onRestartCodexApp() {
  try {
    await restartCodexApp()
    toast(t('toast.codexAppRestartRequested'))
  } catch (e) {
    toast((e as Error).message || t('toast.codexAppRestartFailed'), 'error')
  }
}

async function onEnable(id: string) {
  try {
    await store.setDefault(id)
    toast(t('providers.providerSwitchSuccess'))
  } catch (e) {
    toast((e as Error).message || t('providers.enableFailed'), 'error')
  }
}
function onEdit(id: string) {
  formEditId.value = id
  formOpen.value = true
}
function onRemove(id: string) {
  if (window.confirm(t('providers.deleteMessage'))) store.remove(id)
}
</script>

<template>
  <div class="providers">
    <div class="providers__header">
      <AppButton
        variant="secondary"
        size="sm"
        :icon="IconRefreshCw"
        :label="t('providers.restartCodexApp')"
        @click="onRestartCodexApp"
      />
      <AppButton
        variant="primary"
        size="sm"
        :icon="IconPlus"
        :label="t('providers.add')"
        @click="openAdd"
      />
    </div>

    <div v-if="store.loading" class="providers__hint">{{ t('providers.loading') }}</div>
    <div v-else-if="store.error" class="providers__hint providers__hint--err">{{ store.error }}</div>
    <div v-else class="providers__list">
      <OfficialCodexCard
        :active="store.routingMode === 'official'"
        :logged-in="officialLoggedIn"
        :login-running="officialLoginRunning"
        :busy="officialBusy"
        @enable="activateOfficial"
        @login="onOfficialLogin"
        @cancel="onOfficialCancelLogin"
      />
      <div v-if="!store.list.length" class="providers__hint">{{ t('providers.empty') }}</div>
      <div
        v-for="p in displayList"
        :key="p.id"
        class="providers__item"
        :class="{ 'is-dragging': draggingId === p.id }"
        draggable="true"
        @dragstart="onDragStart(p.id, $event)"
        @dragend="onDragEnd"
        @dragover.prevent
        @drop.prevent="onDrop(p.id)"
      >
        <ProviderCard
          :provider="p"
          @enable="onEnable(p.id)"
          @edit="onEdit(p.id)"
          @remove="onRemove(p.id)"
        />
      </div>
    </div>

    <ProviderFormModal
      v-if="formOpen"
      :edit-id="formEditId"
      @close="formOpen = false"
      @saved="toast(t('providers.saved'))"
    />
  </div>
</template>

<style scoped>
.providers__header {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: var(--space-2);
  margin-bottom: var(--space-4);
}
.providers__list {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.providers__item {
  transition: opacity var(--transition);
}
.is-dragging {
  opacity: 0.4;
}
.providers__hint {
  color: var(--text-muted);
  padding: var(--space-6) 0;
  text-align: center;
}
.providers__hint--err {
  color: var(--danger);
}
</style>
