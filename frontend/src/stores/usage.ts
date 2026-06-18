import { defineStore } from 'pinia'
import { ref } from 'vue'
import * as usageApi from '@/api/usage'
import type { UsageReport, UsageView } from '@/api/usage'

export const useUsageStore = defineStore('usage', () => {
  const report = ref<UsageReport | null>(null)
  const activeView = ref<UsageView>('conversation')
  const loading = ref(false)
  const error = ref('')

  // 常规进页/切 view 命中后端 60s cache;forceRefresh 带 nocache 冷扫最新。
  // 失败不写空 report(避免误显示 "0 用量"),置 error 让 UI 显错误条。
  async function load(forceRefresh = false) {
    if (report.value && !forceRefresh) return
    loading.value = true
    error.value = ''
    try {
      report.value = await usageApi.getUsageSummary(forceRefresh)
    } catch (e) {
      error.value = (e as Error).message || '加载失败'
    } finally {
      loading.value = false
    }
  }

  function setView(v: UsageView) {
    activeView.value = v
  }

  return { report, activeView, loading, error, load, setView }
})
