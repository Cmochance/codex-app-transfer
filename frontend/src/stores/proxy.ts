import { defineStore } from 'pinia'
import { ref } from 'vue'
import * as proxyApi from '@/api/proxy'
import type { ProxyLogEntry, ProxyStats } from '@/api/proxy'

export const useProxyStore = defineStore('proxy', () => {
  const running = ref(false)
  const port = ref(0)
  const stats = ref<ProxyStats>({ total: 0, success: 0, failed: 0, today: 0 })
  const logs = ref<ProxyLogEntry[]>([])

  async function loadStatus() {
    const s = await proxyApi.getProxyStatus()
    running.value = !!s.running
    port.value = s.port || 18080
    stats.value = s.stats || { total: 0, success: 0, failed: 0, today: 0 }
  }
  async function toggle(on: boolean, startPort?: number) {
    if (on) await proxyApi.startProxy(startPort)
    else await proxyApi.stopProxy()
    await loadStatus()
  }
  async function loadLogs() {
    logs.value = await proxyApi.getProxyLogs()
  }
  async function clearLogs() {
    await proxyApi.clearProxyLogs()
    logs.value = []
  }
  async function openLogDir() {
    await proxyApi.openProxyLogDir()
  }
  return { running, port, stats, logs, loadStatus, toggle, loadLogs, clearLogs, openLogDir }
})
