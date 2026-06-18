<script setup lang="ts">
// Codex 原配置完整性自检(#268 反投毒)— 移植旧 refreshResidualScanStatus /
// formatResidualPreview / handleRepairResidual / handleShowResidualFields。
import { onMounted, ref } from 'vue'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import {
  scanResidualPollution,
  repairResidualPollution,
  type ResidualScanReport,
  type PollutedFile,
} from '@/api/desktop'
import AppButton from '@/components/ui/AppButton.vue'

const { show: toast } = useToast()

const statusText = ref('')
const statusClass = ref('')
const showRepair = ref(false)
const preview = ref('')

function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}

onMounted(refreshStatus)

async function refreshStatus(): Promise<ResidualScanReport | null> {
  statusClass.value = ''
  statusText.value = t('settings.residualScanStatusUnknown')
  showRepair.value = false
  preview.value = ''
  let report: ResidualScanReport
  try {
    report = await scanResidualPollution()
  } catch (e) {
    statusText.value = tFmt('settings.residualScanStatusError', { error: errMsg(e) })
    return null
  }
  const count = (report.polluted || []).length
  if (count === 0) {
    statusClass.value = 'is-clean'
    statusText.value = report.transferCurrentlyApplied
      ? t('settings.residualScanStatusCleanWhileApplied')
      : t('settings.residualScanStatusClean')
    return report
  }
  statusClass.value = 'is-dirty'
  statusText.value = tFmt('settings.residualScanStatusDirty', { count })
  showRepair.value = true
  return report
}

function formatPreview(polluted: PollutedFile[]): string {
  const lines: string[] = []
  for (const file of polluted) {
    const kindLabel =
      file.kind === 'liveConfig'
        ? '~/.codex/config.toml'
        : file.kind === 'activeSnapshot'
          ? 'active snapshot'
          : file.kind === 'recoverySnapshot'
            ? 'recovery snapshot'
            : file.kind
    lines.push(`[${kindLabel}] ${file.path}`)
    for (const key of file.fieldsToStrip || []) lines.push(`  - ${key}`)
  }
  return lines.join('\n')
}

// 针对性清除:重扫拿最新 → 预览 → confirm → repair(dryRun:false)→ toast。
async function onRepair() {
  let scan: ResidualScanReport
  try {
    scan = await scanResidualPollution()
  } catch (e) {
    toast(tFmt('settings.residualScanStatusError', { error: errMsg(e) }), 'error')
    return
  }
  if (!scan.polluted?.length) {
    await refreshStatus()
    return
  }
  const pv = formatPreview(scan.polluted)
  preview.value = `${t('settings.residualScanPreviewTitle')}\n\n${pv}`
  if (!window.confirm(tFmt('settings.residualScanConfirm', { preview: pv }))) return
  try {
    const result = await repairResidualPollution(false)
    const cleaned = (result?.repair?.repaired || []).length
    toast(tFmt('settings.residualScanToastCleaned', { count: cleaned }))
  } catch (e) {
    toast(tFmt('settings.residualScanStatusError', { error: errMsg(e) }), 'error')
  } finally {
    await refreshStatus()
  }
}

// 只读查看:复用 scan 结果列残留字段,不弹 confirm、不写盘。
async function onShowFields() {
  const report = await refreshStatus()
  if (!report) return
  if (!report.polluted?.length) {
    preview.value = t('settings.residualScanShowFieldsClean')
    return
  }
  preview.value = `${t('settings.residualScanPreviewTitle')}\n\n${formatPreview(report.polluted)}`
}
</script>

<template>
  <div class="panel">
    <div class="panel-head">{{ t('settings.residualScanTitle') }}</div>
    <p class="hint">{{ t('settings.residualScanHint') }}</p>
      <p class="status" :class="statusClass">{{ statusText }}</p>
      <div class="actions">
        <AppButton size="sm" variant="ghost" :label="t('settings.residualScanRefresh')" @click="refreshStatus" />
        <AppButton size="sm" variant="ghost" :label="t('settings.residualScanShowFields')" @click="onShowFields" />
        <AppButton v-if="showRepair" size="sm" variant="danger" :label="t('settings.residualScanRepair')" @click="onRepair" />
      </div>
      <pre v-if="preview" class="preview">{{ preview }}</pre>
  </div>
</template>

<style scoped>
.panel {
  padding: var(--space-4);
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.panel-head {
  font-size: var(--fs-md);
  font-weight: 550;
  color: var(--text);
}
.hint {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  line-height: 1.5;
  margin: 0;
}
.status {
  font-size: var(--fs-sm);
  line-height: 1.5;
  margin: 0;
  color: var(--text-secondary);
}
.status.is-clean {
  color: var(--success, #30a46c);
}
.status.is-dirty {
  color: var(--warning, #e0a020);
}
.actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.preview {
  margin: 0;
  padding: var(--space-3);
  background: var(--surface-2);
  border-radius: var(--radius);
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-all;
  max-height: 240px;
  overflow-y: auto;
}
</style>
