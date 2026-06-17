<script setup lang="ts">
// Codex Desktop 主题注入页 — 移植旧 app.js renderTheme + bindThemeEvents(#264 / MOC-102)。
// 主题 grid + 选中/隐藏/删除/上传(1:1 crop)+ 徽章派生(绝不暴露 raw CDP 502)+ 重启对话框。
import { computed, onMounted, ref, watch } from 'vue'
import { i18nState, t, tFmt } from '@/i18n'
import { useSettingsStore } from '@/stores/settings'
import type { Settings } from '@/api/settings'
import { useToast } from '@/composables/useToast'
import {
  themeList,
  themeStatus,
  themeApply,
  themeUploadCustom,
  themeDeleteCustom,
  restartCodexApp,
  type ThemeEntry,
} from '@/api/desktop'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppModal from '@/components/ui/AppModal.vue'
import ThemeCropModal from '@/components/codex/ThemeCropModal.vue'
import IconChevronLeft from '~icons/lucide/chevron-left'
import IconRefreshCw from '~icons/lucide/refresh-cw'
import IconPlus from '~icons/lucide/plus'

const store = useSettingsStore()
const { show: toast } = useToast()

const themes = ref<ThemeEntry[]>([])
const badge = ref('')
const cropSrc = ref<string | null>(null)
const showRestart = ref(false)

// 开关本地镜像 + 守卫:AppSwitch 直接 v-model;watch 里做异步落盘/校验,拒绝时回退。
const enabledModel = ref(false)
let internalSet = false
function setEnabled(v: boolean) {
  // 值不变则不 arm 守卫(否则 watch 不触发、标志残留会吞掉下一次用户操作)
  if (enabledModel.value === v) return
  internalSet = true
  enabledModel.value = v
}

const selectedId = computed<string | null>(() => store.str('codexUiTheme') || null)
const hiddenIds = computed<string[]>(() =>
  Array.isArray(store.settings.themeHiddenIds) ? (store.settings.themeHiddenIds as string[]) : [],
)
const visibleThemes = computed(() => themes.value.filter((th) => !hiddenIds.value.includes(th.id)))
const hiddenCount = computed(() => hiddenIds.value.length)
const hasCustomVisible = computed(() => visibleThemes.value.some((th) => th.id === 'custom'))

function displayName(th: ThemeEntry): string {
  return i18nState.locale === 'en' ? th.displayNameEn : th.displayNameZh
}
function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}

onMounted(async () => {
  if (!store.loaded) await store.load().catch(() => {})
  setEnabled(store.bool('codexUiThemeEnabled', false))
  await loadThemes()
  await refreshBadge()
})

async function loadThemes() {
  // 每次重拉不缓存(避免 v1 cache-empty bug);列表仅 5-6 项,走本地 IPC 延迟可忽略。
  try {
    const res = await themeList()
    themes.value = res.themes || []
    if (themes.value.length === 0) toast(t('theme.listEmpty'), 'error')
  } catch (e) {
    themes.value = []
    toast(`${t('theme.loadFailed')}: ${errMsg(e)}`, 'error')
  }
}

// MOC-102:badge 完全由「开关偏好 + 后端 status」推导,绝不把 raw 502 暴露给 user。
async function refreshBadge() {
  try {
    const st = await themeStatus()
    const sObj = st.status
    const reapplyOrPending = async () => {
      try {
        await themeApply(selectedId.value!)
        badge.value = `${t('theme.applied')}: ${selectedId.value}`
      } catch {
        badge.value = t('theme.pendingRestart')
      }
    }
    if (!enabledModel.value) {
      badge.value = t('theme.disabled')
    } else if (sObj && typeof sObj === 'object') {
      if ('Applied' in sObj) {
        badge.value = `${t('theme.applied')}: ${sObj.Applied.theme_id}`
        if (selectedId.value && sObj.Applied.theme_id !== selectedId.value) await reapplyOrPending()
      } else if ('Failed' in sObj) {
        if (selectedId.value) await reapplyOrPending()
        else badge.value = ''
      } else {
        badge.value = ''
      }
    } else if (sObj === 'Disabled') {
      if (selectedId.value) await reapplyOrPending()
      else badge.value = t('theme.disabled')
    }
  } catch {
    badge.value = ''
  }
}

