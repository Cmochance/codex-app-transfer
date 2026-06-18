<script setup lang="ts">
// Codex CLI 接管配置页 — 移植旧 app.js renderDesktop + apply-desktop / clear-desktop action。
// 状态(已配置/需重应用/未配置)+ 配置详情 + 环境变量命令块 + 复制命令 / 还原原配置。
import { computed, onMounted, ref } from 'vue'
import { t } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { useCodexRestore } from '@/composables/useCodexRestore'
import { getDesktopStatus, configureDesktop, type DesktopStatus } from '@/api/desktop'
import SettingsGroup from '@/components/ui/SettingsGroup.vue'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppButton from '@/components/ui/AppButton.vue'
import IconChevronLeft from '~icons/lucide/chevron-left'
import IconCopy from '~icons/lucide/copy'
import IconRotateCcw from '~icons/lucide/rotate-ccw'

const status = ref<DesktopStatus | null>(null)
const busy = ref(false)
const { show: toast } = useToast()
const { restoreCodexConfig } = useCodexRestore()

const configEntries = computed(() =>
  status.value ? Object.entries(status.value.config) : ([] as [string, string][]),
)
const envBlock = computed(() => (status.value ? JSON.stringify(status.value.config, null, 2) : ''))

function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}

onMounted(load)
async function load() {
  try {
    status.value = await getDesktopStatus()
  } catch (e) {
    toast(errMsg(e), 'error')
  }
}

async function onApply() {
  busy.value = true
  try {
    const result = await configureDesktop()
    // configure 是真操作;剪贴板复制只是便利,失败不应掩盖配置成功(故不进外层 catch)。
    if (result?.commands?.temporary) {
      await navigator.clipboard.writeText(result.commands.temporary).catch(() => {})
    }
    toast(t('toast.desktopApplied'))
    await load()
  } catch (e) {
    toast(errMsg(e), 'error')
  } finally {
    busy.value = false
  }
}

async function onClear() {
  busy.value = true
  try {
    if (await restoreCodexConfig()) await load()
  } catch (e) {
    toast(errMsg(e), 'error')
  } finally {
    busy.value = false
  }
}
</script>

<template>
  <div class="desktop-page">
    <header class="page-head">
      <RouterLink to="/settings" class="back-link">
        <IconChevronLeft class="back-icon" />
        {{ t('common.back') }}
      </RouterLink>
    </header>

    <SettingsGroup :title="t('desktop.configTitle')">
      <div class="config-list">
        <div v-for="[key, value] in configEntries" :key="key" class="config-row">
          <span class="config-key">{{ key }}</span>
          <code class="config-val">{{ value }}</code>
        </div>
      </div>
      <pre class="env-block">{{ envBlock }}</pre>
    </SettingsGroup>

    <div class="desktop-actions">
      <AppButton
        variant="primary"
        :icon="IconCopy"
        :label="t('desktop.apply')"
        :disabled="busy"
        @click="onApply"
      />
      <AppButton
        variant="secondary"
        :icon="IconRotateCcw"
        :label="t('desktop.clear')"
        :disabled="busy"
        @click="onClear"
      />
    </div>
  </div>
</template>

<style scoped>
.desktop-page {
  /* 填满容器(左右 20px 边由 AppLayout 统一控制) */
  max-width: 100%;
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
.status-dot {
  width: 10px;
  height: 10px;
  border-radius: var(--radius-full);
  background: var(--warning, #e0a020);
}
.status-dot--ok {
  background: var(--success, #30a46c);
}
.health-warn {
  font-size: var(--fs-sm);
  color: var(--warning, #e0a020);
  font-weight: 500;
}
.config-list {
  padding: var(--space-3) var(--space-4) 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.config-row {
  display: flex;
  gap: var(--space-2);
  align-items: baseline;
  font-size: var(--fs-sm);
}
.config-key {
  color: var(--text-secondary);
  flex-shrink: 0;
}
.config-val {
  font-family: var(--font-mono);
  color: var(--text);
  word-break: break-all;
}
.env-block {
  margin: var(--space-3) var(--space-4) var(--space-4);
  padding: var(--space-3);
  background: var(--surface-2);
  border-radius: var(--radius);
  font-family: var(--font-mono);
  font-size: var(--fs-sm);
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-all;
  overflow-x: auto;
}
.desktop-actions {
  display: flex;
  gap: var(--space-3);
  margin-bottom: var(--space-4);
}
.explain {
  font-size: var(--fs-sm);
  color: var(--text-muted);
  line-height: 1.6;
  margin: 0;
}
</style>
