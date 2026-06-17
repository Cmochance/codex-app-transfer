<script setup lang="ts">
// Codex 配置快照状态 + 还原 — 移植旧 refreshCodexSnapshotStatus;还原动作复用
// useCodexRestore(与 Desktop 页 clear-desktop 同一入口 chooseCodexRestoreTarget)。
import { onMounted, ref } from 'vue'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { useCodexRestore } from '@/composables/useCodexRestore'
import { getDesktopSnapshotStatus } from '@/api/desktop'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppButton from '@/components/ui/AppButton.vue'

const { show: toast } = useToast()
const { restoreCodexConfig } = useCodexRestore()
const statusText = ref('')

function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}

onMounted(refreshStatus)

async function refreshStatus() {
  try {
    const s = await getDesktopSnapshotStatus()
    if (s?.hasSnapshot) {
      statusText.value = tFmt('settings.codexSnapshotStatusActive', { time: s.snapshotAt || '' })
    } else if (s && s.restorableCount > 0) {
      statusText.value = tFmt('settings.codexSnapshotStatusRecovery', { count: s.restorableCount })
    } else {
      statusText.value = t('settings.codexSnapshotStatusEmpty')
    }
  } catch {
    statusText.value = t('settings.codexSnapshotStatusEmpty')
  }
}

async function onRestore() {
  try {
    if (await restoreCodexConfig()) await refreshStatus()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}
</script>

<template>
  <SettingsGroup :title="t('settings.codexSnapshotTitle')">
    <SettingsRow :description="statusText">
      <template #title>
        <span class="snap-title">{{ t('desktop.clear') }}</span>
      </template>
      <AppButton size="sm" variant="secondary" :label="t('desktop.clear')" @click="onRestore" />
    </SettingsRow>
  </SettingsGroup>
</template>

<style scoped>
.snap-title {
  font-size: var(--fs-md);
  font-weight: 550;
}
</style>