// 开关 = 持久化偏好的状态标记(MOC-102):先落盘(唯一真失败),再 best-effort 即时注入。
watch(enabledModel, async (on) => {
  if (internalSet) {
    internalSet = false
    return
  }
  if (on) {
    if (!selectedId.value) {
      toast(t('theme.pickFirst'))
      setEnabled(false)
      return
    }
    try {
      await store.save({ codexUiThemeEnabled: true, codexUiTheme: selectedId.value })
    } catch (e) {
      setEnabled(false)
      toast(`${t('theme.saveFailed')}: ${errMsg(e)}`, 'error')
      return
    }
    try {
      await themeApply(selectedId.value)
      toast(t('theme.appliedToast'))
    } catch {
      await promptRestart()
    }
  } else {
    try {
      await store.save({ codexUiThemeEnabled: false })
      toast(t('theme.disabledPendingRestart'))
    } catch (e) {
      setEnabled(true)
      toast(`${t('theme.saveFailed')}: ${errMsg(e)}`, 'error')
      return
    }
  }
  await refreshBadge()
})

// 点卡片选主题(__themePickHandler):toggle 开则落盘+即时注入,关则仅持久化选择。
async function pickTheme(id: string) {
  if (enabledModel.value) {
    try {
      await store.save({ codexUiThemeEnabled: true, codexUiTheme: id })
    } catch (e) {
      toast(`${t('theme.saveFailed')}: ${errMsg(e)}`, 'error')
      await refreshBadge()
      return
    }
    try {
      await themeApply(id)
      toast(t('theme.appliedToast'))
    } catch {
      await promptRestart()
    }
  } else {
    try {
      await store.save({ codexUiTheme: id })
    } catch (e) {
      toast(`${t('theme.saveFailed')}: ${errMsg(e)}`, 'error')
    }
  }
  await refreshBadge()
}

// 删除 / 隐藏(__themeDeleteHandler):内置=隐藏(themeHiddenIds),custom=真删 disk。
async function onDelete(themeId: string, isCustom: boolean) {
  const confirmMsg = isCustom
    ? t('theme.deleteCustomConfirm')
    : tFmt('theme.hideConfirm', { id: themeId })
  if (!window.confirm(confirmMsg)) return
  // 共用 fallback 选择器:优先 visible 内置;全隐藏极端 case 挑首个内置并自动 unhide。
  const pickFallback = (hiddenList: string[]): { id: string; unhide: string | null } => {
    const visible = themes.value.find(
      (th) => th.id !== 'custom' && th.id !== themeId && !hiddenList.includes(th.id),
    )
    if (visible) return { id: visible.id, unhide: null }
    const anyBuiltin = themes.value.find((th) => th.id !== 'custom' && th.id !== themeId)
    const id = anyBuiltin ? anyBuiltin.id : 'carton'
    return { id, unhide: id }
  }
  const applyFallback = async (id: string) => {
    try {
      await themeApply(id)
    } catch (e) {
      toast(`${t('theme.applyFailed')}: ${errMsg(e)} — ${t('theme.restartToSeeEffect')}`, 'error')
    }
  }
  try {
    const cur = store.settings
    const curSelected = typeof cur.codexUiTheme === 'string' ? cur.codexUiTheme : undefined
    const curEnabled = cur.codexUiThemeEnabled === true
    const curHidden = Array.isArray(cur.themeHiddenIds) ? (cur.themeHiddenIds as string[]).slice() : []
    if (isCustom) {
      await themeDeleteCustom()
      if (curSelected === 'custom') {
        const fb = pickFallback(curHidden)
        const patch: Settings = { codexUiTheme: fb.id }
        if (fb.unhide) patch.themeHiddenIds = curHidden.filter((id) => id !== fb.unhide)
        await store.save(patch)
        if (curEnabled) await applyFallback(fb.id)
      }
    } else {
      if (!curHidden.includes(themeId)) curHidden.push(themeId)
      const patch: Settings = { themeHiddenIds: curHidden }
      let fallbackToApply: string | null = null
      if (curSelected === themeId) {
        const fb = pickFallback(curHidden)
        patch.codexUiTheme = fb.id
        if (fb.unhide) patch.themeHiddenIds = curHidden.filter((id) => id !== fb.unhide)
        if (curEnabled) fallbackToApply = fb.id
      }
      // 先落盘(持久化是唯一真失败,见上方原则),再 best-effort 注入 fallback —
      // 与 custom 分支一致,避免「已 apply 但 save 抛错」的 CDP/settings desync。
      await store.save(patch)
      if (fallbackToApply) await applyFallback(fallbackToApply)
    }
    await loadThemes()
    await refreshBadge()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}

// 上传 / 替换:file picker → readAsDataURL → 打开 1:1 crop 弹窗。
function openUpload() {
  const input = document.createElement('input')
  input.type = 'file'
  input.accept = 'image/jpeg,image/png,image/jpg'
  input.style.display = 'none'
  document.body.appendChild(input)
  input.addEventListener('change', async () => {
    const file = input.files?.[0]
    input.remove()
    if (!file) return
    if (file.size > 20 * 1024 * 1024) {
      toast(t('theme.uploadTooLarge'), 'error')
      return
    }
    try {
      cropSrc.value = await readFileAsDataUrl(file)
    } catch (e) {
      toast(`${t('theme.uploadFailed')}: ${errMsg(e)}`, 'error')
    }
  })
  input.click()
}
function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader()
    r.onload = () => resolve(r.result as string)
    r.onerror = () => reject(r.error)
    r.readAsDataURL(file)
  })
}
async function onCropConfirm(dataUri: string) {
  cropSrc.value = null
  try {
    await themeUploadCustom(dataUri)
    toast(t('theme.uploadOk'))
    await loadThemes()
    await pickTheme('custom')
  } catch (e) {
    toast(`${t('theme.uploadFailed')}: ${errMsg(e)}`, 'error')
  }
}

