import { defineStore } from 'pinia'
import { ref } from 'vue'
import type { Provider } from '@/api/types'
import * as providersApi from '@/api/providers'
import type { CodexRoutingMode } from '@/api/providers'

export const useProvidersStore = defineStore('providers', () => {
  const list = ref<Provider[]>([])
  const routingMode = ref<CodexRoutingMode>('official')
  const loading = ref(false)
  const error = ref('')

  async function load() {
    loading.value = true
    error.value = ''
    try {
      const result = await providersApi.getProviders()
      list.value = result.providers
      routingMode.value = result.routingMode
    } catch (e) {
      error.value = (e as Error).message || '加载失败'
    } finally {
      loading.value = false
    }
  }

  async function setDefault(id: string) {
    await providersApi.setDefaultProvider(id)
    await load()
  }

  async function activateOfficial() {
    await providersApi.activateOfficialCodex()
    await load()
  }

  async function remove(id: string) {
    await providersApi.deleteProvider(id)
    await load()
  }

  // 拖拽排序: 乐观更新本地顺序 + 后端持久化(复用 /api/providers/reorder)
  async function reorder(ids: string[]) {
    const byId = new Map(list.value.map((p) => [p.id, p]))
    list.value = ids.map((id) => byId.get(id)!).filter(Boolean)
    try {
      await providersApi.reorderProviders(ids)
    } catch {
      await load() // 失败回滚到后端真实顺序
    }
  }

  return { list, routingMode, loading, error, load, setDefault, activateOfficial, remove, reorder }
})
