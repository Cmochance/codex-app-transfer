import { ref } from 'vue'
import {
  getMcpRecoveryStatus,
  restoreMcpCredentials,
  removeMcpCredentials,
  ignoreMcpCredentials,
  type McpRecoveryItem,
} from '@/api/desktop'

// MOC-261 一-4:模块级单例 —— 全 app 共享 MCP 凭据「丢失恢复」状态 + 弹窗开关。
// 启动(App.vue)轮询 refresh:pending>0 自动弹窗;设置页入口手动开。操作后端即时生效,
// refresh 刷新列表;未处理 / 已忽略项由后端持久化,下次启动继续提示(不静默丢备份)。
const entries = ref<McpRecoveryItem[]>([])
const pending = ref(0)
const open = ref(false)
const busy = ref(false)

async function refresh(): Promise<void> {
  try {
    const s = await getMcpRecoveryStatus()
    entries.value = s.entries
    pending.value = s.pending
  } catch {
    /* 状态查询失败静默(不阻断启动 / 设置页) */
  }
}

export function useMcpRecovery() {
  function openModal() {
    open.value = true
  }
  function closeModal() {
    open.value = false
  }
  async function restore(keys: string[]): Promise<number> {
    busy.value = true
    try {
      const r = await restoreMcpCredentials(keys)
      await refresh()
      return r.restored ?? 0
    } finally {
      busy.value = false
    }
  }
  async function remove(keys: string[]): Promise<number> {
    busy.value = true
    try {
      const r = await removeMcpCredentials(keys)
      await refresh()
      return r.removed ?? 0
    } finally {
      busy.value = false
    }
  }
  async function ignore(keys: string[]): Promise<number> {
    busy.value = true
    try {
      const r = await ignoreMcpCredentials(keys)
      await refresh()
      return r.ignored ?? 0
    } finally {
      busy.value = false
    }
  }
  return { entries, pending, open, busy, refresh, openModal, closeModal, restore, remove, ignore }
}
