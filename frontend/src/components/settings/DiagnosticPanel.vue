<script setup lang="ts">
// 诊断模式 · 协议流量查看器(MOC-185,session 级)— 移植旧 traceViewerEnabled toggle +
// open-trace-viewer。**不读持久 settings**,init 查查看器真实运行态;不持久化开关。
import { onMounted, ref, watch } from 'vue'
import { t, tFmt } from '@/i18n'
import { useToast } from '@/composables/useToast'
import { traceViewerStatus, traceViewerStart, traceViewerStop, openTraceViewer } from '@/api/desktop'
import SettingsRow from '@/components/ui/SettingsRow.vue'
import AppButton from '@/components/ui/AppButton.vue'
import AppSwitch from '@/components/ui/AppSwitch.vue'

const { show: toast } = useToast()
const showOpenBtn = ref(false)

// 开关本地镜像 + 守卫(同 CodexSkin):区分「程序回写」与「用户操作」。
const enabledModel = ref(false)
let internalSet = false
function setEnabled(v: boolean) {
  // 值不变则不 arm 守卫(否则 watch 不触发、标志残留会吞掉下一次用户操作)
  if (enabledModel.value === v) return
  internalSet = true
  enabledModel.value = v
}

function errMsg(e: unknown): string {
  return (e as Error)?.message || String(e)
}

onMounted(async () => {
  try {
    const running = (await traceViewerStatus())?.running === true
    setEnabled(running)
    showOpenBtn.value = running
  } catch {
    /* status 查询失败:保守置关 */
  }
})

// 纯运行时起/停查看器服务(不持久化)。快速 on→off 竞争:await 后若 enabledModel 已被
// 后续操作改写则放弃本次 toast(stale handler);被放弃的 start 若已成功则补发 stop,
// 确保 viewer 不在用户关闭后残留捕获。启动失败回滚开关。
watch(enabledModel, async (on) => {
  if (internalSet) {
    internalSet = false
    return
  }
  showOpenBtn.value = on
  const requested = on
  try {
    if (on) {
      const r = await traceViewerStart()
      if (enabledModel.value !== requested) {
        // on→off 竞态:start 在途时用户已切回 off。若后端这次 start 成功了, 必须补发
        // stop —— 否则(尤其 stop 先于 start 完成的排序)viewer 残留运行, 诊断会在用户
        // 关闭后继续捕获敏感流量。best-effort 补停, 确保运行态收敛到「关」。
        try {
          await traceViewerStop()
        } catch {
          /* 已停 / 未真正起来: 忽略 */
        }
        return
      }
      toast(r?.url ? tFmt('settings.traceViewerStartedAt', { url: r.url }) : t('settings.traceViewerStarted'))
    } else {
      await traceViewerStop()
      if (enabledModel.value !== requested) return
      toast(t('settings.traceViewerStopped'))
    }
  } catch (e) {
    if (on) {
      setEnabled(false)
      showOpenBtn.value = false
      const msg = errMsg(e)
      toast(t('settings.traceViewerStartFailed') + (msg ? `: ${msg}` : ''), 'error')
    } else {
      toast(t('settings.traceViewerStopFailed'), 'error')
    }
  }
})

async function onOpen() {
  try {
    const r = await openTraceViewer()
    if (r?.success === false) toast(t('settings.traceViewerOpenFailed'), 'error')
  } catch {
    toast(t('settings.traceViewerOpenFailed'), 'error')
  }
}
</script>

<template>
  <SettingsRow :title="t('settings.traceViewerEnabled')" :description="t('settings.traceViewerEnabledHint')">
      <div class="diag-control">
        <AppButton
          v-if="showOpenBtn"
          size="sm"
          variant="secondary"
          :label="t('settings.openTraceViewer')"
          @click="onOpen"
        />
        <AppSwitch v-model="enabledModel" />
      </div>
  </SettingsRow>
</template>

<style scoped>
.diag-control {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
</style>