async function onRestoreHidden() {
  try {
    await store.save({ themeHiddenIds: [] })
    await loadThemes()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}

async function restartCodex() {
  try {
    await restartCodexApp()
    toast(t('theme.restartToast'))
  } catch (e) {
    toast(`${t('theme.restartFailed')}: ${errMsg(e)}`, 'error')
  }
}

// 即时注入失败时弹双按钮窗让 user 自己选立即/稍后重启,绝不自动重启。
let restartResolve: (() => void) | null = null
function promptRestart(): Promise<void> {
  return new Promise<void>((resolve) => {
    showRestart.value = true
    restartResolve = resolve
  })
}
async function onRestartChoice(choice: 'now' | 'later') {
  showRestart.value = false
  if (choice === 'now') {
    await restartCodex()
  } else {
    toast(t('theme.savedPendingRestartToast'))
  }
  restartResolve?.()
  restartResolve = null
}
</script>

<template>
  <div class="skin-page">
    <header class="page-head">
      <RouterLink to="/settings" class="back-link">
        <IconChevronLeft class="back-icon" />
        {{ t('common.back') }}
      </RouterLink>
      <h1 class="page-title">{{ t('theme.title') }}</h1>
      <p class="page-sub">{{ t('theme.subtitle') }}</p>
    </header>

    <SettingsGroup>
      <SettingsRow :title="t('theme.toggleLabel')" :description="t('theme.toggleHint')">
        <div class="toggle-wrap">
          <span v-if="badge" class="status-badge">{{ badge }}</span>
          <AppSwitch v-model="enabledModel" />
        </div>
      </SettingsRow>
    </SettingsGroup>

    <section class="theme-section">
      <div class="theme-bar">
        <AppButton
          variant="secondary"
          size="sm"
          :icon="IconRefreshCw"
          :label="t('theme.restartCodexBtn')"
          @click="restartCodex"
        />
        <div v-if="hiddenCount > 0" class="hidden-restore">
          <span class="hidden-badge">{{ tFmt('theme.hiddenCount', { count: hiddenCount }) }}</span>
          <AppButton variant="ghost" size="sm" :label="t('theme.restoreHidden')" @click="onRestoreHidden" />
        </div>
      </div>

      <div class="theme-grid">
        <div
          v-for="th in visibleThemes"
          :key="th.id"
          class="theme-card"
          :class="{ 'theme-card--sel': th.id === selectedId }"
          @click="pickTheme(th.id)"
        >
          <span v-if="th.id === selectedId" class="card-check">✓</span>
          <span
            v-if="th.id === 'custom'"
            class="card-replace"
            :title="t('theme.replaceImage')"
            @click.stop="openUpload"
            >{{ t('theme.replace') }}</span
          >
          <span
            class="card-del"
            :title="th.id === 'custom' ? t('common.delete') : t('theme.hide')"
            @click.stop="onDelete(th.id, th.id === 'custom')"
            >×</span
          >
          <img class="card-img" :src="th.previewDataUri" :alt="displayName(th)" />
          <div class="card-name">{{ displayName(th) }}</div>
        </div>

        <div v-if="!hasCustomVisible" class="theme-card theme-card--add" @click="openUpload">
          <div class="add-icon"><IconPlus /></div>
          <div class="card-name card-name--muted">{{ t('theme.addCustom') }}</div>
        </div>
      </div>
    </section>

    <ThemeCropModal v-if="cropSrc" :src="cropSrc" @confirm="onCropConfirm" @cancel="cropSrc = null" />

    <AppModal v-if="showRestart" :title="t('theme.savedTitle')" @close="onRestartChoice('later')">
      <p class="restart-body">{{ t('theme.savedPendingRestart') }}</p>
      <div class="restart-actions">
        <AppButton variant="secondary" :label="t('theme.restartLater')" @click="onRestartChoice('later')" />
        <AppButton variant="primary" :label="t('theme.restartNow')" @click="onRestartChoice('now')" />
      </div>
    </AppModal>
  </div>
</template>

<style scoped>
.skin-page {
  max-width: 760px;
  margin: 0 auto;
}
.page-head {
  margin-bottom: var(--space-5);
}
.back-link {
  display: inline-flex;
  align-items: center;
  gap: 2px;
  font-size: var(--fs-sm);
  color: var(--text-secondary);
  text-decoration: none;
  margin-bottom: var(--space-2);
}
.back-link:hover {
  color: var(--accent);
}
.back-icon {
  width: 15px;
  height: 15px;
}
.page-title {
  font-size: var(--fs-xl);
  font-weight: 600;
  margin: 0 0 var(--space-1);
}
.page-sub {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  line-height: 1.5;
  margin: 0;
}
.toggle-wrap {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.status-badge {
  font-size: var(--fs-sm);
  color: var(--text-secondary);
  white-space: nowrap;
}
.theme-section {
  margin-top: var(--space-5);
}
.theme-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: var(--space-3);
}
.hidden-restore {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.hidden-badge {
  font-size: var(--fs-sm);
  color: var(--text-muted);
}
.theme-grid {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: var(--space-3);
}
@media (max-width: 640px) {
  .theme-grid {
    grid-template-columns: repeat(2, 1fr);
  }
}
.theme-card {
  position: relative;
  display: flex;
  flex-direction: column;
  border: 1px solid var(--border);
  border-radius: var(--radius);
  overflow: hidden;
  cursor: pointer;
  background: var(--surface);
  user-select: none;
  transition: border-color var(--transition), box-shadow var(--transition);
}
.theme-card:hover {
  border-color: var(--border-strong);
}
.theme-card--sel {
  border: 2px solid var(--accent);
  box-shadow: 0 0 0 3px var(--accent-soft);
}
.theme-card--add {
  border-style: dashed;
}
.card-img {
  width: 100%;
  aspect-ratio: 16 / 9;
  object-fit: cover;
  display: block;
  pointer-events: none;
  background: var(--surface-2);
}
.card-name {
  padding: var(--space-2);
  text-align: center;
  font-size: var(--fs-sm);
  font-weight: 600;
}
.card-name--muted {
  color: var(--text-muted);
}
.add-icon {
  width: 100%;
  aspect-ratio: 16 / 9;
  display: flex;
  align-items: center;
  justify-content: center;
  color: var(--text-muted);
  background: var(--surface-2);
}
.add-icon svg {
  width: 30px;
  height: 30px;
}
.card-check {
  position: absolute;
  top: 6px;
  left: 8px;
  z-index: 2;
  background: var(--accent);
  color: var(--accent-text);
  font-size: 11px;
  padding: 1px 7px;
  border-radius: var(--radius-sm);
  pointer-events: none;
}
.card-replace {
  position: absolute;
  top: 6px;
  right: 34px;
  z-index: 3;
  background: rgba(0, 0, 0, 0.55);
  color: #fff;
  font-size: 11px;
  padding: 1px 7px;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.card-del {
  position: absolute;
  top: 6px;
  right: 8px;
  z-index: 3;
  width: 20px;
  height: 20px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.55);
  color: #fff;
  font-size: 13px;
  line-height: 1;
  border-radius: var(--radius-full);
  cursor: pointer;
}
.restart-body {
  font-size: var(--fs-base);
  color: var(--text-secondary);
  line-height: 1.6;
  margin: 0 0 var(--space-4);
}
.restart-actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--space-2);
}
</style>
