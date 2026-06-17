import { defineStore } from 'pinia'
import { ref } from 'vue'
import * as proxyApi from '@/api/proxy'
import type { ProxyLogEntry } from '@/api/proxy'

export const useProxyStore = defineStore('proxy', () => {
  const running = ref(false)
  const port = ref(0)
  const logs = ref<ProxyLogEntry[]>([])

  async function loadStatus() {
    const s = await proxyApi.getProxyStatus()
    running.value = !!s.running
    port.value = (s.port as number) || 0
  }
  async function toggle(on: boolean) {
    if (on) await proxyApi.startProxy()
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
  return { running, port, logs, loadStatus, toggle, loadLogs, clearLogs }
})
