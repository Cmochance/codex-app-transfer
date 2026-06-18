<script setup lang="ts">
// Codex 配置快照状态 + 还原 — 移植旧 refreshCodexSnapshotStatus;还原动作复用
// useCodexRestore(与 Desktop 页 clear-desktop 同一入口 chooseCodexRestoreTarget)。
import { computed, onMounted, ref } from 'vue'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { useCodexRestore } from '@/composables/useCodexRestore'
import { getDesktopSnapshotStatus, openCodexConfigDir } from '@/api/desktop'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppButton from '@/components/ui/AppButton.vue'

const { show: toast } = useToast()
const { restoreCodexConfig } = useCodexRestore()
// 存原始数据,statusText 用 computed 派生 → 随 locale 切换实时更新(修「中文界面残留旧 locale 英文」)
const snap = ref<Awaited<ReturnType<typeof getDesktopSnapshotStatus>> | null>(null)
const failed = ref(false)

const statusText = computed(() => {
  const s = snap.value
  if (failed.value || !s) return t('settings.codexSnapshotStatusEmpty')
  if (s.hasSnapshot) return tFmt('settings.codexSnapshotStatusActive', { time: s.snapshotAt || '' })
  if (s.restorableCount > 0) return tFmt('settings.codexSnapshotStatusRecovery', { count: s.restorableCount })
  return t('settings.codexSnapshotStatusEmpty')
})

function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}

onMounted(refreshStatus)
async function refreshStatus() {
  try {
    snap.value = await getDesktopSnapshotStatus()
    failed.value = false
  } catch {
    failed.value = true
  }
}

async function onRestore() {
  try {
    if (await restoreCodexConfig()) await refreshStatus()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}

async function onOpenFolder() {
  try {
    await openCodexConfigDir()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}
</script>

<template>
  <SettingsRow :description="statusText">
    <template #title>
      <span class="snap-title">{{ t('settings.codexSnapshotTitle') }}</span>
    </template>
    <div class="snap-actions">
      <AppButton size="sm" variant="ghost" :label="t('settings.openConfigFolder')" @click="onOpenFolder" />
      <AppButton size="sm" variant="secondary" :label="t('desktop.clear')" @click="onRestore" />
    </div>
  </SettingsRow>
</template>

<style scoped>
.snap-title {
  font-size: var(--fs-md);
  font-weight: 550;
}
.snap-actions {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
</style>
