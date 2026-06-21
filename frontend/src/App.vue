<script setup lang="ts">
import { onMounted } from 'vue'
import AppLayout from './layout/AppLayout.vue'
import McpRecoveryModal from '@/components/settings/McpRecoveryModal.vue'
import ConfirmDialog from '@/components/ui/ConfirmDialog.vue'
import { useSettingsStore } from '@/stores/settings'
import { useAppearance } from '@/composables/useAppearance'
import { useFont } from '@/composables/useFont'
import { useMcpRecovery } from '@/composables/useMcpRecovery'
import { useSessionImport } from '@/composables/useSessionImport'
import { setLocale } from '@/i18n'

// 字体偏好(localStorage)启动即应用 — 顶层调用触发模块级 applyFamily/applySize
useFont()

// 启动后从后端 /api/settings hydrate(权威源,覆盖 main.ts 的 localStorage 初值,跨设备一致)。
// load(false) 应用主题不回写、setLocale 仅本地不 PUT → 无 echo 回环。
const settings = useSettingsStore()
onMounted(async () => {
  const s = await settings.load().catch(() => null)
  if (s) {
    if (typeof s.theme === 'string') useAppearance().load(s.theme)
    if (s.language === 'zh' || s.language === 'en') setLocale(s.language as 'zh' | 'en')
  }
  // MOC-261 一-4:MCP 凭据「丢失恢复」—— 启动轮询状态,有待处理(未忽略)项则自动弹窗。
  // 轮询比一次性 startup event 可靠(避免 listener 注册前 emit 丢失);独立于 settings 加载。
  const mcp = useMcpRecovery()
  await mcp.refresh()
  if (mcp.pending.value > 0) mcp.openModal()

  // CAT-255:启动检测其他工具(cc-switch 等)留下的隔离会话(第三方 model_provider),
  // 有则弹窗问是否导入(确认即关 Codex→归一→重启)。放在最后,避免导入重启 Codex 跟
  // 上面的 hydrate 抢节奏。
  const si = useSessionImport()
  const foreign = await si.detect()
  if (foreign > 0) await si.promptImport(foreign)
})
</script>

<template>
  <AppLayout />
  <McpRecoveryModal />
  <ConfirmDialog />
</template>
